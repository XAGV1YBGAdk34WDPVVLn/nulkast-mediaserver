mod artwork;
mod config;
mod db;
mod diagnostics;
mod jellyfin_compat;
mod scanner;
mod transcode;

use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router, ServiceExt,
};
use tower::Layer;
use artwork::ArtworkManager;
use db::Db;
use diagnostics::Diagnostics;
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing::{error, info};
use transcode::{StartStreamRequest, TranscodeManager};

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub media_root: PathBuf,
    pub data_dir: PathBuf,
    pub transcoder: TranscodeManager,
    pub artwork: ArtworkManager,
    pub diag: Diagnostics,
    pub server_name: String,
    pub server_id: String,
    pub public_url: String,
    pub web_dir: PathBuf,
    pub scanning: Arc<AtomicBool>,
}

#[derive(Deserialize)]
struct DiagToggle {
    enabled: bool,
}

#[derive(Deserialize)]
struct ClientDiagLog {
    level: Option<String>,
    message: String,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

#[derive(Deserialize)]
struct MoviesQuery {
    sort: Option<String>,
    genre: Option<String>,
}

#[derive(Deserialize)]
struct ProgressUpdate {
    position_secs: f64,
    watched: Option<bool>,
}

async fn api_health(State(state): State<AppState>) -> impl IntoResponse {
    let stats = state.db.stats().unwrap_or(db::LibraryStats {
        movies: 0,
        episodes: 0,
        shows: 0,
        total_size_bytes: 0,
    });
    let active_sessions = state.transcoder.active_session_count().await;
    Json(json!({
        "status": "ok",
        "server_name": state.server_name,
        "hw_vaapi": state.transcoder.vaapi_device.is_some(),
        "diagnostics_enabled": state.diag.is_enabled(),
        "scratch_dir": state.transcoder.scratch_root,
        "active_transcode_sessions": active_sessions,
        "library": stats
    }))
}

async fn api_diagnostics(State(state): State<AppState>) -> impl IntoResponse {
    let sessions = state.transcoder.session_snapshots().await;
    let events = state.diag.snapshot(80);
    Json(json!({
        "enabled": state.diag.is_enabled(),
        "hw_vaapi": state.transcoder.vaapi_device.is_some(),
        "vaapi_device": state.transcoder.vaapi_device,
        "scratch_dir": state.transcoder.scratch_root,
        "active_sessions": sessions,
        "events": events
    }))
}

async fn api_diagnostics_toggle(
    State(state): State<AppState>,
    Json(body): Json<DiagToggle>,
) -> impl IntoResponse {
    state.diag.set_enabled(body.enabled);
    state.diag.record("info", "diagnostics", format!("Diagnostics toggled to {}", body.enabled));
    Json(json!({ "enabled": state.diag.is_enabled() }))
}

async fn api_diagnostics_client_log(
    State(state): State<AppState>,
    Json(body): Json<ClientDiagLog>,
) -> impl IntoResponse {
    let lvl = body.level.as_deref().unwrap_or("info");
    state.diag.record(lvl, "client", body.message);
    Json(json!({ "ok": true }))
}

async fn api_media_poster(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let item = match state.db.get_media(id) {
        Ok(Some(i)) => i,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    if let Ok(bytes) = state.artwork.get_media_poster(&item).await {
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "public, max-age=2592000"),
            ],
            bytes,
        )
            .into_response();
    }
    // Fallback to local video frame thumbnail if offline or obscure file
    api_media_thumb(State(state), Path(id)).await.into_response()
}

async fn api_show_poster(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if let Ok(bytes) = state.artwork.get_show_poster(&name).await {
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "public, max-age=2592000"),
            ],
            bytes,
        )
            .into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

