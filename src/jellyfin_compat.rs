use axum::{
    extract::State,
    response::IntoResponse,
    Json,
};
use serde_json::json;
use tokio::net::UdpSocket;
use tracing::{info, warn};

use crate::AppState;

/// Minimal Jellyfin public server handshake so the official Jellyfin Xbox app
/// (a Chromium WebView2 shell) validates this server and loads our custom 10-foot Web UI.
pub async fn system_info_public(State(state): State<AppState>) -> impl IntoResponse {
    info!("Jellyfin handshake requested: /System/Info/Public");
    let compat_version = std::env::var("JELLYFIN_COMPAT_VERSION")
        .unwrap_or_else(|_| "10.11.5".to_string());
    Json(json!({
        "LocalAddress": state.public_url,
        "ServerName": state.server_name,
        "Version": compat_version,
        "ProductName": "Jellyfin Server",
        "OperatingSystem": "Linux",
        "Id": state.server_id,
        "StartupWizardCompleted": true
    }))
}

pub async fn system_ping() -> impl IntoResponse {
    "\"Jellyfin Server\""
}

pub async fn health_ok() -> impl IntoResponse {
    "Healthy"
}

pub async fn web_config_json() -> impl IntoResponse {
    Json(json!({
        "includeCorsCredentials": false,
        "multiserver": false,
        "themes": [],
        "menuLinks": [],
        "servers": []
    }))
}

pub async fn branding_configuration() -> impl IntoResponse {
    Json(json!({
        "LoginDisclaimer": "",
        "CustomCss": "",
        "SplashscreenEnabled": false
    }))
}

/// Spawns a UDP broadcast responder on port 7359 ("Who is JellyfinServer?")
/// so Xbox / mobile / TV Jellyfin apps auto-discover NulKast MediaServer on the LAN.
pub fn spawn_udp_discovery(
    discovery_port: u16,
    public_url: String,
    server_id: String,
    server_name: String,
) {
    tokio::spawn(async move {
        let bind_addr = format!("0.0.0.0:{}", discovery_port);
        let socket = match UdpSocket::bind(&bind_addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!("UDP discovery failed to bind {}: {}", bind_addr, e);
                return;
            }
        };
        let _ = socket.set_broadcast(true);
        info!(
            "Jellyfin LAN UDP auto-discovery listening on {} (advertising {})",
            bind_addr, public_url
        );

        let payload = json!({
            "Address": public_url,
            "Id": server_id,
            "Name": server_name,
            "EndpointAddress": null
        })
        .to_string();

        let mut buf = [0u8; 512];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, peer)) => {
                    let msg = String::from_utf8_lossy(&buf[..len]);
                    if msg.to_ascii_lowercase().contains("who is jellyfinserver") {
                        info!("UDP discovery probe from {} -> advertising {}", peer, public_url);
                        let _ = socket.send_to(payload.as_bytes(), peer).await;
                    }
                }
                Err(e) => {
                    warn!("UDP discovery recv error: {}", e);
                }
            }
        }
    });
}
