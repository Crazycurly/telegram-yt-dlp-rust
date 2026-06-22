use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;

use teloxide::types::UserId;

/// Runtime configuration, loaded once from environment variables at startup.
#[derive(Debug, Clone)]
pub struct Config {
    pub bot_token: String,
    /// Custom Bot API base URL (e.g. a self-hosted server for 2 GB uploads).
    pub api_url: Option<String>,
    pub download_dir: PathBuf,
    pub max_file_bytes: u64,
    /// Allowlist of Telegram user IDs. Empty = nobody is allowed (deny-all).
    pub allowed_users: HashSet<UserId>,
    pub yt_dlp_bin: String,
    pub update_interval_hours: u64,
    pub max_concurrent: usize,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let bot_token = std::env::var("BOT_TOKEN")
            .or_else(|_| std::env::var("TELOXIDE_TOKEN"))
            .map_err(|_| anyhow::anyhow!("BOT_TOKEN (or TELOXIDE_TOKEN) must be set"))?;

        let api_url = std::env::var("TELEGRAM_API_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());

        let download_dir = std::env::var("DOWNLOAD_DIR")
            .unwrap_or_else(|_| "/downloads".to_string())
            .into();

        let max_file_mb: u64 = parse_env("MAX_FILE_MB", 2000);
        let max_file_bytes = max_file_mb.saturating_mul(1024 * 1024);

        let allowed_users = parse_allowed_users();
        let yt_dlp_bin = std::env::var("YTDLP_BIN").unwrap_or_else(|_| "yt-dlp".to_string());
        let update_interval_hours = parse_env("UPDATE_INTERVAL_HOURS", 12u64).max(1);
        let max_concurrent = parse_env("MAX_CONCURRENT_DOWNLOADS", 3usize).max(1);

        Ok(Self {
            bot_token,
            api_url,
            download_dir,
            max_file_bytes,
            allowed_users,
            yt_dlp_bin,
            update_interval_hours,
            max_concurrent,
        })
    }

    /// A user is allowed only if they appear in a non-empty allowlist.
    pub fn is_allowed(&self, user: UserId) -> bool {
        !self.allowed_users.is_empty() && self.allowed_users.contains(&user)
    }
}

fn parse_env<T: FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<T>().ok())
        .unwrap_or(default)
}

fn parse_allowed_users() -> HashSet<UserId> {
    std::env::var("ALLOWED_USER_IDS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse::<u64>().ok())
        .map(UserId)
        .collect()
}
