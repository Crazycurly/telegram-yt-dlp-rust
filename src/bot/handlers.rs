use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use teloxide::prelude::*;
use teloxide::types::{ChatId, InputFile, MessageId, ParseMode};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::bot::menu::{self, FormatChoice};
use crate::downloader::{
    self,
    progress::{self, Phase, ProgressEvent, ProgressState},
    VideoMeta,
};
use crate::error::BotError;
use crate::state::{AppState, PendingJob};

pub type HandlerResult = anyhow::Result<()>;

/// How often the progress message is re-rendered. Decoupled from yt-dlp's output
/// rate: fast enough to feel live, slow enough to stay well under Telegram's
/// per-chat edit limits.
const EDIT_INTERVAL: Duration = Duration::from_millis(2000);
/// Number of segments in the `▰`/`▱` progress bars (download and upload share it).
const BAR_WIDTH: usize = 10;

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
            let _ = bot
                .edit_message_text(chat, menu_msg, "✖ Cancelled.")
                .reply_markup(menu::no_menu())
                .await;
        }
        Err(e) => {
            let _ = bot
                .edit_message_text(chat, menu_msg, format!("❌ {}", user_error(&e)))
                .reply_markup(menu::no_menu())
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
        .edit_message_text(chat, menu_msg, "⏳ Starting…")
        .reply_markup(menu::cancel_menu())
        .await;

    let work = downloader::make_work_dir(&state.cfg.download_dir, chat.0)?;

    // Live progress: the download pushes events into a channel; this task keeps
    // only the latest snapshot/phase and re-renders on a fixed heartbeat. That
    // decoupling is what keeps the bar moving during stalls and post-processing
    // (merge / mp3 conversion), where yt-dlp emits no numbers at all.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ProgressEvent>(16);
    let bot_cl = bot.clone();
    let progress_task = tokio::spawn(async move {
        let mut state = ProgressState::default();
        let mut phase = Phase::Preparing;
        let mut frame = 0usize;
        let mut last_text = String::new();

        let mut ticker = tokio::time::interval(EDIT_INTERVAL);
        // If an edit takes longer than the interval, don't fire a burst to catch up.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(ProgressEvent::Update(p)) => {
                        if phase == Phase::Preparing {
                            phase = Phase::Downloading;
                        }
                        state = p;
                    }
                    Some(ProgressEvent::Phase(ph)) => phase = ph,
                    // Channel closed: the download finished (or errored). Stop.
                    None => break,
                },
                _ = ticker.tick() => {
                    let spinner = progress::SPINNER[frame % progress::SPINNER.len()];
                    let text = progress::render(phase, &state, spinner, BAR_WIDTH);
                    // Skip redundant edits; advance the spinner only when we draw.
                    if text != last_text {
                        last_text = text.clone();
                        frame = frame.wrapping_add(1);
                        // Bound the edit so a slow/stuck API call can't freeze the
                        // heartbeat; drop the frame on timeout or error (rate-limit,
                        // "not modified", etc.) and try again on the next tick.
                        let edit = bot_cl
                            .edit_message_text(chat, menu_msg, text)
                            .parse_mode(ParseMode::Html)
                            .reply_markup(menu::cancel_menu());
                        let _ = tokio::time::timeout(EDIT_INTERVAL, async { edit.await }).await;
                    }
                }
            }
        }
    });

    let dl_result = downloader::run_download(
        &state.cfg.yt_dlp_bin,
        &job.url,
        choice,
        &work.path,
        cancel,
        move |ev| {
            let _ = tx.try_send(ev);
        },
    )
    .await;

    let _ = progress_task.await;
    let path = dl_result?;

    // Make the mp4 streamable (faststart); audio is sent as-is.
    let final_path = if choice.is_audio() {
        path
    } else {
        let _ = bot
            .edit_message_text(chat, menu_msg, format!("✨ <b>{title_html}</b>\nFinalizing…"))
            .parse_mode(ParseMode::Html)
            .reply_markup(menu::no_menu())
            .await;
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
        .reply_markup(menu::no_menu())
        .await?;
        return Ok(());
    }

    // Live upload bar: a counting reader (see `upload_input`) bumps `sent` as the
    // HTTP client streams the file; this heartbeat renders it every ~2s. With a
    // self-hosted Bot API server the bot→server leg is fast, so the bar may reach
    // 100% and then animate (spinner/clock) while the server forwards to Telegram.
    let sent = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(Notify::new());
    let up_task = {
        let bot = bot.clone();
        let sent = sent.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut last_text = String::new();
            let mut ticker = tokio::time::interval(EDIT_INTERVAL);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = stop.notified() => break,
                    _ = ticker.tick() => {
                        let text = render_upload(sent.load(Ordering::Relaxed), size, BAR_WIDTH);
                        if text != last_text {
                            last_text = text.clone();
                            let edit = bot
                                .edit_message_text(chat, menu_msg, text)
                                .parse_mode(ParseMode::Html)
                                .reply_markup(menu::no_menu());
                            let _ = tokio::time::timeout(EDIT_INTERVAL, async { edit.await }).await;
                        }
                    }
                }
            }
        })
    };

    let send_result = if choice.is_audio() {
        send_audio_file(bot, chat, &final_path, &job.meta, &sent).await
    } else {
        send_video_streamable(bot, chat, &final_path, &job.meta, &title_html, &sent).await
    };

    // Stop the heartbeat before the terminal edit so it can't overwrite it.
    stop.notify_one();
    let _ = up_task.await;
    send_result?;

    bot.edit_message_text(chat, menu_msg, format!("✅ <b>{title_html}</b>\nSent!"))
        .parse_mode(ParseMode::Html)
        .reply_markup(menu::no_menu())
        .await?;

    Ok(())
}

