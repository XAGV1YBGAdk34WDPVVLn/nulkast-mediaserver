use crate::db::MediaItem;
use crate::diagnostics::Diagnostics;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Semaphore};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
pub struct StartStreamRequest {
    #[serde(default)]
    pub start_secs: f64,
    /// "auto" | "remux" | "1080p" | "720p" | "480p"
    #[serde(default = "default_quality")]
    pub quality: String,
}

fn default_quality() -> String {
    "auto".to_string()
}

#[derive(Debug, Clone, Serialize)]
pub struct StartStreamResponse {
    pub mode: String, // "direct" | "remux" | "vaapi" | "software"
    pub session_id: Option<String>,
    pub stream_url: String,
    pub hw_accel: bool,
    pub start_secs: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSnapshot {
    pub id: String,
    pub media_id: i64,
    pub mode: String,
    pub idle_secs: u64,
    pub segments_ready: usize,
    pub last_stderr: Vec<String>,
}

pub struct ActiveSession {
    pub id: String,
    pub media_id: i64,
    pub dir: PathBuf,
    pub mode: String,
    pub child: Option<Child>,
    pub last_access: Instant,
    pub stderr_tail: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone)]
pub struct TranscodeManager {
    pub scratch_root: PathBuf,
    pub vaapi_device: Option<PathBuf>,
    pub hls_segment_secs: u32,
    pub diag: Diagnostics,
    sessions: Arc<Mutex<HashMap<String, ActiveSession>>>,
    thumb_sem: Arc<Semaphore>,
}

impl TranscodeManager {
    pub fn new(
        scratch_root: PathBuf,
        hw_accel: bool,
        explicit_vaapi: Option<PathBuf>,
        hls_segment_secs: u32,
        diag: Diagnostics,
    ) -> Self {
        let vaapi_device = if !hw_accel {
            None
        } else if let Some(dev) = explicit_vaapi {
            if dev.exists() { Some(dev) } else { None }
        } else {
            [
                PathBuf::from("/dev/dri/renderD128"),
                PathBuf::from("/dev/dri/renderD129"),
            ]
            .into_iter()
            .find(|p| p.exists())
        };

        if let Some(ref dev) = vaapi_device {
            info!("Hardware VA-API device detected: {:?}", dev);
        } else {
            warn!("No /dev/dri/renderD* device found (or hw_accel=false); video transcoding will use CPU fallback");
        }

        let _ = std::fs::create_dir_all(&scratch_root);

        Self {
            scratch_root,
            vaapi_device,
            hls_segment_secs: hls_segment_secs.clamp(2, 10),
            diag,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            thumb_sem: Arc::new(Semaphore::new(2)),
        }
    }

    pub fn can_direct_play(item: &MediaItem) -> bool {
        let container_ok = matches!(item.container.as_str(), "mp4" | "m4v" | "webm");
        let v_ok = matches!(item.video_codec.as_str(), "h264" | "avc1" | "vp9" | "av1");
        let a_ok = matches!(item.audio_codec.as_str(), "aac" | "mp3" | "opus" | "vorbis" | "");
        container_ok && v_ok && a_ok
    }

    pub fn can_remux_video(item: &MediaItem, quality: &str) -> bool {
        if matches!(quality, "720p" | "480p") {
            return false;
        }
        if quality == "1080p" && item.height > 1080 {
            return false;
        }
        matches!(item.video_codec.as_str(), "h264" | "avc1")
    }

