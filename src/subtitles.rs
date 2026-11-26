use crate::db::MediaItem;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::process::Command;

const OPENSUB_UA: &str = "TemporaryUserAgent";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleOption {
    pub id: String,
    pub label: String,
    pub language: String,
    pub source: String, // "embedded" | "sidecar" | "downloaded"
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineSubtitleResult {
    pub sub_id: String,
    pub filename: String,
    pub language: String,
    pub format: String,
    pub downloads: i64,
    pub hearing_impaired: bool,
    pub download_url: String,
}

pub fn sanitize_token(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

pub fn url_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Pure-Rust fallback SRT -> WebVTT converter (used if ffmpeg fails on quirky encodings).
pub fn srt_to_vtt_fallback(srt: &str) -> String {
    let cleaned = srt.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::from("WEBVTT\n\n");
    for line in cleaned.lines() {
        if line.contains("-->") {
            out.push_str(&line.replace(',', "."));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Lists embedded tracks, sidecar .srt/.vtt/.ass next to the video, and previously downloaded subs.
pub fn list_available_subtitles(item: &MediaItem, subs_dir: &Path) -> Vec<SubtitleOption> {
    let mut opts = Vec::new();

    // 1. Embedded text subtitle tracks
    if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&item.subtitles_json) {
        for sub in arr {
            let idx = sub["index"].as_i64().unwrap_or(-1);
            if idx < 0 {
                continue;
            }
            let lang = sub["language"].as_str().unwrap_or("und").to_string();
            let title = sub["title"].as_str().unwrap_or("").trim();
            let codec = sub["codec"].as_str().unwrap_or("text");
            let label = if title.is_empty() {
                format!("{} ({})", lang.to_uppercase(), codec.to_uppercase())
            } else {
                format!("{} — {}", lang.to_uppercase(), title)
            };
            opts.push(SubtitleOption {
                id: format!("emb_{}", idx),
                label,
                language: lang,
                source: "embedded".to_string(),
                url: format!("/api/stream/{}/subtitles/{}", item.id, idx),
            });
        }
    }

    // 2. Previously downloaded online subtitles cached in /data/subs/
    let prefix = format!("dl_{}_", item.id);
    if let Ok(rd) = std::fs::read_dir(subs_dir) {
        let mut dl_entries: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("vtt")
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with(&prefix))
                        .unwrap_or(false)
            })
            .collect();
        dl_entries.sort();
        for p in dl_entries {
            let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let meta_path = p.with_extension("json");
            let (label, lang) = std::fs::read_to_string(&meta_path)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .map(|v| {
                    (
                        v["label"].as_str().unwrap_or(fname).to_string(),
                        v["language"].as_str().unwrap_or("eng").to_string(),
                    )
                })
                .unwrap_or_else(|| (fname.to_string(), "eng".to_string()));
            opts.push(SubtitleOption {
                id: fname.trim_end_matches(".vtt").to_string(),
                label,
                language: lang,
                source: "downloaded".to_string(),
                url: format!("/api/media/{}/subtitles/file/{}", item.id, fname),
            });
        }
    }

    // 3. Sidecar .srt / .vtt / .ass files next to the video file
    let video_path = Path::new(&item.path);
    if let (Some(parent), Some(stem)) = (video_path.parent(), video_path.file_stem().and_then(|s| s.to_str())) {
        let stem_low = stem.to_ascii_lowercase();
        if let Ok(rd) = std::fs::read_dir(parent) {
            for entry in rd.flatten() {
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                if !matches!(ext.as_str(), "srt" | "vtt" | "ass" | "ssa") {
                    continue;
                }
                let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !fname.to_ascii_lowercase().starts_with(&stem_low) {
                    continue;
                }
                let safe_key = format!("side_{}_{}", item.id, sanitize_token(fname));
                opts.push(SubtitleOption {
                    id: safe_key.clone(),
                    label: format!("Sidecar: {}", fname),
                    language: "eng".to_string(),
                    source: "sidecar".to_string(),
                    url: format!("/api/media/{}/subtitles/sidecar/{}", item.id, url_encode_path(fname)),
                });
            }
        }
    }

    opts
}

async fn curl_opensub(url: &str, timeout_secs: u64) -> Result<Vec<u8>> {
    let t = timeout_secs.to_string();
    let out = Command::new("curl")
        .args(["-sfL", "--max-time", &t, "-A", OPENSUB_UA, url])
        .output()
        .await?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(anyhow!("OpenSubtitles request failed for {}", url));
    }
    Ok(out.stdout)
}

