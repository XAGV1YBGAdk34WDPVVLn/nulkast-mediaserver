FROM rust:1-trixie AS builder
WORKDIR /app
COPY Cargo.toml ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg \
    intel-media-va-driver \
    libva-drm2 \
    vainfo \
    ca-certificates \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/nulkast-mediaserver /usr/local/bin/nulkast-mediaserver
COPY web ./web

ENV PORT=8275 \
    MEDIA_DIR=/media \
    DATA_DIR=/data \
    TRANSCODE_DIR=/dev/shm/nulkast-hls \
    WEB_DIR=/app/web \
    SERVER_NAME="NulKast MediaServer"

EXPOSE 8275
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -sf http://127.0.0.1:${PORT}/api/health || exit 1

ENTRYPOINT ["/usr/local/bin/nulkast-mediaserver"]
