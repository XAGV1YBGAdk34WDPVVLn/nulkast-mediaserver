use crate::diagnostics::Diagnostics;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tokio::process::Command;

#[derive(Clone)]
pub struct TorrentProxy {
    pub base_url: Option<String>,
    pub diag: Diagnostics,
}

impl TorrentProxy {
    pub fn new(base_url: Option<String>, diag: Diagnostics) -> Self {
        let cleaned = base_url
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty());
        Self {
            base_url: cleaned,
            diag,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.base_url.is_some()
    }

    pub fn sanitize_id(raw: &str) -> String {
        raw.chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect()
    }

    pub fn url_encode(s: &str) -> String {
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

    /// Normalize a movie or show title for fuzzy matching against the local MediaServer library.
    pub fn normalize_title(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut prev_space = true;
        for ch in s.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
                prev_space = false;
            } else if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        }
        out.trim().to_string()
    }

    fn require_base(&self) -> Result<&str> {
        self.base_url
            .as_deref()
            .ok_or_else(|| anyhow!("TORRENT_API_URL is not configured"))
    }

    pub async fn get_bytes(&self, path_and_query: &str, timeout_secs: u64) -> Result<(u16, Vec<u8>)> {
        let base = self.require_base()?;
        let url = format!("{}{}", base, path_and_query);
        let timeout_s = timeout_secs.to_string();
        let out = Command::new("curl")
            .args([
                "-sS",
                "--max-time",
                &timeout_s,
                "-w",
                "\n%{http_code}",
                &url,
            ])
            .output()
            .await?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!("Torrent API unreachable: {}", err.trim()));
        }

        // Split trailing "\n<http_code>"
        let bytes = out.stdout;
        let split_pos = bytes
            .iter()
            .rposition(|&b| b == b'\n')
            .ok_or_else(|| anyhow!("Malformed curl response"))?;
        let code_str = std::str::from_utf8(&bytes[split_pos + 1..]).unwrap_or("000").trim();
        let status: u16 = code_str.parse().unwrap_or(502);
        Ok((status, bytes[..split_pos].to_vec()))
    }

    pub async fn get_json(&self, path_and_query: &str, timeout_secs: u64) -> Result<Value> {
        let (status, body) = self.get_bytes(path_and_query, timeout_secs).await?;
        if !(200..300).contains(&status) {
            let snippet = String::from_utf8_lossy(&body);
            return Err(anyhow!("Torrent API HTTP {}: {}", status, snippet.trim()));
        }
        serde_json::from_slice(&body).map_err(|e| anyhow!("Invalid JSON from Torrent API: {}", e))
    }

    pub async fn post_form(&self, path: &str, params: &[(&str, &str)], timeout_secs: u64) -> Result<(u16, String)> {
        let base = self.require_base()?;
        let url = format!("{}{}", base, path);
        let encoded_body = params
            .iter()
            .map(|(k, v)| format!("{}={}", Self::url_encode(k), Self::url_encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        let timeout_s = timeout_secs.to_string();
        let out = Command::new("curl")
            .args([
                "-sS",
                "-X",
                "POST",
                "--max-time",
                &timeout_s,
                "-H",
                "Content-Type: application/x-www-form-urlencoded",
                "--data-raw",
                &encoded_body,
                "-w",
                "\n%{http_code}",
                &url,
            ])
            .output()
            .await?;

        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!("Torrent API POST failed: {}", err.trim()));
        }

        let text = String::from_utf8_lossy(&out.stdout);
        let (body, code_str) = text.rsplit_once('\n').unwrap_or((&text, "502"));
        let status: u16 = code_str.trim().parse().unwrap_or(502);
        Ok((status, body.to_string()))
    }

    /// Annotates movies array with `inLibrary` and `libraryMediaId` from local SQLite movies.
    pub fn annotate_movies(payload: &mut Value, local_movies: &[(i64, String, Option<i64>)]) {
        let mut by_title_year: HashMap<(String, i64), i64> = HashMap::new();
        let mut by_title: HashMap<String, i64> = HashMap::new();
        for (id, title, year) in local_movies {
            let norm = Self::normalize_title(title);
            if norm.is_empty() {
                continue;
            }
            if let Some(y) = year {
                by_title_year.entry((norm.clone(), *y)).or_insert(*id);
            }
            by_title.entry(norm).or_insert(*id);
        }

        if let Some(arr) = payload.get_mut("movies").and_then(|v| v.as_array_mut()) {
            for item in arr {
                let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let year = item
                    .get("year")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<i64>().ok());
                let norm = Self::normalize_title(title);
                let matched_id = year
                    .and_then(|y| by_title_year.get(&(norm.clone(), y)).copied())
                    .or_else(|| by_title.get(&norm).copied());

                if let Some(obj) = item.as_object_mut() {
                    obj.insert("inLibrary".to_string(), json!(matched_id.is_some()));
                    if let Some(mid) = matched_id {
                        obj.insert("libraryMediaId".to_string(), json!(mid));
                    }
                }
            }
        }
    }

    /// Annotates TV shows array with `inLibrary` and `libraryShowName` from local SQLite shows.
    pub fn annotate_shows(payload: &mut Value, local_shows: &[String]) {
        let mut by_norm: HashMap<String, String> = HashMap::new();
        for name in local_shows {
            let norm = Self::normalize_title(name);
            if !norm.is_empty() {
                by_norm.entry(norm).or_insert_with(|| name.clone());
            }
        }

        if let Some(arr) = payload.get_mut("shows").and_then(|v| v.as_array_mut()) {
            for item in arr {
                let title = item
                    .get("cleanTitle")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("title").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let norm = Self::normalize_title(title);
                let matched = by_norm.get(&norm).cloned();
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("inLibrary".to_string(), json!(matched.is_some()));
                    if let Some(show_name) = matched {
                        obj.insert("libraryShowName".to_string(), json!(show_name));
                    }
                }
            }
        }
    }

    /// Filter out zero-seeder and suspicious executable spam, deduplicate, and rank best matches first.
    pub fn rank_search_results(raw_results: &[Value]) -> Vec<Value> {
        let bad_ext = [".exe", ".scr", ".bat", ".cmd", ".msi", ".apk", ".lnk", ".iso", ".zipx"];
        let mut seen = HashSet::new();
        let mut scored: Vec<(i64, Value)> = Vec::new();

        for item in raw_results {
            let name = item.get("fileName").and_then(|v| v.as_str()).unwrap_or("").trim();
            let file_url = item.get("fileUrl").and_then(|v| v.as_str()).unwrap_or("").trim();
            let seeders = item.get("nbSeeders").and_then(|v| v.as_i64()).unwrap_or(0);
            let size = item.get("fileSize").and_then(|v| v.as_i64()).unwrap_or(0);

            if name.is_empty() || file_url.is_empty() || seeders <= 0 {
                continue;
            }
            let lower = name.to_ascii_lowercase();
            if bad_ext.iter().any(|e| lower.ends_with(e)) {
                continue;
            }
            if !seen.insert(file_url.to_string()) {
                continue;
            }

            let mut score = seeders.min(500) * 10;
            if lower.contains("1080p") {
                score += 400;
            } else if lower.contains("2160p") || lower.contains("4k") {
                score += 300;
            } else if lower.contains("720p") {
                score += 150;
            }
            if lower.contains("web-dl") || lower.contains("webrip") || lower.contains("bluray") {
                score += 120;
            }
            if lower.contains("cam") || lower.contains("hdcam") || lower.contains("ts ") || lower.contains("telesync") {
                score -= 600;
            }
            // Reasonable video size bonus (200 MB - 25 GB)
            if size >= 200_000_000 && size <= 25_000_000_000 {
                score += 100;
            }

            scored.push((score, item.clone()));
        }

        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().take(25).map(|(_, v)| v).collect()
    }

    /// Generates progressively simpler fallback search queries when an exact movie/TV/anime title
    /// returns 0 torrent results (handles punctuation, subtitles, year mismatches, and S01 vs Season 1).
    pub fn build_search_fallbacks(query: &str, kind: Option<&str>) -> Vec<String> {
        let orig = query.trim();
        if orig.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(orig.to_ascii_lowercase());

        let mut push_candidate = |s: String| {
            let cleaned = s.split_whitespace().collect::<Vec<_>>().join(" ");
            let key = cleaned.to_ascii_lowercase();
            if cleaned.len() >= 3 && seen.insert(key) {
                out.push(cleaned);
            }
        };

        // Helper: strip punctuation/symbols into clean alphanumeric + spaces
        let strip_punct = |s: &str| -> String {
            s.chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == ' ' { c } else { ' ' })
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };

        // Helper: strip trailing 4-digit year (19xx / 20xx)
        let strip_year = |s: &str| -> String {
            let parts: Vec<&str> = s.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Some(last) = parts.last() {
                    if last.len() == 4
                        && last.chars().all(|c| c.is_ascii_digit())
                        && (last.starts_with("19") || last.starts_with("20"))
                    {
                        return parts[..parts.len() - 1].join(" ");
                    }
                }
            }
            s.to_string()
        };

        // Helper: detect & strip trailing S01 / S02 season tag
        let split_season = |s: &str| -> (String, Option<u32>) {
            let parts: Vec<&str> = s.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Some(last) = parts.last() {
                    let up = last.to_ascii_uppercase();
                    if up.starts_with('S') && up.len() >= 2 && up[1..].chars().all(|c| c.is_ascii_digit()) {
                        if let Ok(num) = up[1..].parse::<u32>() {
                            return (parts[..parts.len() - 1].join(" "), Some(num));
                        }
                    }
                }
            }
            (s.to_string(), None)
        };

        // 1. Punctuation-normalized version (drops colons, hyphens, apostrophes)
        let norm = strip_punct(orig);
        push_candidate(norm.clone());

        // 2. For TV: swap S02 -> "Season 2", then try bare show name without season
        let (no_season_raw, season_num) = split_season(orig);
        let no_season_norm = strip_punct(&no_season_raw);
        if let Some(sn) = season_num {
            push_candidate(format!("{} Season {}", no_season_norm, sn));
            push_candidate(no_season_norm.clone());
        } else if kind == Some("tv") {
            push_candidate(strip_year(&norm));
        }

        // 3. For Movies (or anything with trailing year): drop the year
        let no_year = strip_year(&norm);
        push_candidate(no_year.clone());

        // 4. Subtitle / Anime franchise simplification:
        //    "Demon Slayer -Kimetsu no Yaiba- The Movie: Mugen Train 2020"
        //    -> "Demon Slayer Mugen Train" and "Demon Slayer"
        let base_for_split = strip_year(&no_season_raw);
        if let Some((head, tail)) = base_for_split.split_once(':') {
            let head_clean = strip_punct(
                head.split(" -")
                    .next()
                    .unwrap_or(head)
                    .split(" —")
                    .next()
                    .unwrap_or(head),
            )
            .replace("The Movie", "")
            .trim()
            .to_string();
            let tail_clean = strip_punct(tail);
            if !head_clean.is_empty() && !tail_clean.is_empty() {
                if let Some(sn) = season_num {
                    push_candidate(format!("{} S{:02}", head_clean, sn));
                }
                push_candidate(format!("{} {}", head_clean, tail_clean));
            }
            if head_clean.split_whitespace().count() >= 1 && head_clean.len() >= 4 {
                push_candidate(head_clean);
            }
            if tail_clean.split_whitespace().count() >= 2 && tail_clean.len() >= 6 {
                push_candidate(tail_clean);
            }
        }

        // 5. Long titles (5+ words): trim leading article and keep first 3-4 distinctive words
        let words: Vec<&str> = no_year.split_whitespace().collect();
        if words.len() >= 5 {
            let trimmed: Vec<&str> = if matches!(words[0].to_ascii_lowercase().as_str(), "the" | "a" | "an") {
                words[1..].to_vec()
            } else {
                words.clone()
            };
            if trimmed.len() >= 3 {
                push_candidate(trimmed[..3].join(" "));
            }
        }

        out.truncate(5);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_and_annotate_library_matches() {
        assert_eq!(TorrentProxy::normalize_title("The Odyssey (2026)"), "the odyssey 2026");
        assert_eq!(TorrentProxy::normalize_title("Dune: Part Two"), "dune part two");

        let mut movies = json!({
            "movies": [
                { "id": "1", "title": "Dune: Part Two", "year": "2024" },
                { "id": "2", "title": "Unknown Film", "year": "2026" }
            ]
        });
        TorrentProxy::annotate_movies(&mut movies, &[(42, "Dune Part Two".to_string(), Some(2024))]);
        assert_eq!(movies["movies"][0]["inLibrary"], true);
        assert_eq!(movies["movies"][0]["libraryMediaId"], 42);
        assert_eq!(movies["movies"][1]["inLibrary"], false);

        let mut shows = json!({
            "shows": [
                { "id": "10", "cleanTitle": "Lanterns" },
                { "id": "11", "cleanTitle": "Not Owned" }
            ]
        });
        TorrentProxy::annotate_shows(&mut shows, &["Lanterns".to_string()]);
        assert_eq!(shows["shows"][0]["inLibrary"], true);
        assert_eq!(shows["shows"][0]["libraryShowName"], "Lanterns");
        assert_eq!(shows["shows"][1]["inLibrary"], false);
    }

    #[test]
    fn test_rank_search_results_filters_spam_and_prefers_1080p() {
        let raw = vec![
            json!({"fileName": "Movie.2026.exe", "fileUrl": "magnet:?xt=1", "nbSeeders": 999, "fileSize": 10_000}),
            json!({"fileName": "Movie.2026.HDCAM.mp4", "fileUrl": "magnet:?xt=2", "nbSeeders": 50, "fileSize": 1_200_000_000}),
            json!({"fileName": "Movie.2026.1080p.WEB-DL.mkv", "fileUrl": "magnet:?xt=3", "nbSeeders": 40, "fileSize": 3_500_000_000_i64}),
            json!({"fileName": "Dead.Torrent.1080p.mkv", "fileUrl": "magnet:?xt=4", "nbSeeders": 0, "fileSize": 3_000_000_000_i64}),
        ];
        let ranked = TorrentProxy::rank_search_results(&raw);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0]["fileUrl"], "magnet:?xt=3");
    }

    #[test]
    fn test_build_search_fallbacks_handles_anime_movies_and_tv() {
        let fb_anime = TorrentProxy::build_search_fallbacks(
            "Demon Slayer -Kimetsu no Yaiba- The Movie: Mugen Train 2020",
            Some("movies"),
        );
        assert!(fb_anime.iter().any(|q| q == "Demon Slayer Mugen Train"));
        assert!(fb_anime.iter().any(|q| q == "Demon Slayer"));

        let fb_movie = TorrentProxy::build_search_fallbacks("Spider-Man: Brand New Day 2026", Some("movies"));
        assert!(fb_movie.iter().any(|q| q == "Spider Man Brand New Day 2026"));
        assert!(fb_movie.iter().any(|q| q == "Spider Man Brand New Day"));

        let fb_tv = TorrentProxy::build_search_fallbacks("The Apothecary Diaries S03", Some("tv"));
        assert!(fb_tv.iter().any(|q| q == "The Apothecary Diaries Season 3"));
        assert!(fb_tv.iter().any(|q| q == "The Apothecary Diaries"));
    }
}
