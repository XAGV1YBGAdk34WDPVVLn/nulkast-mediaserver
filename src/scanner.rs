use crate::db::{Db, SubtitleTrack};
use anyhow::Result;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;
use tracing::{info, warn};
use walkdir::WalkDir;

const VIDEO_EXTS: &[&str] = &["mkv", "mp4", "m4v", "avi", "mov", "webm", "ts", "m2ts"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMedia {
    pub kind: String,
    pub title: String,
    pub year: Option<i64>,
    pub show_name: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
}

#[derive(Debug, Default)]
pub struct ProbeInfo {
    pub duration_secs: f64,
    pub width: i64,
    pub height: i64,
    pub video_codec: String,
    pub audio_codec: String,
    pub container: String,
    pub subtitles: Vec<SubtitleTrack>,
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    format: Option<FfprobeFormat>,
    streams: Option<Vec<FfprobeStream>>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
    format_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    index: Option<i64>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    tags: Option<std::collections::HashMap<String, String>>,
}

pub fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| VIDEO_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn clean_title(raw: &str) -> String {
    static RE_BRACKETS: OnceLock<Regex> = OnceLock::new();
    static RE_QUALITY: OnceLock<Regex> = OnceLock::new();
    static RE_EDITION: OnceLock<Regex> = OnceLock::new();
    static RE_SPACES: OnceLock<Regex> = OnceLock::new();

    let re_brackets = RE_BRACKETS.get_or_init(|| Regex::new(r"[\[\(][^\]\)]*[\]\)]").unwrap());
    let re_quality = RE_QUALITY.get_or_init(|| {
        Regex::new(r"(?i)\b(2160p|1080p|720p|480p|4k|uhd|hdr10?|bluray|bdrip|brrip|web-?dl|webrip|hdtv|x264|x265|h\.?264|h\.?265|hevc|av1|10bit|aac5?\.?1?|ddp5?\.?1?|ac3|eac3|atmos|truehd|dts(-hd)?|dual|multi|remux|repack|proper|yts|rarbg|megusta|galaxyrg265|chivaman|varyg|cakes|bone|flux|vyndros|elite)\b.*$").unwrap()
    });
    let re_edition = RE_EDITION.get_or_init(|| {
        Regex::new(r"(?i)(\s*-\s*)?\b(\d+(st|nd|rd|th)\s+anniversary(\s+edition)?|anniversary\s+edition|director'?s\s+cut|extended(\s+cut|\s+edition)?|unrated|remastered|theatrical(\s+cut)?|special\s+edition)\b.*$").unwrap()
    });
    let re_spaces = RE_SPACES.get_or_init(|| Regex::new(r"\s+").unwrap());

    let mut s = raw.replace(['.', '_'], " ");
    s = re_brackets.replace_all(&s, " ").to_string();
    s = re_quality.replace(&s, "").to_string();
    s = re_edition.replace(&s, "").to_string();
    s = s.trim_matches(|c: char| c == '-' || c == ' ').to_string();
    re_spaces.replace_all(&s, " ").trim().to_string()
}

pub fn parse_media_path(root: &Path, file_path: &Path) -> ParsedMedia {
    let rel = file_path.strip_prefix(root).unwrap_or(file_path);
    let components: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    let stem = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown");

    let in_tv_dir = components
        .first()
        .map(|c| c.eq_ignore_ascii_case("tv") || c.eq_ignore_ascii_case("shows") || c.eq_ignore_ascii_case("series"))
        .unwrap_or(false);

    static RE_SE: OnceLock<Regex> = OnceLock::new();
    static RE_YEAR: OnceLock<Regex> = OnceLock::new();
    let re_se = RE_SE.get_or_init(|| Regex::new(r"(?i)S(\d{1,2})[\s._-]*E(\d{1,3})").unwrap());
    let re_year = RE_YEAR.get_or_init(|| Regex::new(r"\b(19\d{2}|20\d{2})\b").unwrap());

    if let Some(caps) = re_se.captures(stem) {
        let season: i64 = caps[1].parse().unwrap_or(1);
        let episode: i64 = caps[2].parse().unwrap_or(1);
        let se_match = caps.get(0).unwrap();

        let prefix = clean_title(&stem[..se_match.start()]);
        let suffix = clean_title(&stem[se_match.end()..]);

        let raw_show = if !prefix.is_empty() {
            prefix
        } else if in_tv_dir && components.len() >= 3 {
            clean_title(&components[1])
        } else {
            "Unknown Show".to_string()
        };
        // Strip trailing release year from TV show name (e.g. "Lanterns 2026" -> "Lanterns")
        let show_name = re_year
            .replace_all(&raw_show, "")
            .trim_matches(|c: char| c == '-' || c == ' ')
            .to_string();
        let show_name = if show_name.is_empty() { raw_show } else { show_name };

        let ep_title = if !suffix.is_empty() {
            format!("S{:02}E{:02} - {}", season, episode, suffix)
        } else {
            format!("S{:02}E{:02}", season, episode)
        };

        return ParsedMedia {
            kind: "episode".to_string(),
            title: ep_title,
            year: None,
            show_name: Some(show_name),
            season: Some(season),
            episode: Some(episode),
        };
    }

    if in_tv_dir {
        let show_name = if components.len() >= 3 {
            clean_title(&components[1])
        } else {
            clean_title(stem)
        };
        let title = clean_title(stem);
        return ParsedMedia {
            kind: "episode".to_string(),
            title: if title.is_empty() { stem.to_string() } else { title },
            year: None,
            show_name: Some(if show_name.is_empty() { "TV".to_string() } else { show_name }),
            season: Some(1),
            episode: None,
        };
    }

    // Movie
    let year = re_year
        .captures(stem)
        .and_then(|c| c[1].parse::<i64>().ok())
        .or_else(|| {
            file_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .and_then(|s| re_year.captures(s))
                .and_then(|c| c[1].parse::<i64>().ok())
        });

    let mut title = clean_title(stem);
    if let Some(y) = year {
        let y_str = y.to_string();
        if let Some(idx) = title.find(&y_str) {
            let trimmed = title[..idx].trim_matches(|c: char| c == '-' || c == ' ' || c == '(');
            if !trimmed.is_empty() {
                title = trimmed.to_string();
            }
        }
    }
    if title.is_empty() {
        title = stem.to_string();
    }

    ParsedMedia {
        kind: "movie".to_string(),
        title,
        year,
        show_name: None,
        season: None,
        episode: None,
    }
}

pub fn probe_media(path: &Path) -> ProbeInfo {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output();

    let Ok(out) = output else {
        return ProbeInfo {
            container: ext,
            ..Default::default()
        };
    };
    if !out.status.success() {
        return ProbeInfo {
            container: ext,
            ..Default::default()
        };
    }

    let parsed: FfprobeOutput = match serde_json::from_slice(&out.stdout) {
        Ok(p) => p,
        Err(_) => {
            return ProbeInfo {
                container: ext,
                ..Default::default()
            };
        }
    };

    let mut info = ProbeInfo {
        container: ext,
        ..Default::default()
    };

    if let Some(fmt) = parsed.format {
        if let Some(d) = fmt.duration.and_then(|s| s.parse::<f64>().ok()) {
            info.duration_secs = d;
        }
        if info.container.is_empty() {
            info.container = fmt.format_name.unwrap_or_default();
        }
    }

    if let Some(streams) = parsed.streams {
        for s in streams {
            match s.codec_type.as_deref() {
                Some("video") if info.video_codec.is_empty() => {
                    info.video_codec = s.codec_name.unwrap_or_default();
                    info.width = s.width.unwrap_or(0);
                    info.height = s.height.unwrap_or(0);
                }
                Some("audio") if info.audio_codec.is_empty() => {
                    info.audio_codec = s.codec_name.unwrap_or_default();
                }
                Some("subtitle") => {
                    let codec = s.codec_name.unwrap_or_default();
                    let is_text = matches!(
                        codec.as_str(),
                        "subrip" | "srt" | "ass" | "ssa" | "webvtt" | "mov_text" | "text"
                    );
                    let tags = s.tags.unwrap_or_default();
                    let language = tags
                        .get("language")
                        .cloned()
                        .unwrap_or_else(|| "und".to_string());
                    let title = tags
                        .get("title")
                        .cloned()
                        .unwrap_or_else(|| format!("{} ({})", language.to_uppercase(), codec));
                    info.subtitles.push(SubtitleTrack {
                        index: s.index.unwrap_or(0),
                        language,
                        title,
                        codec,
                        is_text,
                    });
                }
                _ => {}
            }
        }
    }

    info
}

#[derive(Debug, Default, serde::Serialize)]
pub struct ScanReport {
    pub scanned: usize,
    pub updated: usize,
    pub skipped: usize,
    pub removed: usize,
}

pub fn scan_library(db: &Db, media_root: &Path) -> Result<ScanReport> {
    let mut report = ScanReport::default();
    if !media_root.exists() {
        warn!("Media root {:?} does not exist", media_root);
        return Ok(report);
    }

    let existing_stamps = db.get_all_file_stamps().unwrap_or_default();
    let mut seen_paths = HashSet::new();

    for entry in WalkDir::new(media_root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() || !is_video_file(path) {
            continue;
        }
        // Skip temp / sample files
        let path_str = path.to_string_lossy().to_string();
        if path_str.contains("/temp/") || path_str.to_ascii_lowercase().contains("sample.") {
            continue;
        }

        report.scanned += 1;
        seen_paths.insert(path_str.clone());

        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let file_size = meta.len() as i64;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        if let Some(&(old_size, old_mtime)) = existing_stamps.get(&path_str) {
            if old_size == file_size && old_mtime == mtime {
                report.skipped += 1;
                continue;
            }
        }

        let parsed = parse_media_path(media_root, path);
        let probe = probe_media(path);
        let subs_json = serde_json::to_string(&probe.subtitles).unwrap_or_else(|_| "[]".to_string());

        db.upsert_media(
            &path_str,
            &parsed.kind,
            &parsed.title,
            parsed.year,
            parsed.show_name.as_deref(),
            parsed.season,
            parsed.episode,
            file_size,
            mtime,
            probe.duration_secs,
            probe.width,
            probe.height,
            &probe.video_codec,
            &probe.audio_codec,
            &probe.container,
            &subs_json,
        )?;
        report.updated += 1;
    }

    report.removed = db.prune_missing_paths(&seen_paths)?;
    info!(
        "Library scan complete: scanned={}, updated={}, skipped={}, removed={}",
        report.scanned, report.updated, report.skipped, report.removed
    );
    Ok(report)
}