    pub async fn start_stream(
        &self,
        item: &MediaItem,
        req: &StartStreamRequest,
    ) -> Result<StartStreamResponse> {
        // Direct Play fast path: zero FFmpeg overhead
        if req.quality == "auto" && Self::can_direct_play(item) {
            self.diag.record(
                "info",
                "stream",
                format!("Direct Play media={} ({})", item.id, item.title),
            );
            return Ok(StartStreamResponse {
                mode: "direct".to_string(),
                session_id: None,
                stream_url: format!("/api/stream/{}/direct", item.id),
                hw_accel: false,
                start_secs: req.start_secs,
            });
        }

        let session_id = Uuid::new_v4().to_string();
        let dir = self.scratch_root.join(&session_id);
        std::fs::create_dir_all(&dir)?;

        let playlist_path = dir.join("index.m3u8");
        let seg_pattern = dir.join("seg_%05d.ts");

        let mut cmd = Command::new("ffmpeg");
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .args(["-v", "warning"]);

        let (mode, hw_accel) = if Self::can_remux_video(item, &req.quality) {
            if req.start_secs > 1.0 {
                cmd.args(["-ss", &format!("{:.2}", req.start_secs)]);
            }
            cmd.args(["-i", &item.path])
                .args(["-map", "0:v:0", "-map", "0:a:0?"])
                .args(["-c:v", "copy"])
                .args(["-c:a", "aac", "-ac", "2", "-b:a", "192k"]);
            ("remux".to_string(), false)
        } else if let Some(ref dev) = self.vaapi_device {
            // scale_vaapi=format=nv12 converts 10-bit HEVC (p010) to 8-bit nv12 in GPU VRAM
            // so h264_vaapi works on both 8-bit and 10-bit sources with zero CPU copy.
            cmd.args(["-hwaccel", "vaapi"])
                .args(["-vaapi_device", &dev.to_string_lossy()])
                .args(["-hwaccel_output_format", "vaapi"]);
            if req.start_secs > 1.0 {
                cmd.args(["-ss", &format!("{:.2}", req.start_secs)]);
            }
            cmd.args(["-i", &item.path])
                .args(["-map", "0:v:0", "-map", "0:a:0?"]);

            let scale_filter = match req.quality.as_str() {
                "480p" => "scale_vaapi=w=-2:h=480:format=nv12",
                "720p" => "scale_vaapi=w=-2:h=720:format=nv12",
                "1080p" => "scale_vaapi=w=-2:h=1080:format=nv12",
                _ => "scale_vaapi=format=nv12",
            };
            let bitrate = match req.quality.as_str() {
                "480p" => "2500k",
                "720p" => "5000k",
                _ => "12000k",
            };

            cmd.args(["-vf", scale_filter])
                .args(["-c:v", "h264_vaapi", "-b:v", bitrate, "-maxrate", bitrate, "-bufsize", "24000k"])
                .args(["-c:a", "aac", "-ac", "2", "-b:a", "192k"]);
            ("vaapi".to_string(), true)
        } else {
            if req.start_secs > 1.0 {
                cmd.args(["-ss", &format!("{:.2}", req.start_secs)]);
            }
            cmd.args(["-i", &item.path])
                .args(["-map", "0:v:0", "-map", "0:a:0?"]);
            match req.quality.as_str() {
                "480p" => { cmd.args(["-vf", "scale=-2:480"]); }
                "720p" => { cmd.args(["-vf", "scale=-2:720"]); }
                _ => {}
            }
            cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "22", "-pix_fmt", "yuv420p"])
                .args(["-c:a", "aac", "-ac", "2", "-b:a", "192k"]);
            ("software".to_string(), false)
        };

        let hls_time = self.hls_segment_secs.to_string();
        cmd.args([
            "-f",
            "hls",
            "-hls_time",
            &hls_time,
            "-hls_list_size",
            "0",
            "-hls_flags",
            "independent_segments+temp_file",
            "-hls_segment_filename",
            &seg_pattern.to_string_lossy(),
            &playlist_path.to_string_lossy(),
        ]);

        let mut child = cmd.spawn().map_err(|e| anyhow!("Failed to spawn ffmpeg: {}", e))?;
        let stderr_tail = Arc::new(Mutex::new(Vec::<String>::new()));

        if let Some(stderr) = child.stderr.take() {
            let tail_clone = stderr_tail.clone();
            let diag_clone = self.diag.clone();
            let sid_clone = session_id.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if !line.trim().is_empty() {
                        let mut guard = tail_clone.lock().await;
                        if guard.len() >= 15 {
                            guard.remove(0);
                        }
                        guard.push(line.clone());
                        diag_clone.record("warn", "ffmpeg", format!("[{}] {}", &sid_clone[..8], line));
                    }
                }
            });
        }

        info!(
            "Started HLS session {} for media {} (mode={}, codec={}/{}, start={:.1}s)",
            session_id, item.id, mode, item.video_codec, item.audio_codec, req.start_secs
        );
        self.diag.record(
            "info",
            "transcode",
            format!(
                "Started {} session {} for '{}' ({}/{}, {}p, start={:.0}s)",
                mode, &session_id[..8], item.title, item.video_codec, item.audio_codec, item.height, req.start_secs
            ),
        );

        {
            let mut map = self.sessions.lock().await;
            map.insert(
                session_id.clone(),
                ActiveSession {
                    id: session_id.clone(),
                    media_id: item.id,
                    dir: dir.clone(),
                    mode: mode.clone(),
                    child: Some(child),
                    last_access: Instant::now(),
                    stderr_tail: stderr_tail.clone(),
                },
            );
        }

        // Wait up to 10s for index.m3u8 and first .ts segment, aborting early if ffmpeg exits with error
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if playlist_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&playlist_path) {
                    if content.contains(".ts") {
                        return Ok(StartStreamResponse {
                            mode,
                            session_id: Some(session_id.clone()),
                            stream_url: format!("/api/hls/{}/index.m3u8", session_id),
                            hw_accel,
                            start_secs: req.start_secs,
                        });
                    }
                }
            }
            // Check if ffmpeg died prematurely
            let exited_err = {
                let mut map = self.sessions.lock().await;
                if let Some(sess) = map.get_mut(&session_id) {
                    if let Some(ref mut ch) = sess.child {
                        match ch.try_wait() {
                            Ok(Some(status)) if !status.success() => Some(status.to_string()),
                            _ => None,
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some(status) = exited_err {
                let err_lines = stderr_tail.lock().await.join(" | ");
                error!("FFmpeg exited early ({}): {}", status, err_lines);
                self.diag.record("error", "ffmpeg", format!("FFmpeg failed ({}): {}", status, err_lines));
                self.stop_session(&session_id).await;
                return Err(anyhow!("FFmpeg failed ({}): {}", status, err_lines));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Ok(StartStreamResponse {
            mode,
            session_id: Some(session_id.clone()),
            stream_url: format!("/api/hls/{}/index.m3u8", session_id),
            hw_accel,
            start_secs: req.start_secs,
        })
    }

    pub async fn generate_thumb(&self, item: &MediaItem, thumb_path: &Path) -> Result<()> {
        if thumb_path.exists() {
            return Ok(());
        }
        if let Some(parent) = thumb_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _permit = self.thumb_sem.acquire().await?;
        if thumb_path.exists() {
            return Ok(());
        }
        let seek_secs = if item.duration_secs > 180.0 {
            (item.duration_secs * 0.12).min(180.0)
        } else if item.duration_secs > 20.0 {
            item.duration_secs * 0.2
        } else {
            2.0
        };
        let tmp_path = thumb_path.with_extension("tmp.jpg");
        let status = Command::new("ffmpeg")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(["-v", "error", "-ss", &format!("{:.1}", seek_secs), "-i", &item.path])
            .args(["-frames:v", "1", "-vf", "scale=480:-1", "-q:v", "5", "-y"])
            .arg(&tmp_path)
            .status()
            .await?;

        if status.success() && tmp_path.exists() {
            let _ = std::fs::rename(&tmp_path, thumb_path);
            Ok(())
        } else {
            let _ = std::fs::remove_file(&tmp_path);
            Err(anyhow!("Thumbnail generation failed for media {}", item.id))
        }
    }

    pub async fn touch_and_resolve(&self, session_id: &str, filename: &str) -> Option<PathBuf> {
        if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
            return None;
        }
        let mut map = self.sessions.lock().await;
        let sess = map.get_mut(session_id)?;
        sess.last_access = Instant::now();
        Some(sess.dir.join(filename))
    }

    pub async fn stop_session(&self, session_id: &str) {
        let mut map = self.sessions.lock().await;
        if let Some(mut sess) = map.remove(session_id) {
            if let Some(ref mut child) = sess.child {
                let _ = child.kill().await;
            }
            let _ = std::fs::remove_dir_all(&sess.dir);
            info!("Stopped and cleaned HLS session {}", session_id);
        }
    }

    pub async fn reap_idle_sessions(&self, max_idle: Duration) {
        let mut to_remove = Vec::new();
        {
            let map = self.sessions.lock().await;
            for (id, sess) in map.iter() {
                if sess.last_access.elapsed() > max_idle {
                    to_remove.push(id.clone());
                }
            }
        }
        for id in to_remove {
            self.stop_session(&id).await;
        }
    }

    pub async fn active_session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    pub async fn session_snapshots(&self) -> Vec<SessionSnapshot> {
        let map = self.sessions.lock().await;
        let mut out = Vec::with_capacity(map.len());
        for s in map.values() {
            let segs = std::fs::read_dir(&s.dir)
                .ok()
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter(|e| e.file_name().to_string_lossy().ends_with(".ts"))
                        .count()
                })
                .unwrap_or(0);
            let last_stderr = s.stderr_tail.lock().await.clone();
            out.push(SessionSnapshot {
                id: s.id.clone(),
                media_id: s.media_id,
                mode: s.mode.clone(),
                idle_secs: s.last_access.elapsed().as_secs(),
                segments_ready: segs,
                last_stderr,
            });
        }
        out
    }
}

pub async fn extract_subtitle_vtt(media_path: &Path, stream_index: i64) -> Result<String> {
    let output = Command::new("ffmpeg")
        .args(["-v", "quiet", "-i"])
        .arg(media_path)
        .args(["-map", &format!("0:{}", stream_index), "-f", "webvtt", "pipe:1"])
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!("ffmpeg subtitle extraction failed"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
