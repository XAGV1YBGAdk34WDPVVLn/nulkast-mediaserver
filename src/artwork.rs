use crate::db::MediaItem;
use crate::diagnostics::Diagnostics;
use anyhow::{anyhow, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{info, warn};

#[derive(Clone)]
pub struct ArtworkManager {
    pub cache_dir: PathBuf,
    pub tmdb_api_key: Option<String>,
    pub online_enabled: bool,
    pub diag: Diagnostics,
    sem: Arc<Semaphore>,
}

impl ArtworkManager {
    pub fn new(cache_dir: PathBuf, tmdb_api_key: Option<String>, online_enabled: bool, diag: Diagnostics) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        Self {
            cache_dir,
            tmdb_api_key: tmdb_api_key.filter(|k| !k.trim().is_empty()),
            online_enabled,
            diag,
            sem: Arc::new(Semaphore::new(4)),
        }
    }

    fn url_encode(s: &str) -> String {
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

    async fn curl_bytes(url: &str) -> Result<Vec<u8>> {
        let out = Command::new("curl")
            .args([
                "-sfL",
                "--max-time",
                "8",
                "-A",
                "NulKast-MediaServer/0.1",
                url,
            ])
            .output()
            .await?;
        if !out.status.success() || out.stdout.is_empty() {
            return Err(anyhow!("curl failed for {}", url));
        }
        Ok(out.stdout)
    }

    /// Checks for local poster.jpg / folder.jpg / cover.jpg next to the media file.
    fn find_local_poster(media_path: &Path) -> Option<PathBuf> {
        let dir = media_path.parent()?;
        for name in ["poster.jpg", "poster.png", "folder.jpg", "cover.jpg", "Poster.jpg"] {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }

    async fn fetch_tmdb_poster(&self, title: &str, year: Option<i64>, is_tv: bool) -> Option<String> {
        let key = self.tmdb_api_key.as_ref()?;
        let kind = if is_tv { "tv" } else { "movie" };
        let mut url = format!(
            "https://api.themoviedb.org/3/search/{}?api_key={}&query={}",
            kind,
            Self::url_encode(key),
            Self::url_encode(title)
        );
        if let Some(y) = year {
            url.push_str(&format!("&year={}", y));
        }
        let bytes = Self::curl_bytes(&url).await.ok()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let path = json["results"]
            .as_array()?
            .iter()
            .find_map(|r| r["poster_path"].as_str())?;
        Some(format!("https://image.tmdb.org/t/p/w500{}", path))
    }

    async fn fetch_tvmaze_poster(show_name: &str) -> Option<String> {
        let url = format!(
            "https://api.tvmaze.com/singlesearch/shows?q={}",
            Self::url_encode(show_name)
        );
        let bytes = Self::curl_bytes(&url).await.ok()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        json["image"]["medium"]
            .as_str()
            .or_else(|| json["image"]["original"].as_str())
            .map(|s| s.to_string())
    }

    async fn fetch_imdb_poster(title: &str, year: Option<i64>) -> Option<String> {
        let query = match year {
            Some(y) => format!("{} {}", title, y),
            None => title.to_string(),
        };
        let url = format!(
            "https://v3.sg.media-imdb.com/suggestion/x/{}.json",
            Self::url_encode(&query)
        );
        let bytes = Self::curl_bytes(&url).await.ok()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let items = json["d"].as_array()?;

        // Prefer exact year match if available, otherwise first entry with an image
        let chosen = if let Some(y) = year {
            items
                .iter()
                .find(|x| x["y"].as_i64() == Some(y) && x["i"]["imageUrl"].is_string())
                .or_else(|| items.iter().find(|x| x["i"]["imageUrl"].is_string()))
        } else {
            items.iter().find(|x| x["i"]["imageUrl"].is_string())
        }?;

        let raw = chosen["i"]["imageUrl"].as_str()?;
        // Resize Amazon CDN poster to crisp 500px width for fast Xbox/LAN loading
        let resized = if raw.ends_with("._V1_.jpg") {
            raw.replace("._V1_.jpg", "._V1_UX500_.jpg")
        } else {
            raw.to_string()
        };
        Some(resized)
    }

    async fn resolve_and_cache(
        &self,
        cache_path: &Path,
        title: &str,
        year: Option<i64>,
        is_tv: bool,
        local_media_path: Option<&Path>,
    ) -> Result<Vec<u8>> {
        if cache_path.exists() {
            return Ok(tokio::fs::read(cache_path).await?);
        }

        // 1. Check local folder poster.jpg / folder.jpg
        if let Some(mp) = local_media_path {
            if let Some(local) = Self::find_local_poster(mp) {
                if let Ok(bytes) = tokio::fs::read(&local).await {
                    let _ = tokio::fs::write(cache_path, &bytes).await;
                    return Ok(bytes);
                }
            }
        }

        if !self.online_enabled {
            return Err(anyhow!("Online artwork disabled and no local poster found"));
        }

        let _permit = self.sem.acquire().await?;
        if cache_path.exists() {
            return Ok(tokio::fs::read(cache_path).await?);
        }

        // 2. Try TMDB (if key configured) -> TVMaze (for TV) -> IMDb public suggestion
        let mut poster_url = self.fetch_tmdb_poster(title, year, is_tv).await;
        if poster_url.is_none() && is_tv {
            poster_url = Self::fetch_tvmaze_poster(title).await;
        }
        if poster_url.is_none() {
            poster_url = Self::fetch_imdb_poster(title, year).await;
        }
        // Retry without year if year-qualified query returned nothing
        if poster_url.is_none() && year.is_some() {
            poster_url = Self::fetch_imdb_poster(title, None).await;
        }

        let Some(url) = poster_url else {
            self.diag.record("warn", "artwork", format!("No official poster found for '{}'", title));
            return Err(anyhow!("No official poster found for '{}'", title));
        };

        let bytes = Self::curl_bytes(&url).await?;
        let tmp = cache_path.with_extension("tmp.jpg");
        if tokio::fs::write(&tmp, &bytes).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, cache_path).await;
        }
        info!("Cached official poster for '{}' ({} bytes)", title, bytes.len());
        self.diag.record("info", "artwork", format!("Cached poster for '{}' ({} KB)", title, bytes.len() / 1024));
        Ok(bytes)
    }

    pub async fn get_media_poster(&self, item: &MediaItem) -> Result<Vec<u8>> {
        let is_tv = item.kind == "episode";
        let lookup_title = if is_tv {
            item.show_name.as_deref().unwrap_or(&item.title)
        } else {
            &item.title
        };
        let cache_path = if is_tv {
            self.show_cache_path(lookup_title)
        } else {
            self.cache_dir.join(format!("movie_{}.jpg", item.id))
        };
        self.resolve_and_cache(
            &cache_path,
            lookup_title,
            item.year,
            is_tv,
            Some(Path::new(&item.path)),
        )
        .await
    }

    pub async fn get_show_poster(&self, show_name: &str) -> Result<Vec<u8>> {
        let cache_path = self.show_cache_path(show_name);
        match self.resolve_and_cache(&cache_path, show_name, None, true, None).await {
            Ok(b) => Ok(b),
            Err(e) => {
                warn!("Show poster miss for '{}': {}", show_name, e);
                Err(e)
            }
        }
    }

    fn show_cache_path(&self, show_name: &str) -> PathBuf {
        let mut h = DefaultHasher::new();
        show_name.to_ascii_lowercase().hash(&mut h);
        self.cache_dir.join(format!("show_{:016x}.jpg", h.finish()))
    }

    /// Resolves IMDb ID via public suggestion API (zero API key).
    async fn lookup_imdb_id(title: &str, year: Option<i64>) -> Option<String> {
        let query = match year {
            Some(y) => format!("{} {}", title, y),
            None => title.to_string(),
        };
        let url = format!(
            "https://v3.sg.media-imdb.com/suggestion/x/{}.json",
            Self::url_encode(&query)
        );
        let bytes = Self::curl_bytes(&url).await.ok()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let items = json["d"].as_array()?;
        let chosen = if let Some(y) = year {
            items
                .iter()
                .find(|x| x["y"].as_i64() == Some(y) && x["id"].as_str().map(|s| s.starts_with("tt")).unwrap_or(false))
                .or_else(|| items.iter().find(|x| x["id"].as_str().map(|s| s.starts_with("tt")).unwrap_or(false)))
        } else {
            items.iter().find(|x| x["id"].as_str().map(|s| s.starts_with("tt")).unwrap_or(false))
        }?;
        chosen["id"].as_str().map(|s| s.to_string())
    }

    /// Normalizes verbose Wikidata genre labels into crisp living-room categories.
    pub fn normalize_genres(raw: &str) -> String {
        let lower = raw.to_ascii_lowercase();
        let rules: &[(&str, &[&str])] = &[
            ("Action", &["action", "martial arts", "superhero", "ninja"]),
            ("Adventure", &["adventure"]),
            ("Animation", &["animated", "animation", "anime"]),
            ("Comedy", &["comedy", "satire", "parody"]),
            ("Crime", &["crime", "gangster", "heist", "noir"]),
            ("Documentary", &["documentary"]),
            ("Drama", &["drama", "biographical", "melodrama"]),
            ("Family", &["family", "children"]),
            ("Fantasy", &["fantasy", "magic"]),
            ("Horror", &["horror", "slasher", "zombie", "vampire", "supernatural"]),
            ("Mystery", &["mystery", "detective"]),
            ("Romance", &["romance", "romantic"]),
            ("Sci-Fi", &["science fiction", "sci-fi", "cyberpunk", "dystopian", "post-apocalyptic", "space"]),
            ("Thriller", &["thriller", "suspense", "psychological"]),
            ("War", &["war film", "military"]),
            ("Western", &["western"]),
        ];
        let mut found = Vec::new();
        for (label, needles) in rules {
            if needles.iter().any(|n| lower.contains(n)) {
                found.push(*label);
            }
        }
        found.join(", ")
    }

    /// Background batch enrichment of movie genres via IMDb IDs + Wikidata SPARQL (zero API key).
    pub async fn enrich_missing_movie_genres(&self, db: &crate::db::Db) {
        if !self.online_enabled {
            return;
        }
        let missing = match db.list_movies_missing_genres() {
            Ok(m) if !m.is_empty() => m,
            _ => return,
        };
        info!("Enriching genres for {} movies via IMDb + Wikidata...", missing.len());

        let mut id_to_imdb: Vec<(i64, String)> = Vec::new();
        for (id, title, year, existing_imdb) in missing {
            let imdb = if !existing_imdb.is_empty() {
                Some(existing_imdb)
            } else {
                let mut hit = Self::lookup_imdb_id(&title, year).await;
                if hit.is_none() && year.is_some() {
                    hit = Self::lookup_imdb_id(&title, None).await;
                }
                hit
            };
            if let Some(tt) = imdb {
                id_to_imdb.push((id, tt));
            }
        }

        let mut updated = 0usize;
        for chunk in id_to_imdb.chunks(25) {
            let values = chunk
                .iter()
                .map(|(_, tt)| format!("\"{}\"", tt))
                .collect::<Vec<_>>()
                .join(" ");
            let sparql = format!(
                "SELECT ?imdb (GROUP_CONCAT(DISTINCT ?g; separator=\", \") AS ?genres) WHERE {{ VALUES ?imdb {{ {} }} ?item wdt:P345 ?imdb . ?item wdt:P136 ?genre . ?genre rdfs:label ?g . FILTER(LANG(?g) = \"en\") }} GROUP BY ?imdb",
                values
            );
            let url = format!(
                "https://query.wikidata.org/sparql?format=json&query={}",
                Self::url_encode(&sparql)
            );
            let Ok(bytes) = Self::curl_bytes(&url).await else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(bindings) = json["results"]["bindings"].as_array() else {
                continue;
            };

            let mut genre_map = std::collections::HashMap::new();
            for b in bindings {
                if let (Some(tt), Some(raw)) = (b["imdb"]["value"].as_str(), b["genres"]["value"].as_str()) {
                    let norm = Self::normalize_genres(raw);
                    if !norm.is_empty() {
                        genre_map.insert(tt.to_string(), norm);
                    }
                }
            }

            for (id, tt) in chunk {
                if let Some(g) = genre_map.get(tt) {
                    if db.update_movie_metadata(*id, tt, g).is_ok() {
                        updated += 1;
                    }
                }
            }
        }
        info!("Movie genre enrichment complete: updated {} movies", updated);
        self.diag.record("info", "metadata", format!("Enriched genres for {} movies", updated));
    }

    /// Background enrichment of TV show genres via TVMaze (zero API key).
    pub async fn enrich_missing_show_genres(&self, db: &crate::db::Db) {
        if !self.online_enabled {
            return;
        }
        let missing = match db.list_shows_missing_genres() {
            Ok(m) if !m.is_empty() => m,
            _ => return,
        };
        info!("Enriching genres for {} TV shows via TVMaze...", missing.len());
        let mut updated = 0usize;
        for show in missing {
            let url = format!(
                "https://api.tvmaze.com/singlesearch/shows?q={}",
                Self::url_encode(&show)
            );
            let Ok(bytes) = Self::curl_bytes(&url).await else {
                continue;
            };
            let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let mut tags: Vec<String> = json["genres"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            if let Some(show_type) = json["type"].as_str() {
                if show_type.eq_ignore_ascii_case("Animation") && !tags.iter().any(|t| t.eq_ignore_ascii_case("Animation")) {
                    tags.push("Animation".to_string());
                } else if show_type.eq_ignore_ascii_case("Documentary") && !tags.iter().any(|t| t.eq_ignore_ascii_case("Documentary")) {
                    tags.push("Documentary".to_string());
                }
            }
            if !tags.is_empty() {
                let norm = Self::normalize_genres(&tags.join(", "));
                let final_genres = if norm.is_empty() { tags.join(", ") } else { norm };
                if db.update_show_genres(&show, &final_genres).is_ok() {
                    updated += 1;
                }
            }
        }
        info!("TV show genre enrichment complete: updated {} shows", updated);
        self.diag.record("info", "metadata", format!("Enriched genres for {} TV shows", updated));
    }
}