async fn api_media_thumb(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let item = match state.db.get_media(id) {
        Ok(Some(i)) => i,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let thumb_path = state.data_dir.join("thumbs").join(format!("{}.jpg", id));
    if !thumb_path.exists() {
        if let Err(e) = state.transcoder.generate_thumb(&item, &thumb_path).await {
            state.diag.record("warn", "thumb", format!("Thumb failed for {}: {}", id, e));
            return StatusCode::NOT_FOUND.into_response();
        }
    }
    match tokio::fs::read(&thumb_path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                (header::CACHE_CONTROL, "public, max-age=604800"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_movies(
    State(state): State<AppState>,
    Query(q): Query<MoviesQuery>,
) -> impl IntoResponse {
    let sort = q.sort.as_deref().unwrap_or("az");
    match state.db.list_movies_filtered(sort, q.genre.as_deref()) {
        Ok(items) => (StatusCode::OK, Json(json!({ "items": items }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn api_shows(State(state): State<AppState>) -> impl IntoResponse {
    match state.db.list_shows() {
        Ok(shows) => (StatusCode::OK, Json(json!({ "shows": shows }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn api_show_episodes(
    State(state): State<AppState>,
    Path(show_name): Path<String>,
) -> impl IntoResponse {
    match state.db.list_show_episodes(&show_name) {
        Ok(items) => (StatusCode::OK, Json(json!({ "show": show_name, "episodes": items }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn api_continue_watching(State(state): State<AppState>) -> impl IntoResponse {
    match state.db.list_continue_watching(20) {
        Ok(items) => (StatusCode::OK, Json(json!({ "items": items }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn api_search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> impl IntoResponse {
    let query = q.q.unwrap_or_default();
    match state.db.search(&query, 40) {
        Ok(items) => (StatusCode::OK, Json(json!({ "query": query, "items": items }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn api_get_media(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    match state.db.get_media(id) {
        Ok(Some(item)) => (StatusCode::OK, Json(item)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "error": "Not found" }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn api_update_progress(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ProgressUpdate>,
) -> impl IntoResponse {
    match state.db.update_progress(id, body.position_secs, body.watched) {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn api_scan(State(state): State<AppState>) -> impl IntoResponse {
    if state
        .scanning
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (StatusCode::ACCEPTED, Json(json!({ "status": "already_scanning" }))).into_response();
    }

    let db = state.db.clone();
    let root = state.media_root.clone();
    let scanning = state.scanning.clone();
    let artwork = state.artwork.clone();

    // Detached task guarantees `scanning` flag ALWAYS resets even if HTTP client disconnects
    let handle = tokio::spawn(async move {
        let res = tokio::task::spawn_blocking(move || scanner::scan_library(&db, &root)).await;
        scanning.store(false, Ordering::SeqCst);
        res
    });

    match handle.await {
        Ok(Ok(Ok(report))) => {
            // Trigger background genre enrichment for any newly added items
            let enrich_db = state.db.clone();
            tokio::spawn(async move {
                artwork.enrich_missing_movie_genres(&enrich_db).await;
                artwork.enrich_missing_show_genres(&enrich_db).await;
            });
            (StatusCode::OK, Json(report)).into_response()
        }
        Ok(Ok(Err(e))) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn api_start_stream(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<StartStreamRequest>,
) -> impl IntoResponse {
    let item = match state.db.get_media(id) {
        Ok(Some(i)) => i,
        Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({ "error": "Media not found" }))).into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response(),
    };

    match state.transcoder.start_stream(&item, &req).await {
        Ok(resp) => (StatusCode::OK, Json(resp)).into_response(),
        Err(e) => {
            error!("Start stream failed for {}: {}", id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))).into_response()
        }
    }
}

async fn api_stop_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    state.transcoder.stop_session(&session_id).await;
    (StatusCode::OK, Json(json!({ "stopped": session_id })))
}

/// Parses RFC 9110 byte range header: `bytes=start-end`, `bytes=start-`, or `bytes=-suffix`.
fn parse_byte_range(header_val: &str, file_size: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = header_val.strip_prefix("bytes=")?;
    // Use first range if multiple requested
    let first = spec.split(',').next()?.trim();
    let (start_s, end_s) = first.split_once('-')?;
    if file_size == 0 {
        return Some(Err(()));
    }
    if start_s.is_empty() {
        // Suffix range: bytes=-N (last N bytes, used by browsers for MP4 moov atoms)
        let suffix: u64 = end_s.parse().ok()?;
        if suffix == 0 {
            return Some(Err(()));
        }
        let len = suffix.min(file_size);
        Some(Ok((file_size - len, file_size - 1)))
    } else {
        let start: u64 = start_s.parse().ok()?;
        if start >= file_size {
            return Some(Err(()));
        }
        let end = if end_s.is_empty() {
            // Generous 16 MB open-ended chunk for smooth high-bitrate playback
            (start + 16 * 1024 * 1024 - 1).min(file_size - 1)
        } else {
            let parsed_end: u64 = end_s.parse().ok()?;
            if parsed_end < start {
                return Some(Err(()));
            }
            parsed_end.min(file_size - 1)
        };
        Some(Ok((start, end)))
    }
}

/// Full RFC 9110 HTTP Range (206 Partial Content / 416 Range Not Satisfiable) direct byte-streaming.
async fn api_direct_stream(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let item = match state.db.get_media(id) {
        Ok(Some(i)) => i,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let mut file = match File::open(&item.path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let file_size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let content_type = mime_guess::from_path(&item.path)
        .first_or_octet_stream()
        .to_string();

    if let Some(range_val) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        if let Some(parsed) = parse_byte_range(range_val, file_size) {
            match parsed {
                Ok((start, end)) => {
                    let len = end - start + 1;
                    if file.seek(SeekFrom::Start(start)).await.is_ok() {
                        let stream = ReaderStream::with_capacity(file.take(len), 64 * 1024);
                        return (
                            StatusCode::PARTIAL_CONTENT,
                            [
                                (header::CONTENT_TYPE, content_type),
                                (header::ACCEPT_RANGES, "bytes".to_string()),
                                (header::CONTENT_LENGTH, len.to_string()),
                                (header::CONTENT_RANGE, format!("bytes {}-{}/{}", start, end, file_size)),
                            ],
                            Body::from_stream(stream),
                        )
                            .into_response();
                    }
                }
                Err(()) => {
                    return (
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        [(header::CONTENT_RANGE, format!("bytes */{}", file_size))],
                    )
                        .into_response();
                }
            }
        }
    }

    let stream = ReaderStream::with_capacity(file, 64 * 1024);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::ACCEPT_RANGES, "bytes".to_string()),
            (header::CONTENT_LENGTH, file_size.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn api_hls_file(
    State(state): State<AppState>,
    Path((session_id, filename)): Path<(String, String)>,
) -> impl IntoResponse {
    let Some((file_path, running)) = state.transcoder.touch_and_resolve(&session_id, &filename).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Wait briefly only if ffmpeg is actively running and writing the next segment
    if running {
        for _ in 0..25 {
            if tokio::fs::try_exists(&file_path).await.unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    }

    if filename.ends_with(".m3u8") {
        let bytes = match tokio::fs::read(&file_path).await {
            Ok(b) => b,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
                (header::CACHE_CONTROL, "no-cache, no-store"),
            ],
            bytes,
        )
            .into_response();
    }

    // Stream .ts segments directly from /dev/shm with 64 KB chunks (zero heap duplication!)
    let file = match File::open(&file_path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let ctype = if filename.ends_with(".ts") {
        "video/mp2t"
    } else {
        "application/octet-stream"
    };
    let stream = ReaderStream::with_capacity(file, 64 * 1024);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ctype.to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CACHE_CONTROL, "public, max-age=3600, immutable".to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn api_subtitle_vtt(
    State(state): State<AppState>,
    Path((id, track)): Path<(i64, i64)>,
) -> impl IntoResponse {
    let item = match state.db.get_media(id) {
        Ok(Some(i)) => i,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let subs_dir = state.data_dir.join("subs");
    match state
        .transcoder
        .extract_subtitle_cached(id, std::path::Path::new(&item.path), track, &subs_dir)
        .await
    {
        Ok(vtt) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/vtt; charset=utf-8"),
                (header::CACHE_CONTROL, "public, max-age=604800"),
            ],
            vtt,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn normalize_double_slashes(mut req: Request, next: Next) -> impl IntoResponse {
    let path = req.uri().path();
    if path.contains("//") {
        let mut norm = path.to_string();
        while norm.contains("//") {
            norm = norm.replace("//", "/");
        }
        let pq = match req.uri().query() {
            Some(q) => format!("{}?{}", norm, q),
            None => norm,
        };
        if let Ok(new_uri) = pq.parse::<axum::http::Uri>() {
            *req.uri_mut() = new_uri;
        }
    }
    next.run(req).await
}

async fn log_requests(req: Request, next: Next) -> impl IntoResponse {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let resp = next.run(req).await;
    let status = resp.status();
    if status != StatusCode::OK || !uri.path().starts_with("/api/hls/") {
        info!("HTTP {} {} -> {}", method, uri, status.as_u16());
    }
    resp
}

pub fn build_router(state: AppState) -> Router {
    let web_service = ServeDir::new(&state.web_dir).append_index_html_on_directories(true);

    Router::new()
        // Jellyfin Xbox/UWP WebView compatibility handshake
        .route("/System/Info/Public", get(jellyfin_compat::system_info_public))
        .route("/system/info/public", get(jellyfin_compat::system_info_public))
        .route("/emby/System/Info/Public", get(jellyfin_compat::system_info_public))
        .route("/System/Ping", get(jellyfin_compat::system_ping).post(jellyfin_compat::system_ping))
        .route("/system/ping", get(jellyfin_compat::system_ping).post(jellyfin_compat::system_ping))
        .route("/health", get(jellyfin_compat::health_ok))
        .route("/Health", get(jellyfin_compat::health_ok))
        .route("/web/config.json", get(jellyfin_compat::web_config_json))
        .route("/Branding/Configuration", get(jellyfin_compat::branding_configuration))
        // Core MediaServer REST API
        .route("/api/health", get(api_health))
        .route("/api/diagnostics", get(api_diagnostics))
        .route("/api/diagnostics/toggle", post(api_diagnostics_toggle))
        .route("/api/diagnostics/client-log", post(api_diagnostics_client_log))
        .route("/api/movies", get(api_movies))
        .route("/api/shows", get(api_shows))
        .route("/api/shows/{name}/episodes", get(api_show_episodes))
        .route("/api/shows/{name}/poster", get(api_show_poster))
        .route("/api/continue", get(api_continue_watching))
        .route("/api/search", get(api_search))
        .route("/api/scan", post(api_scan))
        .route("/api/media/{id}", get(api_get_media))
        .route("/api/media/{id}/poster", get(api_media_poster))
        .route("/api/media/{id}/thumb", get(api_media_thumb))
        .route("/api/media/{id}/progress", post(api_update_progress))
        .route("/api/stream/{id}/start", post(api_start_stream))
        .route("/api/stream/{id}/direct", get(api_direct_stream))
        .route("/api/stream/{id}/subtitles/{track}", get(api_subtitle_vtt))
        .route("/api/hls/{session_id}/{filename}", get(api_hls_file))
        .route("/api/hls/{session_id}", delete(api_stop_session))
        // Serve our custom 10-foot Web UI at both /web/* (for Jellyfin Xbox app) and /
        .nest_service("/web", web_service.clone())
        .fallback_service(web_service)
        .layer(middleware::from_fn(log_requests))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nulkast_mediaserver=info,tower_http=warn".into()),
        )
        .init();

    let cfg = config::AppConfig::load();
    let diag = Diagnostics::new(cfg.diagnostics_enabled);
    let db = Db::open(cfg.data_dir.join("nulkast-mediaserver.db"))?;
    let transcoder = TranscodeManager::new(
        cfg.transcode_dir.clone(),
        cfg.hw_accel,
        cfg.vaapi_device.clone(),
        cfg.hls_segment_secs,
        diag.clone(),
    );
    let artwork = ArtworkManager::new(
        cfg.data_dir.join("posters"),
        cfg.tmdb_api_key.clone(),
        cfg.online_artwork,
        diag.clone(),
    );

    let state = AppState {
        db: db.clone(),
        media_root: cfg.media_dir.clone(),
        data_dir: cfg.data_dir.clone(),
        transcoder: transcoder.clone(),
        artwork,
        diag,
        server_name: cfg.server_name.clone(),
        server_id: cfg.server_id.clone(),
        public_url: cfg.public_url.clone(),
        web_dir: cfg.web_dir.clone(),
        scanning: Arc::new(AtomicBool::new(false)),
    };

    if cfg.scan_on_startup {
        let scan_db = db.clone();
        let scan_root = cfg.media_dir.clone();
        let scan_flag = state.scanning.clone();
        let enrich_art = state.artwork.clone();
        let enrich_db = db.clone();
        tokio::spawn(async move {
            if scan_flag
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let _ = tokio::task::spawn_blocking(move || {
                    if let Err(e) = scanner::scan_library(&scan_db, &scan_root) {
                        error!("Initial scan error: {}", e);
                    }
                })
                .await;
                scan_flag.store(false, Ordering::SeqCst);
            }
            enrich_art.enrich_missing_movie_genres(&enrich_db).await;
            enrich_art.enrich_missing_show_genres(&enrich_db).await;
        });
    }

    let reaper_tc = transcoder.clone();
    let idle_secs = cfg.idle_timeout_secs.max(60);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            reaper_tc.reap_idle_sessions(Duration::from_secs(idle_secs)).await;
        }
    });

    // Spawn Jellyfin UDP 7359 LAN auto-discovery so Xbox finds us automatically
    let discovery_port: u16 = std::env::var("DISCOVERY_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7359);
    if discovery_port > 0 {
        jellyfin_compat::spawn_udp_discovery(
            discovery_port,
            cfg.public_url.clone(),
            cfg.server_id.clone(),
            cfg.server_name.clone(),
        );
    }

    let app = middleware::from_fn(normalize_double_slashes).layer(build_router(state));
    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse()?;
    info!("{} listening on http://{}", cfg.server_name, addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Optional Jellyfin default-port (8096) alias listener for host-networked deployments
    let compat_port: u16 = std::env::var("JELLYFIN_COMPAT_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if compat_port > 0 && compat_port != cfg.port {
        let compat_addr: SocketAddr = format!("{}:{}", cfg.host, compat_port).parse()?;
        if let Ok(compat_listener) = tokio::net::TcpListener::bind(compat_addr).await {
            let compat_app = app.clone();
            info!("{} Jellyfin-compat listener on http://{}", cfg.server_name, compat_addr);
            tokio::spawn(async move {
                let _ = axum::serve(compat_listener, ServiceExt::<Request>::into_make_service(compat_app)).await;
            });
        }
    }

    let shutdown_tc = transcoder.clone();
    axum::serve(listener, ServiceExt::<Request>::into_make_service(app))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("Shutting down: stopping all active HLS sessions...");
            shutdown_tc.stop_all_sessions().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state(tmp: &tempfile::TempDir) -> AppState {
        let db = Db::open(tmp.path().join("test.db")).unwrap();
        let diag = Diagnostics::new(true);
        let web_dir = tmp.path().join("web");
        std::fs::create_dir_all(&web_dir).unwrap();
        std::fs::write(web_dir.join("index.html"), "<h1>NUC Media</h1>").unwrap();
        AppState {
            db,
            media_root: tmp.path().join("media"),
            data_dir: tmp.path().join("data"),
            transcoder: TranscodeManager::new(tmp.path().join("hls"), false, None, 4, diag.clone()),
            artwork: ArtworkManager::new(tmp.path().join("posters"), None, false, diag.clone()),
            diag,
            server_name: "NUC Test".to_string(),
            server_id: "test-id-01".to_string(),
            public_url: "http://127.0.0.1:8275".to_string(),
            web_dir,
            scanning: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn test_config_toml_loading() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            r#"
server_name = "Custom Living Room"
port = 9090
hls_segment_secs = 6
hw_accel = false
"#,
        )
        .unwrap();
        let cfg = config::AppConfig::load_from(&cfg_path);
        assert_eq!(cfg.server_name, "Custom Living Room");
        assert_eq!(cfg.port, 9090);
        assert_eq!(cfg.hls_segment_secs, 6);
        assert!(!cfg.hw_accel);
    }

    #[test]
    fn test_filename_parsing_movies_and_tv() {
        let root = PathBuf::from("/media");
        let m1 = scanner::parse_media_path(
            &root,
            &PathBuf::from("/media/Movies/28.Days.Later.2002.1080p.BluRay.DDP5.1.x265.10bit-GalaxyRG265.mkv"),
        );
        assert_eq!(m1.kind, "movie");
        assert_eq!(m1.title, "28 Days Later");
        assert_eq!(m1.year, Some(2002));

        let tv1 = scanner::parse_media_path(
            &root,
            &PathBuf::from("/media/TV/Get.Jiro.S01E01.Pilot.1080p.HEVC.x265-MeGusta.mkv"),
        );
        assert_eq!(tv1.kind, "episode");
        assert_eq!(tv1.show_name.as_deref(), Some("Get Jiro"));
        assert_eq!(tv1.season, Some(1));
        assert_eq!(tv1.episode, Some(1));
        assert_eq!(tv1.title, "S01E01 - Pilot");

        let tv2 = scanner::parse_media_path(
            &root,
            &PathBuf::from("/media/TV/Cyberpunk.Edgerunners.S02/Cyberpunk.Edgerunners.S02E05.Fades.1080p.NF.WEB-DL.mkv"),
        );
        assert_eq!(tv2.kind, "episode");
        assert_eq!(tv2.show_name.as_deref(), Some("Cyberpunk Edgerunners"));
        assert_eq!(tv2.season, Some(2));
        assert_eq!(tv2.episode, Some(5));

        // Edition stripping & TV trailing-year normalization
        let akira = scanner::parse_media_path(
            &root,
            &PathBuf::from("/media/Movies/Akira - 30th Anniversary/[Reaktor] Akira - 30th Anniversary [1080p][x265].mkv"),
        );
        assert_eq!(akira.kind, "movie");
        assert_eq!(akira.title, "Akira");

        let lanterns = scanner::parse_media_path(
            &root,
            &PathBuf::from("/media/TV/Lanterns.2026.S01E03.1080p.HEVC.x265-MeGusta.mkv"),
        );
        assert_eq!(lanterns.kind, "episode");
        assert_eq!(lanterns.show_name.as_deref(), Some("Lanterns"));
    }

    #[test]
    fn test_vaapi_codec_guard_and_range_parser() {
        assert!(TranscodeManager::vaapi_decode_supported("hevc"));
        assert!(TranscodeManager::vaapi_decode_supported("h264"));
        assert!(!TranscodeManager::vaapi_decode_supported("mpeg4"));
        assert!(!TranscodeManager::vaapi_decode_supported("vc1"));

        // RFC 9110 range parsing: normal, open-ended, suffix, and OOB
        assert_eq!(parse_byte_range("bytes=0-9", 36), Some(Ok((0, 9))));
        assert_eq!(parse_byte_range("bytes=10-", 36), Some(Ok((10, 35))));
        assert_eq!(parse_byte_range("bytes=-6", 36), Some(Ok((30, 35))));
        assert_eq!(parse_byte_range("bytes=100-200", 36), Some(Err(())));
        assert_eq!(parse_byte_range("bytes=20-10", 36), Some(Err(())));
    }

    #[tokio::test]
    async fn test_orphaned_scratch_cleanup_and_diagnostics_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        let hls_root = tmp.path().join("hls");
        let orphan = hls_root.join("orphaned-session-123");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("seg_00001.ts"), b"stale").unwrap();

        let diag = Diagnostics::new(true);
        let _tc = TranscodeManager::new(hls_root.clone(), false, None, 4, diag.clone());
        assert!(!orphan.exists(), "Orphaned /dev/shm HLS dir must be purged on startup");

        let app = middleware::from_fn(normalize_double_slashes).layer(build_router(test_state(&tmp)));
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/diagnostics/toggle")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"enabled":false}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["enabled"], false);
    }

    #[tokio::test]
    async fn test_jellyfin_xbox_handshake_and_health() {
        let tmp = tempfile::tempdir().unwrap();
        let app = middleware::from_fn(normalize_double_slashes).layer(build_router(test_state(&tmp)));

        // Verify Xbox double-slash quirk: GET //System/Info/Public
        let resp = app
            .clone()
            .oneshot(Request::builder().uri("//System/Info/Public").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ServerName"], "NUC Test");
        assert_eq!(v["StartupWizardCompleted"], true);

        let resp2 = app
            .oneshot(Request::builder().uri("/api/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_db_search_progress_and_direct_range_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(&tmp);

        let fake_video = tmp.path().join("sample.mp4");
        std::fs::write(&fake_video, b"0123456789abcdefghijklmnopqrstuvwxyz").unwrap();

        let id = state
            .db
            .upsert_media(
                &fake_video.to_string_lossy(),
                "movie",
                "Mortal Kombat II",
                Some(2026),
                None,
                None,
                None,
                36,
                100,
                7200.0,
                1920,
                1080,
                "h264",
                "aac",
                "mp4",
                "[]",
            )
            .unwrap();

        let hits = state.db.search("Mortal", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);

        state.db.update_progress(id, 300.0, None).unwrap();
        let cont = state.db.list_continue_watching(10).unwrap();
        assert_eq!(cont.len(), 1);
        assert_eq!(cont[0].position_secs, 300.0);

        let app = build_router(state);
        // Standard range
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/stream/{}/direct", id))
                    .header(header::RANGE, "bytes=0-9")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..], b"0123456789");

        // Suffix range (MP4 moov atom at EOF)
        let resp_suf = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/stream/{}/direct", id))
                    .header(header::RANGE, "bytes=-6")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_suf.status(), StatusCode::PARTIAL_CONTENT);
        let suf_bytes = resp_suf.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&suf_bytes[..], b"uvwxyz");

        // Out-of-bounds range -> 416 Range Not Satisfiable
        let resp_oob = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/stream/{}/direct", id))
                    .header(header::RANGE, "bytes=9999-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_oob.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[tokio::test]
    async fn test_ffmpeg_e2e_hls_stream_and_session_cleanup() {
        // Skip gracefully if ffmpeg is not installed in the build environment
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let media_dir = tmp.path().join("media").join("Movies");
        std::fs::create_dir_all(&media_dir).unwrap();
        let sample_mkv = media_dir.join("Test.Movie.2025.1080p.mkv");

        // Generate a 4-second synthetic test clip (H.264 + AAC)
        let gen = std::process::Command::new("ffmpeg")
            .args([
                "-v", "error", "-y",
                "-f", "lavfi", "-i", "testsrc=duration=4:size=320x180:rate=24",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=4",
                "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac",
            ])
            .arg(&sample_mkv)
            .status()
            .unwrap();
        assert!(gen.success(), "Failed to generate synthetic test MKV");

        let state = test_state(&tmp);
        let report = scanner::scan_library(&state.db, &state.media_root).unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.updated, 1);

        let movies = state.db.list_movies().unwrap();
        assert_eq!(movies.len(), 1);
        let mid = movies[0].id;

        // Start HLS stream in CPU mode and verify playlist + segment + session replacement cleanup
        let s1 = state
            .transcoder
            .start_stream(
                &movies[0],
                &StartStreamRequest {
                    start_secs: 0.0,
                    quality: "cpu".to_string(),
                    previous_session_id: None,
                },
            )
            .await
            .unwrap();
        let sid1 = s1.session_id.expect("HLS session_id expected");
        assert_eq!(state.transcoder.active_session_count().await, 1);

        let app = build_router(state.clone());
        let m3u8_resp = app
            .clone()
            .oneshot(Request::builder().uri(&s1.stream_url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(m3u8_resp.status(), StatusCode::OK);
        let m3u8_body = String::from_utf8_lossy(&m3u8_resp.into_body().collect().await.unwrap().to_bytes()).to_string();
        assert!(m3u8_body.contains(".ts"), "Playlist must contain .ts segment");

        // Starting a second stream with previous_session_id must clean up the first session
        let s2 = state
            .transcoder
            .start_stream(
                &movies[0],
                &StartStreamRequest {
                    start_secs: 1.0,
                    quality: "cpu".to_string(),
                    previous_session_id: Some(sid1),
                },
            )
            .await
            .unwrap();
        assert_eq!(state.transcoder.active_session_count().await, 1);
        if let Some(sid2) = s2.session_id {
            state.transcoder.stop_session(&sid2).await;
        }
        assert_eq!(state.transcoder.active_session_count().await, 0);
        assert_eq!(movies[0].id, mid);
    }
}
