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
use std::sync::Arc;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio_util::io::ReaderStream;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing::{error, info};
use transcode::{extract_subtitle_vtt, StartStreamRequest, TranscodeManager};

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
    pub scanning: Arc<tokio::sync::Mutex<bool>>,
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
    let mut guard = state.scanning.lock().await;
    if *guard {
        return (StatusCode::ACCEPTED, Json(json!({ "status": "already_scanning" }))).into_response();
    }
    *guard = true;
    drop(guard);

    let db = state.db.clone();
    let root = state.media_root.clone();
    let scanning = state.scanning.clone();

    let res = tokio::task::spawn_blocking(move || scanner::scan_library(&db, &root)).await;
    *scanning.lock().await = false;

    match res {
        Ok(Ok(report)) => (StatusCode::OK, Json(report)).into_response(),
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

/// Full HTTP Range (206 Partial Content) direct byte-streaming for instant seek & zero transcoding.
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
        if let Some(spec) = range_val.strip_prefix("bytes=") {
            let parts: Vec<&str> = spec.split('-').collect();
            if parts.len() == 2 {
                let start: u64 = parts[0].parse().unwrap_or(0);
                let end: u64 = if parts[1].is_empty() {
                    (start + 4 * 1024 * 1024 - 1).min(file_size.saturating_sub(1))
                } else {
                    parts[1].parse().unwrap_or(file_size.saturating_sub(1)).min(file_size.saturating_sub(1))
                };
                if start <= end && start < file_size {
                    let len = end - start + 1;
                    if file.seek(SeekFrom::Start(start)).await.is_ok() {
                        let stream = ReaderStream::new(file.take(len));
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
            }
        }
    }

    let stream = ReaderStream::new(file);
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
    let Some(file_path) = state.transcoder.touch_and_resolve(&session_id, &filename).await else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Wait briefly if FFmpeg is actively writing the next segment
    for _ in 0..25 {
        if file_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    let bytes = match tokio::fs::read(&file_path).await {
        Ok(b) => b,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    let ctype = if filename.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if filename.ends_with(".ts") {
        "video/mp2t"
    } else {
        "application/octet-stream"
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ctype),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        bytes,
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

    match extract_subtitle_vtt(std::path::Path::new(&item.path), track).await {
        Ok(vtt) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/vtt; charset=utf-8")],
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
        scanning: Arc::new(tokio::sync::Mutex::new(false)),
    };

    if cfg.scan_on_startup {
        let scan_db = db.clone();
        let scan_root = cfg.media_dir.clone();
        let enrich_art = state.artwork.clone();
        let enrich_db = db.clone();
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = scanner::scan_library(&scan_db, &scan_root) {
                    error!("Initial scan error: {}", e);
                }
            })
            .await;
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

    axum::serve(listener, ServiceExt::<Request>::into_make_service(app)).await?;
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
            scanning: Arc::new(tokio::sync::Mutex::new(false)),
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
        let resp = app
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
    }
}
