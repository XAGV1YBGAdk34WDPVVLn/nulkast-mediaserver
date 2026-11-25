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

const MAX_CONCURRENT_SESSIONS: usize = 4;

#[derive(Debug, Clone, Deserialize)]
pub struct StartStreamRequest {
    #[serde(default)]
    pub start_secs: f64,
    /// "auto" | "remux" | "1080p" | "720p" | "480p" | "cpu"
    #[serde(default = "default_quality")]
    pub quality: String,
    #[serde(default)]
    pub previous_session_id: Option<String>,
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
    sub_sem: Arc<Semaphore>,
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

        // Clean up any orphaned HLS directories from prior runs/crashes in /dev/shm
        if scratch_root.exists() {
            if let Ok(rd) = std::fs::read_dir(&scratch_root) {
                let mut cleaned = 0usize;
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.is_dir() && std::fs::remove_dir_all(&p).is_ok() {
                        cleaned += 1;
                    }
                }
                if cleaned > 0 {
                    info!("Purged {} orphaned HLS session dirs from {:?}", cleaned, scratch_root);
                }
            }
        }
        let _ = std::fs::create_dir_all(&scratch_root);

        Self {
            scratch_root,
            vaapi_device,
            hls_segment_secs: hls_segment_secs.clamp(2, 10),
            diag,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            thumb_sem: Arc::new(Semaphore::new(2)),
            sub_sem: Arc::new(Semaphore::new(2)),
        }
    }

    pub fn can_direct_play(item: &MediaItem) -> bool {
        let container_ok = matches!(item.container.as_str(), "mp4" | "m4v" | "webm");
        let v_ok = matches!(item.video_codec.as_str(), "h264" | "avc1" | "vp9" | "av1");
        let a_ok = matches!(item.audio_codec.as_str(), "aac" | "mp3" | "opus" | "vorbis" | "");
        container_ok && v_ok && a_ok
    }

    pub fn can_remux_video(item: &MediaItem, quality: &str) -> bool {
        if matches!(quality, "720p" | "480p" | "cpu") {
            return false;
        }
        if quality == "1080p" && item.height > 1080 {
            return false;
        }
        matches!(item.video_codec.as_str(), "h264" | "avc1")
    }

    pub fn vaapi_decode_supported(codec: &str) -> bool {
        matches!(codec, "h264" | "avc1" | "hevc" | "h265" | "vp9" | "av1" | "mpeg2video")
    }

    fn target_height(item: &MediaItem, quality: &str) -> Option<i64> {
        let desired = match quality {
            "480p" => Some(480),
            "720p" => Some(720),
            "1080p" => Some(1080),
            _ => None,
        }?;
        // Never upscale smaller videos
        if item.height > 0 && item.height <= desired {
            None
        } else {
            Some(desired)
        }
    }

    pub async fn start_stream(
        &self,
        item: &MediaItem,
        req: &StartStreamRequest,
    ) -> Result<StartStreamResponse> {
        // Immediately clean up prior session if the client switched videos or seeked
        if let Some(ref prev_id) = req.previous_session_id {
            self.stop_session(prev_id).await;
        }

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

        // Enforce concurrent session cap (evict oldest idle session if at limit)
        self.evict_if_full().await;

        let try_remux = Self::can_remux_video(item, &req.quality);
        let try_vaapi = !try_remux
            && req.quality != "cpu"
            && self.vaapi_device.is_some()
            && Self::vaapi_decode_supported(&item.video_codec);

        let initial_mode = if try_remux {
            "remux"
        } else if try_vaapi {
            "vaapi"
        } else {
            "software"
        };

        match self.spawn_hls_session(item, req, initial_mode).await {
            Ok(resp) => Ok(resp),
            Err(e) if initial_mode == "vaapi" => {
                warn!("VA-API failed for media {} ({}); falling back to CPU libx264", item.id, e);
                self.diag.record(
                    "warn",
                    "transcode",
                    format!("VA-API fallback -> CPU for '{}' ({})", item.title, e),
                );
                self.spawn_hls_session(item, req, "software").await
            }
            Err(e) => Err(e),
        }
    }

    async fn spawn_hls_session(
        &self,
        item: &MediaItem,
        req: &StartStreamRequest,
        mode: &str,
    ) -> Result<StartStreamResponse> {
        let session_id = Uuid::new_v4().to_string();
        let dir = self.scratch_root.join(&session_id);
        tokio::fs::create_dir_all(&dir).await?;

        let playlist_path = dir.join("index.m3u8");
        let seg_pattern = dir.join("seg_%05d.ts");
        let seg_secs = self.hls_segment_secs;
        let kf_expr = format!("expr:gte(t,n_forced*{})", seg_secs);

        let mut cmd = Command::new("ffmpeg");
        cmd.kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .args(["-v", "warning", "-nostdin"]);

        let scale_h = Self::target_height(item, &req.quality);
        let (bitrate, bufsize) = match req.quality.as_str() {
            "480p" => ("2500k", "5000k"),
            "720p" => ("5000k", "10000k"),
            _ => ("10000k", "20000k"),
        };

        let hw_accel = mode == "vaapi";

        match mode {
            "remux" => {
                if req.start_secs > 0.0 {
                    cmd.args(["-ss", &format!("{:.2}", req.start_secs)]);
                }
                cmd.args(["-fflags", "+genpts", "-i", &item.path])
                    .args(["-map", "0:v:0", "-map", "0:a:0?"])
                    .args(["-c:v", "copy"])
                    .args(["-avoid_negative_ts", "make_zero"])
                    .args(["-af", "aresample=async=1:first_pts=0"])
                    .args(["-c:a", "aac", "-ac", "2", "-b:a", "192k"]);
            }
            "vaapi" => {
                let dev = self
                    .vaapi_device
                    .as_ref()
                    .ok_or_else(|| anyhow!("No VA-API device"))?;
                // Pace transcode at 2.5x realtime after an initial 24s fast burst so /dev/shm stays bounded
                cmd.args(["-readrate", "2.5", "-readrate_initial_burst", "24"])
                    .args(["-hwaccel", "vaapi"])
                    .args(["-vaapi_device", &dev.to_string_lossy()])
                    .args(["-hwaccel_output_format", "vaapi"]);
                if req.start_secs > 0.0 {
                    cmd.args(["-ss", &format!("{:.2}", req.start_secs)]);
                }
                cmd.args(["-i", &item.path])
                    .args(["-map", "0:v:0", "-map", "0:a:0?"]);

                // scale_vaapi=format=nv12 converts 10-bit HEVC (p010) -> 8-bit nv12 in GPU VRAM
                let vf = match scale_h {
                    Some(h) => format!("scale_vaapi=w=-2:h={}:format=nv12", h),
                    None => "scale_vaapi=format=nv12".to_string(),
                };
                cmd.args(["-vf", &vf])
                    .args(["-force_key_frames", &kf_expr])
                    .args(["-c:v", "h264_vaapi", "-b:v", bitrate, "-maxrate", bitrate, "-bufsize", bufsize])
                    .args(["-af", "aresample=async=1:first_pts=0"])
                    .args(["-c:a", "aac", "-ac", "2", "-b:a", "192k"]);
            }
            _ => {
                cmd.args(["-readrate", "2.5", "-readrate_initial_burst", "24"]);
                if req.start_secs > 0.0 {
                    cmd.args(["-ss", &format!("{:.2}", req.start_secs)]);
                }
                cmd.args(["-i", &item.path])
                    .args(["-map", "0:v:0", "-map", "0:a:0?"]);
                if let Some(h) = scale_h {
                    cmd.args(["-vf", &format!("scale=-2:{}", h)]);
                }
                cmd.args(["-force_key_frames", &kf_expr, "-sc_threshold", "0"])
                    .args([
                        "-c:v", "libx264", "-preset", "veryfast", "-profile:v", "high", "-level", "4.1",
                        "-crf", "22", "-maxrate", bitrate, "-bufsize", bufsize, "-pix_fmt", "yuv420p",
                    ])
                    .args(["-af", "aresample=async=1:first_pts=0"])
                    .args(["-c:a", "aac", "-ac", "2", "-b:a", "192k"]);
            }
        }

        let hls_time = seg_secs.to_string();
        cmd.args([
            "-f",
            "hls",
            "-hls_playlist_type",
            "event",
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
                    mode: mode.to_string(),
                    child: Some(child),
                    last_access: Instant::now(),
                    stderr_tail: stderr_tail.clone(),
                },
            );
        }

        // Wait up to 10s for index.m3u8 and first .ts segment, aborting early if ffmpeg exits with error
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if tokio::fs::try_exists(&playlist_path).await.unwrap_or(false) {
                if let Ok(content) = tokio::fs::read_to_string(&playlist_path).await {
                    if content.contains(".ts") {
                        return Ok(StartStreamResponse {
                            mode: mode.to_string(),
                            session_id: Some(session_id.clone()),
                            stream_url: format!("/api/hls/{}/index.m3u8", session_id),
                            hw_accel,
                            start_secs: req.start_secs,
                        });
                    }
                }
            }
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
                tokio::time::sleep(Duration::from_millis(50)).await;
                let err_lines = stderr_tail.lock().await.join(" | ");
                error!("FFmpeg ({}) exited early ({}): {}", mode, status, err_lines);
                self.diag.record("error", "ffmpeg", format!("FFmpeg {} failed ({}): {}", mode, status, err_lines));
                self.stop_session(&session_id).await;
                return Err(anyhow!("FFmpeg {} failed ({}): {}", mode, status, err_lines));
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
        }

        Ok(StartStreamResponse {
            mode: mode.to_string(),
            session_id: Some(session_id.clone()),
            stream_url: format!("/api/hls/{}/index.m3u8", session_id),
            hw_accel,
            start_secs: req.start_secs,
        })
    }

    async fn evict_if_full(&self) {
        let oldest_id = {
            let map = self.sessions.lock().await;
            if map.len() < MAX_CONCURRENT_SESSIONS {
                return;
            }
            map.values()
                .min_by_key(|s| s.last_access)
                .map(|s| s.id.clone())
        };
        if let Some(id) = oldest_id {
            warn!("Evicting LRU transcode session {} (max {} reached)", id, MAX_CONCURRENT_SESSIONS);
            self.stop_session(&id).await;
        }
    }

    pub async fn generate_thumb(&self, item: &MediaItem, thumb_path: &Path) -> Result<()> {
        if tokio::fs::try_exists(thumb_path).await.unwrap_or(false) {
            return Ok(());
        }
        if let Some(parent) = thumb_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _permit = self.thumb_sem.acquire().await?;
        if tokio::fs::try_exists(thumb_path).await.unwrap_or(false) {
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
        let status = tokio::time::timeout(
            Duration::from_secs(15),
            Command::new("ffmpeg")
                .kill_on_drop(true)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .args(["-v", "error", "-nostdin", "-ss", &format!("{:.1}", seek_secs), "-i", &item.path])
                .args(["-frames:v", "1", "-vf", "scale=480:-1", "-q:v", "5", "-y"])
                .arg(&tmp_path)
                .status(),
        )
        .await??;

        if status.success() && tokio::fs::try_exists(&tmp_path).await.unwrap_or(false) {
            let _ = tokio::fs::rename(&tmp_path, thumb_path).await;
            Ok(())
        } else {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            Err(anyhow!("Thumbnail generation failed for media {}", item.id))
        }
    }

    pub async fn extract_subtitle_cached(
        &self,
        media_id: i64,
        media_path: &Path,
        stream_index: i64,
        cache_dir: &Path,
    ) -> Result<String> {
        let cache_path = cache_dir.join(format!("{}_{}.vtt", media_id, stream_index));
        if let Ok(vtt) = tokio::fs::read_to_string(&cache_path).await {
            return Ok(vtt);
        }
        let _ = tokio::fs::create_dir_all(cache_dir).await;
        let _permit = self.sub_sem.acquire().await?;
        if let Ok(vtt) = tokio::fs::read_to_string(&cache_path).await {
            return Ok(vtt);
        }

        let output = tokio::time::timeout(
            Duration::from_secs(25),
            Command::new("ffmpeg")
                .kill_on_drop(true)
                .args(["-v", "quiet", "-nostdin", "-i"])
                .arg(media_path)
                .args(["-map", &format!("0:{}", stream_index), "-f", "webvtt", "pipe:1"])
                .output(),
        )
        .await??;

        if !output.status.success() {
            return Err(anyhow!("ffmpeg subtitle extraction failed"));
        }
        let vtt = String::from_utf8_lossy(&output.stdout).to_string();
        let tmp = cache_path.with_extension("tmp.vtt");
        if tokio::fs::write(&tmp, &vtt).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &cache_path).await;
        }
        Ok(vtt)
    }

    /// Resolves a session file and reports whether the underlying ffmpeg process is still running.
    pub async fn touch_and_resolve(&self, session_id: &str, filename: &str) -> Option<(PathBuf, bool)> {
        if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
            return None;
        }
        let mut map = self.sessions.lock().await;
        let sess = map.get_mut(session_id)?;
        sess.last_access = Instant::now();
        let running = match sess.child.as_mut() {
            Some(ch) => matches!(ch.try_wait(), Ok(None)),
            None => false,
        };
        Some((sess.dir.join(filename), running))
    }

    pub async fn stop_session(&self, session_id: &str) {
        // Release the sessions mutex BEFORE awaiting child.kill() and filesystem removal
        let removed = {
            let mut map = self.sessions.lock().await;
            map.remove(session_id)
        };
        if let Some(mut sess) = removed {
            if let Some(ref mut child) = sess.child {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            let _ = tokio::fs::remove_dir_all(&sess.dir).await;
            info!("Stopped and cleaned HLS session {}", session_id);
        }
    }

    pub async fn stop_all_sessions(&self) {
        let ids: Vec<String> = {
            let map = self.sessions.lock().await;
            map.keys().cloned().collect()
        };
        for id in ids {
            self.stop_session(&id).await;
        }
    }

    pub async fn reap_idle_sessions(&self, max_idle: Duration) {
        let to_remove: Vec<String> = {
            let map = self.sessions.lock().await;
            map.iter()
                .filter(|(_, sess)| sess.last_access.elapsed() > max_idle)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in to_remove {
            self.stop_session(&id).await;
        }
    }

    pub async fn active_session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    pub async fn session_snapshots(&self) -> Vec<SessionSnapshot> {
        let entries: Vec<(String, i64, String, PathBuf, u64, Arc<Mutex<Vec<String>>>)> = {
            let map = self.sessions.lock().await;
            map.values()
                .map(|s| {
                    (
                        s.id.clone(),
                        s.media_id,
                        s.mode.clone(),
                        s.dir.clone(),
                        s.last_access.elapsed().as_secs(),
                        s.stderr_tail.clone(),
                    )
                })
                .collect()
        };

        let mut out = Vec::with_capacity(entries.len());
        for (id, media_id, mode, dir, idle_secs, stderr_tail) in entries {
            let mut segs = 0usize;
            if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
                while let Ok(Some(e)) = rd.next_line_or_entry().await {
                    if e.file_name().to_string_lossy().ends_with(".ts") {
                        segs += 1;
                    }
                }
            }
            let last_stderr = stderr_tail.lock().await.clone();
            out.push(SessionSnapshot {
                id,
                media_id,
                mode,
                idle_secs,
                segments_ready: segs,
                last_stderr,
            });
        }
        out
    }
}

trait ReadDirCompat {
    async fn next_line_or_entry(&mut self) -> std::io::Result<Option<tokio::fs::DirEntry>>;
}
impl ReadDirCompat for tokio::fs::ReadDir {
    async fn next_line_or_entry(&mut self) -> std::io::Result<Option<tokio::fs::DirEntry>> {
        self.next_entry().await
    }
}