/// Searches OpenSubtitles public REST API (zero API key) for English subtitles.
pub async fn search_online_subtitles(
    item: &MediaItem,
    lang: &str,
    custom_query: Option<&str>,
) -> Result<Vec<OnlineSubtitleResult>> {
    let clean_lang = sanitize_token(lang);
    let sub_lang = if clean_lang.is_empty() { "eng" } else { &clean_lang };

    let mut urls: Vec<String> = Vec::new();
    if let Some(q) = custom_query.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        urls.push(format!(
            "https://rest.opensubtitles.org/search/query-{}/sublanguageid-{}",
            url_encode_path(q),
            sub_lang
        ));
    } else if item.kind == "episode" {
        let show = item.show_name.as_deref().unwrap_or(&item.title);
        let q = url_encode_path(&show.to_ascii_lowercase());
        if let (Some(s), Some(e)) = (item.season, item.episode) {
            urls.push(format!(
                "https://rest.opensubtitles.org/search/episode-{}/query-{}/season-{}/sublanguageid-{}",
                e, q, s, sub_lang
            ));
        }
        urls.push(format!(
            "https://rest.opensubtitles.org/search/query-{}/sublanguageid-{}",
            q, sub_lang
        ));
    } else {
        if !item.imdb_id.is_empty() {
            let digits: String = item.imdb_id.chars().filter(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                urls.push(format!(
                    "https://rest.opensubtitles.org/search/imdbid-{}/sublanguageid-{}",
                    digits, sub_lang
                ));
            }
        }
        let q = url_encode_path(&item.title.to_ascii_lowercase());
        urls.push(format!(
            "https://rest.opensubtitles.org/search/query-{}/sublanguageid-{}",
            q, sub_lang
        ));
    }

    let mut parsed_items: Vec<Value> = Vec::new();
    for url in &urls {
        if let Ok(bytes) = curl_opensub(url, 10).await {
            if let Ok(arr) = serde_json::from_slice::<Vec<Value>>(&bytes) {
                if !arr.is_empty() {
                    parsed_items = arr;
                    break;
                }
            }
        }
    }

    let mut results: Vec<OnlineSubtitleResult> = Vec::new();
    for v in parsed_items {
        let dl_url = v["SubDownloadLink"].as_str().unwrap_or("").trim();
        let filename = v["SubFileName"].as_str().unwrap_or("").trim();
        let format = v["SubFormat"].as_str().unwrap_or("srt").to_ascii_lowercase();
        let sub_id = v["IDSubtitleFile"].as_str().unwrap_or("").trim();
        if dl_url.is_empty() || filename.is_empty() || sub_id.is_empty() {
            continue;
        }
        if !matches!(format.as_str(), "srt" | "vtt" | "ass" | "ssa" | "sub") {
            continue;
        }
        let downloads = v["SubDownloadsCnt"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| v["SubDownloadsCnt"].as_i64())
            .unwrap_or(0);
        let hi = v["SubHearingImpaired"].as_str().map(|s| s == "1").unwrap_or(false);
        let language = v["LanguageName"].as_str().unwrap_or("English").to_string();
        results.push(OnlineSubtitleResult {
            sub_id: sanitize_token(sub_id),
            filename: filename.to_string(),
            language,
            format,
            downloads,
            hearing_impaired: hi,
            download_url: dl_url.to_string(),
        });
    }

    results.sort_by(|a, b| b.downloads.cmp(&a.downloads));
    results.truncate(15);
    Ok(results)
}

