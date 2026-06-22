use std::time::Duration;

use tokio::process::Command;

/// Run a one-shot yt-dlp self-update and log the outcome. Never fails the bot.
pub async fn update_once(bin: &str) {
    match run_update(bin).await {
        Ok(version) => tracing::info!("yt-dlp self-update ok; version: {version}"),
        Err(e) => tracing::warn!("yt-dlp self-update failed (continuing): {e}"),
    }
}

async fn run_update(bin: &str) -> anyhow::Result<String> {
    let out = Command::new(bin).arg("-U").output().await?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    version(bin).await
}

/// Return the yt-dlp version string.
pub async fn version(bin: &str) -> anyhow::Result<String> {
    let out = Command::new(bin).arg("--version").output().await?;
    if !out.status.success() {
        anyhow::bail!("yt-dlp --version exited non-zero");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Spawn a background task that self-updates yt-dlp every `interval_hours`.
pub fn spawn_periodic(bin: String, interval_hours: u64) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_hours.max(1) * 3600));
        // The first tick fires immediately; skip it since startup already updated.
        interval.tick().await;
        loop {
            interval.tick().await;
            update_once(&bin).await;
        }
    });
}