/// An `AsyncRead` that tallies bytes read into a shared counter, so an in-flight
/// multipart upload can be observed for a progress bar.
struct CountingReader<R> {
    inner: R,
    counter: Arc<AtomicU64>,
}

impl<R: AsyncRead + Unpin> AsyncRead for CountingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &res {
            let n = buf.filled().len() - before;
            self.counter.fetch_add(n as u64, Ordering::Relaxed);
        }
        res
    }
}

/// Build a Telegram upload body that streams `path` while counting bytes into
/// `counter`. Resets the counter to 0 so retries restart the bar from zero.
async fn upload_input(path: &Path, counter: &Arc<AtomicU64>) -> Result<InputFile, BotError> {
    counter.store(0, Ordering::Relaxed);
    let file = tokio::fs::File::open(path).await.map_err(BotError::Io)?;
    let reader = CountingReader { inner: file, counter: counter.clone() };
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    Ok(InputFile::read(reader).file_name(name))
}

/// Send a streamable video, retrying on Telegram rate limits.
async fn send_video_streamable(
    bot: &Bot,
    chat: ChatId,
    path: &Path,
    meta: &VideoMeta,
    caption_html: &str,
    sent: &Arc<AtomicU64>,
) -> Result<(), BotError> {
    let mut attempt = 0;
    loop {
        let mut req = bot
            .send_video(chat, upload_input(path, sent).await?)
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
    sent: &Arc<AtomicU64>,
) -> Result<(), BotError> {
    let mut attempt = 0;
    loop {
        let mut req = bot
            .send_audio(chat, upload_input(path, sent).await?)
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

/// Render the upload progress line, e.g. `🏁 Uploading: ▰▰▰▱▱▱▱▱▱▱ 30% (2.8 MB)`.
/// The parenthesised value is the total file size; `width` is the segment count.
fn render_upload(sent: u64, total: u64, width: usize) -> String {
    let pct = if total > 0 {
        ((sent as f64 / total as f64) * 100.0).min(100.0)
    } else {
        0.0
    };
    let bar = progress::bar(pct as f32, width);
    format!("🏁 Uploading: {bar} {pct:.0}% ({})", human_size_dec(total))
}

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

/// Human size in decimal units (MB = 10⁶ bytes), to match the upload bar style.
fn human_size_dec(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut b = bytes as f64;
    let mut i = 0;
    while b >= 1000.0 && i < UNITS.len() - 1 {
        b /= 1000.0;
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

    #[test]
    fn upload_bar_style() {
        let half = render_upload(512 * 1024, 1024 * 1024, 10);
        assert!(half.starts_with("🏁 Uploading: "));
        assert!(half.contains("50%"));
        assert_eq!(half.matches('▰').count(), 5);
        assert_eq!(half.matches('▱').count(), 5);
        assert!(half.contains("(1.0 MB)"));

        let full = render_upload(2_800_000, 2_800_000, 10);
        assert!(full.contains("100% (2.8 MB)"));
        assert_eq!(full.matches('▰').count(), 10);
        assert_eq!(full.matches('▱').count(), 0);

        // zero total must not panic and reads 0%; over-100 clamps.
        assert!(render_upload(10, 0, 10).contains("0%"));
        assert!(render_upload(999, 100, 10).contains("100%"));
    }
}
