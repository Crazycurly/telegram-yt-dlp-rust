use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use crate::error::BotError;

/// Minimal metadata used to build the info card and the streaming `send_video` call.
#[derive(Debug, Clone, Default)]
pub struct VideoMeta {
    pub title: String,
    pub uploader: Option<String>,
    pub duration: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Fetch metadata with `yt-dlp -J` (no download), bounded by a 30 s timeout.
pub async fn fetch(yt_dlp_bin: &str, url: &str) -> Result<VideoMeta, BotError> {
    let fut = Command::new(yt_dlp_bin)
        .args(["-J", "--no-playlist", "--no-warnings", url])
        .output();

    let out = tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .map_err(|_| BotError::Timeout)?
        .map_err(BotError::Io)?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(classify_stderr(&stderr));
    }

    let v: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| BotError::Metadata(e.to_string()))?;
    Ok(parse_meta(&v))
}

pub fn parse_meta(v: &Value) -> VideoMeta {
    VideoMeta {
        title: v
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("video")
            .to_string(),
        uploader: v
            .get("uploader")
            .or_else(|| v.get("channel"))
            .and_then(Value::as_str)
            .map(String::from),
        duration: v.get("duration").and_then(Value::as_f64).map(|d| d as u32),
        width: v.get("width").and_then(Value::as_u64).map(|x| x as u32),
        height: v.get("height").and_then(Value::as_u64).map(|x| x as u32),
    }
}

/// Best-effort classification of yt-dlp stderr into a friendly error.
pub fn classify_stderr(stderr: &str) -> BotError {
    let s = stderr.to_lowercase();
    if s.contains("private video")
        || s.contains("sign in to confirm your age")
        || s.contains("confirm your age")
        || s.contains("not available in your country")
        || s.contains("video unavailable")
        || s.contains("members-only")
    {
        BotError::Unavailable
    } else {
        BotError::DownloadFailed(tail(stderr, 300))
    }
}

/// Last `n` characters of trimmed text, char-boundary safe.
pub fn tail(s: &str, n: usize) -> String {
    let t = s.trim();
    let chars: Vec<char> = t.chars().collect();
    if chars.len() <= n {
        t.to_string()
    } else {
        format!("…{}", chars[chars.len() - n..].iter().collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metadata_json() {
        let v: Value = serde_json::from_str(
            r#"{"title":"My Clip","channel":"Someone","duration":83.0,"width":1920,"height":1080}"#,
        )
        .unwrap();
        let m = parse_meta(&v);
        assert_eq!(m.title, "My Clip");
        assert_eq!(m.uploader.as_deref(), Some("Someone"));
        assert_eq!(m.duration, Some(83));
        assert_eq!(m.width, Some(1920));
        assert_eq!(m.height, Some(1080));
    }

    #[test]
    fn classifies_private() {
        assert!(matches!(
            classify_stderr("ERROR: Private video. Sign in if you've been granted access"),
            BotError::Unavailable
        ));
        assert!(matches!(
            classify_stderr("ERROR: some random failure"),
            BotError::DownloadFailed(_)
        ));
    }
}
