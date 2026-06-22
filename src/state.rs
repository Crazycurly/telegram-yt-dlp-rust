use std::collections::HashMap;
use std::sync::Arc;

use teloxide::types::ChatId;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::downloader::VideoMeta;

/// A URL + its fetched metadata, parked between "user sent a link" and "user
/// tapped a format button". Keyed by chat, because callback data can't hold a URL.
#[derive(Clone)]
pub struct PendingJob {
    pub url: String,
    pub meta: VideoMeta,
}

/// Shared application state injected into every handler.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    /// Links awaiting a format choice, per chat.
    pub pending: Arc<Mutex<HashMap<ChatId, PendingJob>>>,
    /// In-flight downloads, per chat, each with a cancel handle.
    pub active: Arc<Mutex<HashMap<ChatId, CancellationToken>>>,
    /// Global cap on concurrent downloads across all chats.
    pub sem: Arc<Semaphore>,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        let sem = Arc::new(Semaphore::new(cfg.max_concurrent));
        Self {
            cfg: Arc::new(cfg),
            pending: Arc::new(Mutex::new(HashMap::new())),
            active: Arc::new(Mutex::new(HashMap::new())),
            sem,
        }
    }
}
