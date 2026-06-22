# syntax=docker/dockerfile:1

# ---- cargo-chef base: shared toolchain + chef ----
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

# ---- plan: capture the dependency graph for caching ----
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- build: cook deps (cached layer), then build the binary ----
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin telegram-yt-dlp-rust

# ---- runtime: slim image with ffmpeg + yt-dlp + deno ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ffmpeg ca-certificates curl unzip \
    # Standalone (PyInstaller) build — no system Python needed, and `yt-dlp -U` self-updates it.
    && curl -fsSL https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux \
        -o /usr/local/bin/yt-dlp \
    && chmod a+rx /usr/local/bin/yt-dlp \
    # Deno: the JS runtime yt-dlp uses to solve YouTube's player (EJS) challenges.
    && curl -fsSL https://github.com/denoland/deno/releases/latest/download/deno-x86_64-unknown-linux-gnu.zip \
        -o /tmp/deno.zip \
    && unzip -q /tmp/deno.zip -d /usr/local/bin \
    && chmod a+rx /usr/local/bin/deno \
    && rm -f /tmp/deno.zip \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/telegram-yt-dlp-rust /usr/local/bin/bot

RUN useradd -m -u 10001 bot && mkdir -p /downloads && chown bot:bot /downloads
USER bot
ENV DOWNLOAD_DIR=/downloads
ENTRYPOINT ["bot"]
