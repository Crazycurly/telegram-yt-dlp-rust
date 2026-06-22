use thiserror::Error;

/// Typed errors for the download pipeline. Handlers map these to friendly,
/// user-facing messages via [`crate::bot::handlers::user_error`].
#[derive(Error, Debug)]
pub enum BotError {
    #[error("the video is unavailable (private, age-restricted, or geo-blocked)")]
    Unavailable,

    #[error("download failed: {0}")]
    DownloadFailed(String),

    #[error("could not read video info: {0}")]
    Metadata(String),

    #[error("the source took too long to respond")]
    Timeout,

    #[error("cancelled")]
    Cancelled,

    #[error("no output file was produced")]
    NoOutput,

    #[error(transparent)]
    Telegram(#[from] teloxide::RequestError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}
