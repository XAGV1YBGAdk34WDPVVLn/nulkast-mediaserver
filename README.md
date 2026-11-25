# NulKast MediaServer

A lean, zero-bloat Rust media server built for Linux hosts (Intel Quick Sync / VA-API / AMD VA-API), RAM-backed HLS streaming (`/dev/shm`), SQLite WAL + FTS5 instant search, official movie/TV posters & genres, and a 10-foot Xbox / Desktop / Mobile Web UI.

**~2.2 MiB RAM at idle.** Zero telemetry, zero cloud accounts, zero paywalls.

## Features
- **Zero-Copy First Streaming:** HTTP 206 Range Direct Play → Zero-copy video remux (`-c:v copy` + AAC) → Hardware VA-API (`h264_vaapi` with GPU `scale_vaapi=format=nv12` for 10-bit HEVC) → CPU fallback.
- **RAM-Backed HLS:** Segments write directly to `/dev/shm/nulkast-hls` (zero SSD wear, full movie scrub bar with server-side `-ss` seek, 15-minute idle reaper, and automatic client recovery).
- **Zero Subtitle Burn-In:** Embedded text subtitles (SRT/ASS/WebVTT) extract on the fly to client-side WebVTT.
- **Official Jellyfin Xbox App Compatible:** Implements UDP `7359` LAN auto-discovery, `/System/Info/Public` handshake (with double-slash normalization), and `/web/index.html` so the official Jellyfin Xbox Retail Store app (WebView2) loads our custom 10-foot UI fullscreen with zero Xbox Dev Mode.
- **Xbox Wireless Controller Support:** Full D-pad & stick navigation, stick timeline scrubbing with preview bubble + `A` jump, `LB`/`RB` (`±10s`/`30s`), `LT`/`RT` (`±60s`), `X` Play/Pause, `B` Back, and `⏮ Restart`.
- **Official Posters & Genres (Zero API Key):** Local `poster.jpg`/`folder.jpg` → optional TMDB (`TMDB_API_KEY`) → TVMaze + IMDb + Wikidata SPARQL, cached locally on disk.
- **Toggleable Deep Diagnostics:** Live `Diag: ON/OFF` HUD and `/api/diagnostics` endpoint (captures FFmpeg `stderr`, active RAM HLS segment counts, and client playback events).

## Quick Start (Docker on Linux)
```bash
cp .env.example .env
# Find your host GPU group IDs for VA-API hardware acceleration:
getent group video render
# Edit .env: set HOST_MEDIA_DIR, PUBLIC_URL, VIDEO_GID, and RENDER_GID
docker compose up -d --build
```

Then open `http://<your-server-ip>:8275` in any browser, or open the **Jellyfin** app on your Xbox (it will auto-discover **NulKast MediaServer** on the LAN).

## Configuration
All settings can be configured via `.env` (see [`.env.example`](.env.example)) or `config.toml` (see [`config.example.toml`](config.example.toml)).

## Control CLI
```bash
python3 scripts/mediaserver_ctl.py test
python3 scripts/mediaserver_ctl.py deploy
python3 scripts/mediaserver_ctl.py health
python3 scripts/mediaserver_ctl.py scan
python3 scripts/mediaserver_ctl.py bench-vaapi
```
