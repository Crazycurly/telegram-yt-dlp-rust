# telegram-yt-dlp-rust

A self-hosted Telegram bot (Rust + [teloxide](https://github.com/teloxide/teloxide)) that
downloads videos with [yt-dlp](https://github.com/yt-dlp/yt-dlp). It shows an inline format
menu, a **live progress bar**, can convert to **MP3**, sends **streamable** video, and keeps
yt-dlp up to date automatically. Ships with Docker Compose, including an optional local Bot API
server for **2 GB** uploads.

## Features

- 🔗 Paste a link → bot fetches **title / uploader / duration**, then shows a menu.
- 🎬 **Best video (MP4)** — picks H.264/AAC and remuxes with faststart so Telegram streams it inline.
- 🎵 **MP3 audio** — extracts audio at best quality (via ffmpeg).
- 📊 **Live progress bar** edited in place, throttled to respect Telegram rate limits, with a **Cancel** button.
- 🔐 **Allowlist** — only configured Telegram user IDs may use the bot.
- ♻️ **Auto-update** — yt-dlp self-updates on startup and on a schedule.
- 📦 **2 GB uploads** via a bundled self-hosted Bot API server (50 MB on the cloud API).

## Quick start (Docker Compose)

1. **Create a bot** with [@BotFather](https://t.me/BotFather) and copy the token.
2. **Get API credentials** at <https://my.telegram.org> → *API development tools* (`api_id` + `api_hash`) — needed for 2 GB uploads.
3. **Find your user ID** with [@userinfobot](https://t.me/userinfobot).
4. Configure env:
   ```bash
   cp .env.example .env
   # edit .env: BOT_TOKEN, ALLOWED_USER_IDS, TELEGRAM_API_ID, TELEGRAM_API_HASH
   ```
5. Launch:
   ```bash
   docker compose up --build -d
   docker compose logs -f bot
   ```
6. DM your bot a video link and pick a format.

The bot talks to the `telegram-bot-api` service (`TELEGRAM_API_URL` is set automatically in
`docker-compose.yml`), so uploads up to 2 GB work out of the box. Downloads land in an internal
named volume (`downloads`) — they're ephemeral: the bot uploads each file then removes its
per-job temp dir. The runtime image bundles **ffmpeg**, **yt-dlp**, and **deno** (the JS runtime
yt-dlp needs for YouTube's player challenges).

### Cloud API only (no 2 GB server)

Don't need large files? Remove the `telegram-bot-api` service and the `TELEGRAM_API_URL`
line from `docker-compose.yml`, and set `MAX_FILE_MB=50`. The 50 MB Bot API limit then applies.

## Local development

Requires the Rust toolchain plus `yt-dlp` and `ffmpeg` on `PATH`.

```bash
cp .env.example .env   # leave TELEGRAM_API_URL commented out for the cloud API
export $(grep -v '^#' .env | xargs)   # or use a dotenv-aware shell
cargo run
```

Run the test suite (pure, no network):

```bash
cargo test
```

## Configuration

| Variable | Default | Description |
|---|---|---|
| `BOT_TOKEN` | — | Bot token from @BotFather (required) |
| `ALLOWED_USER_IDS` | *(empty = deny all)* | CSV of numeric Telegram user IDs |
| `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` | — | For the local Bot API server (2 GB) |
| `TELEGRAM_API_URL` | *(cloud API)* | Base URL of the Bot API server |
| `MAX_FILE_MB` | `2000` | Upload size cap (use `50` on the cloud API) |
| `DOWNLOAD_DIR` | `/downloads` | Working/output directory |
| `UPDATE_INTERVAL_HOURS` | `12` | yt-dlp self-update cadence |
| `MAX_CONCURRENT_DOWNLOADS` | `3` | Global concurrency cap |
| `RUST_LOG` | `info` | Log level |

## How it works

```
URL ─▶ message_handler ─▶ yt-dlp -J (metadata) ─▶ info + inline menu
                                                      │ button tap
                                                      ▼
                          callback_handler ─▶ yt-dlp download ──▶ progress channel
                                                      │                 │ throttled edits
                                                      ▼                 ▼
                              faststart remux (video) ─▶ size check ─▶ send_video/send_audio
```

State (pending link + in-flight jobs) lives in an in-memory map keyed by chat; it's
intentionally not persisted across restarts (the bot tells the user to resend the link if a
session expired). yt-dlp is run as a subprocess; its `--progress-template` output is parsed and
rendered as an ASCII bar.

## Notes & limits

- The Telegram **cloud** Bot API caps bot uploads at 50 MB; the bundled local server lifts that to 2 GB.
- Streaming requires H.264/AAC MP4; sources offering only VP9/AV1 may not stream inline without re-encoding.
- yt-dlp version is logged on startup and after each scheduled update.
- **YouTube from a datacenter/VPS IP** may hit *"Sign in to confirm you're not a bot."* If so,
  export browser cookies and pass them to yt-dlp (mount a `cookies.txt` and add `--cookies` in
  `downloader/mod.rs::build_args`). Residential IPs usually don't need this.
