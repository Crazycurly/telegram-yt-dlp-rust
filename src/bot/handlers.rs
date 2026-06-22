use std::path::Path;
use std::time::{Duration, Instant};

use teloxide::prelude::*;
use teloxide::types::{ChatId, InputFile, MessageId, ParseMode};
use tokio_util::sync::CancellationToken;

use crate::bot::menu::{self, FormatChoice};
use crate::downloader::{
    self,
    progress::{self, ProgressState},
    VideoMeta,
};
use crate::error::BotError;
use crate::state::{AppState, PendingJob};

pub type HandlerResult = anyhow::Result<()>;

/// Handle a plain message: validate a URL, fetch info, show the format menu.
pub async fn message_handler(bot: Bot, msg: Message, state: AppState) -> HandlerResult {
    let chat = msg.chat.id;

    let allowed = msg
        .from
        .as_ref()
        .map(|u| state.cfg.is_allowed(u.id))
        .unwrap_or(false);
    if !allowed {
        bot.send_message(chat, "⛔ You are not authorized to use this bot.")
            .await?;
        return Ok(());
    }

    let text = msg.text().unwrap_or("").trim().to_string();
    if text.is_empty() || text.starts_with("/start") || text.starts_with("/help") {
        bot.send_message(chat, help_text())
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    let url = match parse_url(&text) {
        Some(u) => u,
        None => {
            bot.send_message(
                chat,
                "🔗 Send me a video link and I'll download it. Type /help for details.",
            )
            .await?;
            return Ok(());
        }
    };

    if state.active.lock().await.contains_key(&chat) {
        bot.send_message(
            chat,
            "⏳ A download is already running in this chat. Please wait or press Cancel.",
        )
        .await?;
        return Ok(());
    }

    let status = bot.send_message(chat, "🔎 Fetching video info…").await?;
    match downloader::fetch_metadata(&state.cfg.yt_dlp_bin, &url).await {
        Ok(meta) => {
            let info = render_info(&meta);
            bot.edit_message_text(chat, status.id, info)
                .parse_mode(ParseMode::Html)
                .reply_markup(menu::format_menu())
                .await?;
            state.pending.lock().await.insert(chat, PendingJob { url, meta });
        }
        Err(e) => {
            bot.edit_message_text(chat, status.id, format!("❌ {}", user_error(&e)))
                .await?;
        }
    }
    Ok(())
}

/// Handle a button tap: cancel, or run the selected download.
pub async fn callback_handler(bot: Bot, q: CallbackQuery, state: AppState) -> HandlerResult {
    let data = q.data.clone().unwrap_or_default();

    let (chat, menu_msg): (ChatId, MessageId) = match q.message.as_ref() {
        Some(m) => (m.chat().id, m.id()),
        None => {
            bot.answer_callback_query(q.id).await?;
            return Ok(());
        }
    };

    if !state.cfg.is_allowed(q.from.id) {
        bot.answer_callback_query(q.id)
            .text("Not authorized")
            .show_alert(true)
            .await?;
        return Ok(());
    }

    // Cancel button: signal the running job, if any.
    if data == menu::CB_CANCEL {
        if let Some(tok) = state.active.lock().await.get(&chat) {
            tok.cancel();
        }
        bot.answer_callback_query(q.id).text("Cancelling…").await?;
        return Ok(());
    }

    let choice = match FormatChoice::from_callback(&data) {
        Some(c) => c,
        None => {
            bot.answer_callback_query(q.id).await?;
            return Ok(());
        }
    };

    if state.active.lock().await.contains_key(&chat) {
        bot.answer_callback_query(q.id)
            .text("A download is already running.")
            .await?;
        return Ok(());
    }

    // Atomically claim the pending job (guards against double taps).
    let job = match state.pending.lock().await.remove(&chat) {
        Some(j) => j,
        None => {
            bot.answer_callback_query(q.id)
                .text("Session expired — please resend the link.")
                .show_alert(true)
                .await?;
            return Ok(());
        }
    };

    bot.answer_callback_query(q.id).await?;

    let cancel = CancellationToken::new();
    state.active.lock().await.insert(chat, cancel.clone());
    // Bound total concurrent downloads across all chats.
    let _permit = state.sem.clone().acquire_owned().await.ok();

    let result = run_job(&bot, &state, chat, menu_msg, &job, choice, cancel).await;

    state.active.lock().await.remove(&chat);
    drop(_permit);

    match result {
        Ok(()) => {}
        Err(BotError::Cancelled) => {
            let _ = bot.edit_message_text(chat, menu_msg, "✖ Cancelled.").await;
        }
        Err(e) => {
            let _ = bot
                .edit_message_text(chat, menu_msg, format!("❌ {}", user_error(&e)))
                .await;
        }
    }
    Ok(())
}

/// Download with a live progress bar, then upload the file.
async fn run_job(
    bot: &Bot,
    state: &AppState,
    chat: ChatId,
    menu_msg: MessageId,
    job: &PendingJob,
    choice: FormatChoice,
    cancel: CancellationToken,
) -> Result<(), BotError> {
    let title_html = html_escape(&job.meta.title);

    let _ = bot
        .edit_message_text(chat, menu_msg, format!("⬇️ <b>{title_html}</b>\nStarting…"))
        .parse_mode(ParseMode::Html)
        .reply_markup(menu::cancel_menu())
        .await;

    let work = downloader::make_work_dir(&state.cfg.download_dir, chat.0)?;

    // Progress pipeline: the download pushes snapshots into a channel; this task
    // throttles them into at most one message edit every 3 s / 5 % change.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ProgressState>(8);
    let bot_cl = bot.clone();
    let title_for_task = title_html.clone();
    let progress_task = tokio::spawn(async move {
        let mut last_edit: Option<Instant> = None;
        let mut last_pct = -100.0f32;
        while let Some(p) = rx.recv().await {
            let now = Instant::now();
            let due = last_edit
                .map(|t| now.duration_since(t) >= Duration::from_secs(3))
                .unwrap_or(true);
            if due && (p.percent - last_pct >= 5.0) {
                last_edit = Some(now);
                last_pct = p.percent;
                let text = progress::render_bar(&title_for_task, &p, 12);
                // Drop the frame on any error (rate-limit, "not modified", etc.).
                let _ = bot_cl
                    .edit_message_text(chat, menu_msg, text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(menu::cancel_menu())
                    .await;
            }
        }
    });

    let dl_result = downloader::run_download(
        &state.cfg.yt_dlp_bin,
        &job.url,
        choice,
        &work.path,
        cancel,
        move |p| {
            let _ = tx.try_send(p);
        },
    )
    .await;

    let _ = progress_task.await;
    let path = dl_result?;

    // Make the mp4 streamable (faststart); audio is sent as-is.
    let final_path = if choice.is_audio() {
        path
    } else {
        downloader::faststart_remux(&path).await
    };

    let size = std::fs::metadata(&final_path).map_err(BotError::Io)?.len();
    if size > state.cfg.max_file_bytes {
        bot.edit_message_text(
            chat,
            menu_msg,
            format!(
                "⚠️ <b>{title_html}</b>\nFile is {} — over the {} limit. Try 🎵 MP3 or a shorter video.",
                human_size(size),
                human_size(state.cfg.max_file_bytes)
            ),
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    bot.edit_message_text(chat, menu_msg, format!("⬆️ <b>{title_html}</b>\nUploading…"))
        .parse_mode(ParseMode::Html)
        .await?;

    if choice.is_audio() {
        send_audio_file(bot, chat, &final_path, &job.meta).await?;
    } else {
        send_video_streamable(bot, chat, &final_path, &job.meta, &title_html).await?;
    }

    bot.edit_message_text(chat, menu_msg, format!("✅ <b>{title_html}</b>\nSent!"))
        .parse_mode(ParseMode::Html)
        .await?;

    Ok(())
}

/// Send a streamable video, retrying on Telegram rate limits.
async fn send_video_streamable(
    bot: &Bot,
    chat: ChatId,
    path: &Path,
    meta: &VideoMeta,
    caption_html: &str,
) -> Result<(), BotError> {
    let mut attempt = 0;
    loop {
        let mut req = bot
            .send_video(chat, InputFile::file(path))
            .supports_streaming(true)
            .caption(caption_html.to_string())
            .parse_mode(ParseMode::Html);
        if let Some(w) = meta.width {
            req = req.width(w);
        }
        if let Some(h) = meta.height {
            req = req.height(h);
        }
        if let Some(d) = meta.duration {
            req = req.duration(d);
        }
        match req.await {
            Ok(_) => return Ok(()),
            Err(teloxide::RequestError::RetryAfter(s)) if attempt < 3 => {
                attempt += 1;
                tokio::time::sleep(s.duration()).await;
            }
            Err(e) => return Err(BotError::Telegram(e)),
        }
    }
}

/// Send an audio file, retrying on Telegram rate limits.
async fn send_audio_file(
    bot: &Bot,
    chat: ChatId,
    path: &Path,
    meta: &VideoMeta,
) -> Result<(), BotError> {
    let mut attempt = 0;
    loop {
        let mut req = bot
            .send_audio(chat, InputFile::file(path))
            .title(meta.title.clone());
        if let Some(p) = &meta.uploader {
            req = req.performer(p.clone());
        }
        if let Some(d) = meta.duration {
            req = req.duration(d);
        }
        match req.await {
            Ok(_) => return Ok(()),
            Err(teloxide::RequestError::RetryAfter(s)) if attempt < 3 => {
                attempt += 1;
                tokio::time::sleep(s.duration()).await;
            }
            Err(e) => return Err(BotError::Telegram(e)),
        }
    }
}

// ---- presentation helpers -------------------------------------------------

fn help_text() -> String {
    "<b>🎬 Video Downloader</b>\n\n\
     Send me a link (YouTube and many other sites) and pick a format:\n\
     • <b>Best video (MP4)</b> — streamable, highest quality\n\
     • <b>MP3 audio</b> — audio only\n\n\
     You'll get a live progress bar while it downloads."
        .to_string()
}

fn render_info(meta: &VideoMeta) -> String {
    let mut s = format!("🎬 <b>{}</b>\n", html_escape(&meta.title));
    if let Some(up) = &meta.uploader {
        s.push_str(&format!("👤 {}\n", html_escape(up)));
    }
    if let Some(d) = meta.duration {
        s.push_str(&format!("⏱ {}\n", fmt_duration(d)));
    }
    if let (Some(w), Some(h)) = (meta.width, meta.height) {
        s.push_str(&format!("📐 {w}×{h}\n"));
    }
    s.push_str("\nChoose a format:");
    s
}

pub fn user_error(e: &BotError) -> String {
    match e {
        BotError::Unavailable => {
            "This video is unavailable (private, age-restricted, or geo-blocked).".to_string()
        }
        BotError::Timeout => "The source took too long to respond. Try again.".to_string(),
        BotError::DownloadFailed(m) => format!("Download failed.\n{m}"),
        BotError::Metadata(_) => "Couldn't read this video's info.".to_string(),
        BotError::NoOutput => "No file was produced.".to_string(),
        BotError::Cancelled => "Cancelled.".to_string(),
        BotError::Telegram(_) => "Telegram error while sending. Try again.".to_string(),
        BotError::Io(_) => "A local error occurred.".to_string(),
    }
}

fn parse_url(text: &str) -> Option<String> {
    let t = text.trim();
    let u = url::Url::parse(t).ok()?;
    if matches!(u.scheme(), "http" | "https") && u.host().is_some() {
        Some(t.to_string())
    } else {
        None
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn fmt_duration(secs: u32) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut b = bytes as f64;
    let mut i = 0;
    while b >= 1024.0 && i < UNITS.len() - 1 {
        b /= 1024.0;
        i += 1;
    }
    format!("{b:.1} {}", UNITS[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_validation() {
        assert!(parse_url("https://youtu.be/abc").is_some());
        assert!(parse_url("http://example.com/v").is_some());
        assert!(parse_url("just some text").is_none());
        assert!(parse_url("ftp://x/y").is_none());
    }

    #[test]
    fn escaping_and_formatting() {
        assert_eq!(html_escape("a<b>&c"), "a&lt;b&gt;&amp;c");
        assert_eq!(fmt_duration(83), "1:23");
        assert_eq!(fmt_duration(3661), "1:01:01");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }
}