/// Converts any local subtitle file (.srt/.ass/.vtt) to WebVTT string via ffmpeg (with SRT fallback).
pub async fn convert_file_to_vtt(src_path: &Path) -> Result<String> {
    if src_path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("vtt")).unwrap_or(false) {
        return Ok(tokio::fs::read_to_string(src_path).await?);
    }
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(src_path)
        .args(["-f", "webvtt", "pipe:1"])
        .output()
        .await;
    if let Ok(o) = out {
        if o.status.success() && !o.stdout.is_empty() {
            return Ok(String::from_utf8_lossy(&o.stdout).to_string());
        }
    }
    let raw = tokio::fs::read(src_path).await?;
    Ok(srt_to_vtt_fallback(&String::from_utf8_lossy(&raw)))
}

/// Downloads a subtitle from OpenSubtitles, gunzips if needed, converts to WebVTT, and caches it.
pub async fn download_and_cache_subtitle(
    media_id: i64,
    sub_id: &str,
    download_url: &str,
    filename: &str,
    language: &str,
    subs_dir: &Path,
) -> Result<SubtitleOption> {
    if !download_url.starts_with("https://dl.opensubtitles.org/") {
        return Err(anyhow!("Unsupported subtitle download host"));
    }
    let safe_id = sanitize_token(sub_id);
    if safe_id.is_empty() {
        return Err(anyhow!("Invalid subtitle id"));
    }
    tokio::fs::create_dir_all(subs_dir).await?;

    let vtt_name = format!("dl_{}_{}.vtt", media_id, safe_id);
    let vtt_path = subs_dir.join(&vtt_name);
    let meta_path = vtt_path.with_extension("json");

    if !tokio::fs::try_exists(&vtt_path).await.unwrap_or(false) {
        let raw_bytes = curl_opensub(download_url, 15).await?;
        let raw_path = subs_dir.join(format!("tmp_{}_{}.raw", media_id, safe_id));
        let ext = Path::new(filename)
            .extension()
            .and_then(|e| e.to_str())
            .filter(|e| matches!(e.to_ascii_lowercase().as_str(), "srt" | "vtt" | "ass" | "ssa"))
            .unwrap_or("srt");
        let unpacked_path = subs_dir.join(format!("tmp_{}_{}.{}", media_id, safe_id, ext));

        // Decompress gzip if magic header 0x1f 0x8b
        if raw_bytes.len() >= 2 && raw_bytes[0] == 0x1f && raw_bytes[1] == 0x8b {
            tokio::fs::write(&raw_path, &raw_bytes).await?;
            let gunzip = Command::new("gzip").args(["-dc"]).arg(&raw_path).output().await?;
            let _ = tokio::fs::remove_file(&raw_path).await;
            if !gunzip.status.success() || gunzip.stdout.is_empty() {
                return Err(anyhow!("Failed to gunzip downloaded subtitle"));
            }
            tokio::fs::write(&unpacked_path, &gunzip.stdout).await?;
        } else {
            tokio::fs::write(&unpacked_path, &raw_bytes).await?;
        }

        let vtt = convert_file_to_vtt(&unpacked_path).await?;
        let _ = tokio::fs::remove_file(&unpacked_path).await;

        let tmp_vtt = vtt_path.with_extension("tmp.vtt");
        tokio::fs::write(&tmp_vtt, &vtt).await?;
        tokio::fs::rename(&tmp_vtt, &vtt_path).await?;
    }

    let label = format!("{} — {}", language, filename);
    let meta = serde_json::json!({
        "label": label,
        "language": language,
        "filename": filename,
    });
    let _ = tokio::fs::write(&meta_path, meta.to_string()).await;

    Ok(SubtitleOption {
        id: format!("dl_{}_{}", media_id, safe_id),
        label,
        language: language.to_string(),
        source: "downloaded".to_string(),
        url: format!("/api/media/{}/subtitles/file/{}", media_id, vtt_name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_srt_to_vtt_fallback_converts_timestamps() {
        let srt = "1\r\n00:00:02,740 --> 00:00:06,790\r\nHello world!\r\n";
        let vtt = srt_to_vtt_fallback(srt);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("00:00:02.740 --> 00:00:06.790"));
        assert!(vtt.contains("Hello world!"));
    }
}
