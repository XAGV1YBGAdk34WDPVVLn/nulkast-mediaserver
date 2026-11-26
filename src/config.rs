use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub server_name: String,
    pub server_id: String,
    pub host: String,
    pub port: u16,
    pub public_url: String,
    pub media_dir: PathBuf,
    pub data_dir: PathBuf,
    pub transcode_dir: PathBuf,
    pub web_dir: PathBuf,
    pub hw_accel: bool,
    pub vaapi_device: Option<PathBuf>,
    pub hls_segment_secs: u32,
    pub idle_timeout_secs: u64,
    pub scan_on_startup: bool,
    pub scan_interval_secs: u64,
    pub diagnostics_enabled: bool,
    pub online_artwork: bool,
    pub tmdb_api_key: Option<String>,
    pub torrent_api_url: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server_name: "NulKast MediaServer".to_string(),
            server_id: "nulkast-mediaserver-01".to_string(),
            host: "0.0.0.0".to_string(),
            port: 8275,
            public_url: "http://localhost:8275".to_string(),
            media_dir: PathBuf::from("/media"),
            data_dir: PathBuf::from("./data"),
            transcode_dir: PathBuf::from("/dev/shm/nulkast-hls"),
            web_dir: PathBuf::from("./web"),
            hw_accel: true,
            vaapi_device: None,
            hls_segment_secs: 4,
            idle_timeout_secs: 900,
            scan_on_startup: true,
            scan_interval_secs: 900,
            diagnostics_enabled: true,
            online_artwork: true,
            tmdb_api_key: None,
            torrent_api_url: None,
        }
    }
}

impl AppConfig {
    /// Loads optional TOML config (`CONFIG_PATH` or `./config.toml`) and applies env var overrides.
    pub fn load() -> Self {
        let config_path = std::env::var("CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./config.toml"));
        Self::load_from(&config_path)
    }

    pub fn load_from(path: &Path) -> Self {
        let mut cfg = if path.exists() {
            match std::fs::read_to_string(path)
                .ok()
                .and_then(|s| toml::from_str::<AppConfig>(&s).ok())
            {
                Some(parsed) => {
                    info!("Loaded config from {:?}", path);
                    parsed
                }
                None => {
                    warn!("Failed to parse {:?}; falling back to defaults + env", path);
                    Self::default()
                }
            }
        } else {
            Self::default()
        };

        if let Ok(v) = std::env::var("SERVER_NAME") { cfg.server_name = v; }
        if let Ok(v) = std::env::var("SERVER_ID") { cfg.server_id = v; }
        if let Ok(v) = std::env::var("HOST") { cfg.host = v; }
        if let Ok(v) = std::env::var("PORT").or_else(|_| std::env::var("NULKAST_PORT")) {
            if let Ok(p) = v.parse() { cfg.port = p; }
        }
        if let Ok(v) = std::env::var("PUBLIC_URL") { cfg.public_url = v; }
        if let Ok(v) = std::env::var("MEDIA_DIR") { cfg.media_dir = PathBuf::from(v); }
        if let Ok(v) = std::env::var("DATA_DIR") { cfg.data_dir = PathBuf::from(v); }
        if let Ok(v) = std::env::var("TRANSCODE_DIR") { cfg.transcode_dir = PathBuf::from(v); }
        if let Ok(v) = std::env::var("WEB_DIR") { cfg.web_dir = PathBuf::from(v); }
        if let Ok(v) = std::env::var("HW_ACCEL") {
            cfg.hw_accel = !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no");
        }
        if let Ok(v) = std::env::var("VAAPI_DEVICE") {
            if !v.trim().is_empty() { cfg.vaapi_device = Some(PathBuf::from(v)); }
        }
        if let Ok(v) = std::env::var("HLS_SEGMENT_SECS") {
            if let Ok(n) = v.parse() { cfg.hls_segment_secs = n; }
        }
        if let Ok(v) = std::env::var("IDLE_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() { cfg.idle_timeout_secs = n; }
        }
        if let Ok(v) = std::env::var("SCAN_ON_STARTUP") {
            cfg.scan_on_startup = !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no");
        }
        if let Ok(v) = std::env::var("SCAN_INTERVAL_SECS") {
            if let Ok(n) = v.parse() { cfg.scan_interval_secs = n; }
        }
        if let Ok(v) = std::env::var("DIAGNOSTICS_ENABLED") {
            cfg.diagnostics_enabled = !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no");
        }
        if let Ok(v) = std::env::var("ONLINE_ARTWORK") {
            cfg.online_artwork = !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no");
        }
        if let Ok(v) = std::env::var("TMDB_API_KEY") {
            if !v.trim().is_empty() { cfg.tmdb_api_key = Some(v); }
        }
        if let Ok(v) = std::env::var("TORRENT_API_URL") {
            let trimmed = v.trim().trim_end_matches('/').to_string();
            if !trimmed.is_empty() { cfg.torrent_api_url = Some(trimmed); }
        }

        cfg
    }
}
