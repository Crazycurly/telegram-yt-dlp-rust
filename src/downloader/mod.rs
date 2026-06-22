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
use progress::ProgressState;

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
        "--progress-template".into(),
        "download:%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s".into(),
        "--print".into(),
        "after_move:filepath".into(),
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
    F: FnMut(ProgressState) + Send,
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
                            on_progress(p);
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
    fn audio_args_extract_mp3() {
        let args = build_args("URL", FormatChoice::AudioMp3, Path::new("/tmp/job"));
        assert!(args.contains(&"-x".to_string()));
        assert!(args.iter().any(|a| a == "mp3"));
    }
}
