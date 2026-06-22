pub mod metadata;
pub mod progress;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::bot::menu::FormatChoice;
use crate::error::BotError;
use progress::{Phase, ProgressEvent, PROGRESS_TEMPLATE};

pub use metadata::{fetch as fetch_metadata, VideoMeta};

/// A per-job temporary directory that is removed when dropped, so an early
/// return or panic still cleans up partial downloads.
pub struct WorkDir {
    pub path: PathBuf,
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Create an isolated working directory for one job.
pub fn make_work_dir(base: &Path, chat: i64) -> Result<WorkDir, BotError> {
    let path = base.join(format!("job-{chat}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).map_err(BotError::Io)?;
    Ok(WorkDir { path })
}

/// Build the yt-dlp argument vector for a given choice and working directory.
pub fn build_args(url: &str, choice: FormatChoice, work_dir: &Path) -> Vec<String> {
    let out_tmpl = format!("{}/%(title).80B [%(id)s].%(ext)s", work_dir.display());

    let mut args: Vec<String> = vec![
        "--no-playlist".into(),
        "--newline".into(),
        "--no-color".into(),
        "--no-warnings".into(),
        "--restrict-filenames".into(),
        "-o".into(),
        out_tmpl,
        // `--progress` forces progress output even if some flag implies --quiet.
        // NOTE: we intentionally do NOT use `--print` here: `--print` implies
        // `--quiet`, which silently suppresses the progress template entirely
        // (the long-standing reason the bar never moved). We recover the output
        // path via `resolve_output` (largest media file in the per-job dir).
        "--progress".into(),
        "--progress-template".into(),
        PROGRESS_TEMPLATE.into(),
    ];

    match choice {
        FormatChoice::BestVideo => args.extend([
            // Prefer Telegram-playable codecs (H.264/AAC), then force an mp4 container.
            "-S".into(),
            "res,vcodec:h264,acodec:aac".into(),
            "-f".into(),
            "bv*+ba/b".into(),
            "--merge-output-format".into(),
            "mp4".into(),
        ]),
        FormatChoice::AudioMp3 => args.extend([
            "-x".into(),
            "--audio-format".into(),
            "mp3".into(),
            "--audio-quality".into(),
            "0".into(),
        ]),
    }

    args.push(url.to_string());
    args
}

/// Map a yt-dlp post-processing banner (printed on stdout) to a UI [`Phase`].
/// Returns `None` for lines that aren't a stage marker we care about.
fn detect_phase(line: &str) -> Option<Phase> {
    let l = line.trim_start();
    if l.starts_with("[Merger]") {
        Some(Phase::Merging)
    } else if l.starts_with("[ExtractAudio]") {
        Some(Phase::Converting)
    } else if l.starts_with("[VideoConvertor]")
        || l.starts_with("[VideoRemuxer]")
        || l.starts_with("[Fixup")
        || l.starts_with("[Metadata]")
        || l.starts_with("[ThumbnailsConvertor]")
        || l.starts_with("[EmbedThumbnail]")
    {
        Some(Phase::Finalizing)
    } else {
        None
    }
}

/// Spawn yt-dlp, stream progress to `on_progress`, and return the final file path.
///
/// `on_progress` is a *synchronous* callback (it should be cheap and non-blocking,
/// e.g. push into a channel); the actual Telegram edit happens elsewhere.
pub async fn run_download<F>(
    yt_dlp_bin: &str,
    url: &str,
    choice: FormatChoice,
    work_dir: &Path,
    cancel: CancellationToken,
    mut on_progress: F,
) -> Result<PathBuf, BotError>
where
    F: FnMut(ProgressEvent) + Send,
{
    let args = build_args(url, choice, work_dir);

    let mut child = Command::new(yt_dlp_bin)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(BotError::Io)?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Drain stderr concurrently so the pipe never blocks the child.
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
        }
        buf
    });

