mod bot;
mod config;
mod downloader;
mod error;
mod state;
mod updater;

use teloxide::prelude::*;
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let cfg = Config::from_env()?;

    if cfg.allowed_users.is_empty() {
        tracing::warn!(
            "ALLOWED_USER_IDS is empty — the bot will reject ALL users. \
             Set it to your numeric Telegram user id(s)."
        );
    }
    if let Err(e) = std::fs::create_dir_all(&cfg.download_dir) {
        tracing::warn!("could not create download dir {:?}: {e}", cfg.download_dir);
    }

    // Keep yt-dlp current: update now, then on a schedule.
    updater::update_once(&cfg.yt_dlp_bin).await;
    match updater::version(&cfg.yt_dlp_bin).await {
        Ok(v) => tracing::info!("yt-dlp version: {v}"),
        Err(e) => tracing::warn!("yt-dlp not runnable ({e}); downloads will fail until it's available"),
    }
    updater::spawn_periodic(cfg.yt_dlp_bin.clone(), cfg.update_interval_hours);

    // Build the bot, optionally pointing at a self-hosted Bot API server (2 GB uploads).
    let mut bot = Bot::new(cfg.bot_token.clone());
    if let Some(api) = &cfg.api_url {
        let url = url::Url::parse(api)
            .map_err(|e| anyhow::anyhow!("invalid TELEGRAM_API_URL `{api}`: {e}"))?;
        bot = bot.set_api_url(url);
        tracing::info!("using custom Telegram API server: {api}");
    }

    let state = AppState::new(cfg);

    tracing::info!("bot starting…");
    Dispatcher::builder(bot, bot::schema())
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}