    let mut out_lines = BufReader::new(stdout).lines();
    let mut printed_path: Option<PathBuf> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.start_kill();
                let _ = stderr_task.await;
                return Err(BotError::Cancelled);
            }
            line = out_lines.next_line() => {
                match line.map_err(BotError::Io)? {
                    Some(line) => {
                        if let Some(p) = progress::parse_progress_line(&line) {
                            on_progress(ProgressEvent::Update(p));
                        } else if let Some(phase) = detect_phase(&line) {
                            on_progress(ProgressEvent::Phase(phase));
                        } else {
                            let t = line.trim();
                            if !t.is_empty() && Path::new(t).is_absolute() {
                                printed_path = Some(PathBuf::from(t));
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }

    let status = child.wait().await.map_err(BotError::Io)?;
    let stderr_buf = stderr_task.await.unwrap_or_default();

    if !status.success() {
        return Err(metadata::classify_stderr(&stderr_buf));
    }

    resolve_output(printed_path, work_dir)
}

/// Determine the produced file: trust yt-dlp's printed path if it exists, else
/// pick the largest real media file in the (single-job) working directory.
fn resolve_output(printed: Option<PathBuf>, work_dir: &Path) -> Result<PathBuf, BotError> {
    if let Some(p) = printed {
        if p.exists() {
            return Ok(p);
        }
    }

    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(work_dir).map_err(BotError::Io)? {
        let entry = entry.map_err(BotError::Io)?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.ends_with(".part")
            || name.ends_with(".ytdl")
            || name.ends_with(".json")
            || name.ends_with(".webp")
            || name.ends_with(".jpg")
        {
            continue;
        }
        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if best.as_ref().map(|(b, _)| len > *b).unwrap_or(true) {
            best = Some((len, path));
        }
    }

    best.map(|(_, p)| p).ok_or(BotError::NoOutput)
}

/// Remux an mp4 with `-movflags +faststart` so Telegram can stream it inline.
/// This is a fast stream copy (no re-encode); falls back to the input on failure.
pub async fn faststart_remux(input: &Path) -> PathBuf {
    let output = input.with_extension("stream.mp4");
    let status = Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(input)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(&output)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    match status {
        Ok(s) if s.success() && output.exists() => output,
        _ => input.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_args_force_mp4_and_streamable_codecs() {
        let args = build_args("URL", FormatChoice::BestVideo, Path::new("/tmp/job"));
        assert!(args.contains(&"--merge-output-format".to_string()));
        assert!(args.iter().any(|a| a == "mp4"));
        assert!(args.iter().any(|a| a.contains("vcodec:h264")));
        assert!(args.contains(&"--no-playlist".to_string()));
        assert_eq!(args.last().unwrap(), "URL");
    }

    #[test]
    fn progress_is_forced_and_print_is_absent() {
        // `--print` implies `--quiet`, which suppresses the progress template,
        // so it must never be present; `--progress` must be.
        let args = build_args("URL", FormatChoice::BestVideo, Path::new("/tmp/job"));
        assert!(args.contains(&"--progress".to_string()));
        assert!(!args.iter().any(|a| a == "--print"));
        assert!(args.iter().any(|a| a.starts_with("download:PROGRESS:")));
    }

    #[test]
    fn audio_args_extract_mp3() {
        let args = build_args("URL", FormatChoice::AudioMp3, Path::new("/tmp/job"));
        assert!(args.contains(&"-x".to_string()));
        assert!(args.iter().any(|a| a == "mp3"));
    }

    #[test]
    fn detects_postprocess_phases() {
        assert_eq!(detect_phase("[Merger] Merging formats into \"x.mp4\""), Some(Phase::Merging));
        assert_eq!(detect_phase("[ExtractAudio] Destination: x.mp3"), Some(Phase::Converting));
        assert_eq!(detect_phase("[FixupM4a] Correcting container"), Some(Phase::Finalizing));
        // Plain download/info banners are not stage markers.
        assert_eq!(detect_phase("[download] Destination: x.mp4"), None);
        assert_eq!(detect_phase("[youtube] abc: Downloading webpage"), None);
    }
}
