use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub index: i64,
    pub language: String,
    pub title: String,
    pub codec: String,
    pub is_text: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaItem {
    pub id: i64,
    pub path: String,
    pub kind: String, // "movie" | "episode"
    pub title: String,
    pub year: Option<i64>,
    pub show_name: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub file_size: i64,
    pub mtime: i64,
    pub duration_secs: f64,
    pub width: i64,
    pub height: i64,
    pub video_codec: String,
    pub audio_codec: String,
    pub container: String,
    pub subtitles_json: String,
    pub genres: String,
    pub imdb_id: String,
    pub position_secs: f64,
    pub watched: bool,
    pub last_played: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowSummary {
    pub show_name: String,
    pub episode_count: i64,
    pub season_count: i64,
    pub unwatched_count: i64,
    pub latest_id: i64,
    pub latest_mtime: i64,
    pub genres: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryStats {
    pub movies: i64,
    pub episodes: i64,
    pub shows: i64,
    pub total_size_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadHistoryEntry {
    pub id: i64,
    pub title: String,
    pub category: String,
    pub torrent_count: i64,
    pub status: String,
    pub created_at: String,
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;
            PRAGMA temp_store = MEMORY;
            PRAGMA cache_size = -16000;

            CREATE TABLE IF NOT EXISTS media_items (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                path TEXT NOT NULL UNIQUE,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                year INTEGER,
                show_name TEXT,
                season INTEGER,
                episode INTEGER,
                file_size INTEGER NOT NULL DEFAULT 0,
                mtime INTEGER NOT NULL DEFAULT 0,
                duration_secs REAL NOT NULL DEFAULT 0.0,
                width INTEGER NOT NULL DEFAULT 0,
                height INTEGER NOT NULL DEFAULT 0,
                video_codec TEXT NOT NULL DEFAULT '',
                audio_codec TEXT NOT NULL DEFAULT '',
                container TEXT NOT NULL DEFAULT '',
                subtitles_json TEXT NOT NULL DEFAULT '[]',
                genres TEXT NOT NULL DEFAULT '',
                imdb_id TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS watch_progress (
                media_id INTEGER PRIMARY KEY REFERENCES media_items(id) ON DELETE CASCADE,
                position_secs REAL NOT NULL DEFAULT 0.0,
                watched INTEGER NOT NULL DEFAULT 0,
                last_played TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE TABLE IF NOT EXISTS download_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'Movies',
                torrent_count INTEGER NOT NULL DEFAULT 1,
                status TEXT NOT NULL DEFAULT 'queued',
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            );

            CREATE INDEX IF NOT EXISTS idx_media_kind ON media_items(kind);
            CREATE INDEX IF NOT EXISTS idx_media_kind_mtime ON media_items(kind, mtime DESC);
            CREATE INDEX IF NOT EXISTS idx_media_show ON media_items(show_name, season, episode);
            CREATE INDEX IF NOT EXISTS idx_progress_watched_played ON watch_progress(watched, last_played DESC);

            CREATE VIRTUAL TABLE IF NOT EXISTS media_fts USING fts5(
                title,
                show_name,
                content='media_items',
                content_rowid='id'
            );

            CREATE TRIGGER IF NOT EXISTS media_ai AFTER INSERT ON media_items BEGIN
                INSERT INTO media_fts(rowid, title, show_name) VALUES (new.id, new.title, COALESCE(new.show_name, ''));
            END;
            CREATE TRIGGER IF NOT EXISTS media_ad AFTER DELETE ON media_items BEGIN
                INSERT INTO media_fts(media_fts, rowid, title, show_name) VALUES('delete', old.id, old.title, COALESCE(old.show_name, ''));
            END;
            CREATE TRIGGER IF NOT EXISTS media_au AFTER UPDATE ON media_items BEGIN
                INSERT INTO media_fts(media_fts, rowid, title, show_name) VALUES('delete', old.id, old.title, COALESCE(old.show_name, ''));
                INSERT INTO media_fts(rowid, title, show_name) VALUES (new.id, new.title, COALESCE(new.show_name, ''));
            END;
            ",
        )?;
        // Safe schema migrations for existing databases
        let _ = conn.execute("ALTER TABLE media_items ADD COLUMN genres TEXT NOT NULL DEFAULT ''", []);
        let _ = conn.execute("ALTER TABLE media_items ADD COLUMN imdb_id TEXT NOT NULL DEFAULT ''", []);
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock_conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get_all_file_stamps(&self) -> Result<std::collections::HashMap<String, (i64, i64)>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare("SELECT path, file_size, mtime FROM media_items")?;
        let mut map = std::collections::HashMap::new();
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for r in rows.flatten() {
            map.insert(r.0, (r.1, r.2));
        }
        Ok(map)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_media(
        &self,
        path: &str,
        kind: &str,
        title: &str,
        year: Option<i64>,
        show_name: Option<&str>,
        season: Option<i64>,
        episode: Option<i64>,
        file_size: i64,
        mtime: i64,
        duration_secs: f64,
        width: i64,
        height: i64,
        video_codec: &str,
        audio_codec: &str,
        container: &str,
        subtitles_json: &str,
    ) -> Result<i64> {
        let conn = self.lock_conn();
        let id: i64 = conn.query_row(
            "INSERT INTO media_items (
                path, kind, title, year, show_name, season, episode,
                file_size, mtime, duration_secs, width, height,
                video_codec, audio_codec, container, subtitles_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ON CONFLICT(path) DO UPDATE SET
                kind=excluded.kind,
                title=excluded.title,
                year=excluded.year,
                show_name=excluded.show_name,
                season=excluded.season,
                episode=excluded.episode,
                file_size=excluded.file_size,
                mtime=excluded.mtime,
                duration_secs=excluded.duration_secs,
                width=excluded.width,
                height=excluded.height,
                video_codec=excluded.video_codec,
                audio_codec=excluded.audio_codec,
                container=excluded.container,
                subtitles_json=excluded.subtitles_json
            RETURNING id",
            params![
                path,
                kind,
                title,
                year,
                show_name,
                season,
                episode,
                file_size,
                mtime,
                duration_secs,
                width,
                height,
                video_codec,
                audio_codec,
                container,
                subtitles_json,
            ],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn prune_missing_paths(&self, existing_paths: &std::collections::HashSet<String>) -> Result<usize> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let rows: Vec<(i64, String)> = {
            let mut stmt = tx.prepare("SELECT id, path FROM media_items")?;
            let mapped = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            mapped
        };
        let mut removed = 0usize;
        for (id, path) in rows {
            if !existing_paths.contains(&path) {
                tx.execute("DELETE FROM media_items WHERE id = ?1", params![id])?;
                removed += 1;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    fn map_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MediaItem> {
        Ok(MediaItem {
            id: r.get(0)?,
            path: r.get(1)?,
            kind: r.get(2)?,
            title: r.get(3)?,
            year: r.get(4)?,
            show_name: r.get(5)?,
            season: r.get(6)?,
            episode: r.get(7)?,
            file_size: r.get(8)?,
            mtime: r.get(9)?,
            duration_secs: r.get(10)?,
            width: r.get(11)?,
            height: r.get(12)?,
            video_codec: r.get(13)?,
            audio_codec: r.get(14)?,
            container: r.get(15)?,
            subtitles_json: r.get(16)?,
            genres: r.get(17)?,
            imdb_id: r.get(18)?,
            position_secs: r.get(19)?,
            watched: r.get::<_, i64>(20)? != 0,
            last_played: r.get(21)?,
        })
    }

    const BASE_SELECT: &'static str = "
        SELECT m.id, m.path, m.kind, m.title, m.year, m.show_name, m.season, m.episode,
               m.file_size, m.mtime, m.duration_secs, m.width, m.height,
               m.video_codec, m.audio_codec, m.container, m.subtitles_json,
               m.genres, m.imdb_id,
               COALESCE(w.position_secs, 0.0), COALESCE(w.watched, 0), w.last_played
        FROM media_items m
        LEFT JOIN watch_progress w ON w.media_id = m.id
    ";

    pub fn get_media(&self, id: i64) -> Result<Option<MediaItem>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!("{} WHERE m.id = ?1", Self::BASE_SELECT);
        Ok(conn.query_row(&sql, params![id], Self::map_row).optional()?)
    }

    pub fn list_movies(&self) -> Result<Vec<MediaItem>> {
        self.list_movies_filtered("az", None)
    }

    pub fn list_movies_filtered(&self, sort: &str, genre: Option<&str>) -> Result<Vec<MediaItem>> {
        let conn = self.conn.lock().unwrap();
        let order_by = match sort {
            "added" => "m.mtime DESC, LOWER(m.title) ASC",
            "year_desc" => "COALESCE(m.year, 0) DESC, LOWER(m.title) ASC",
            "year_asc" => "CASE WHEN m.year IS NULL THEN 9999 ELSE m.year END ASC, LOWER(m.title) ASC",
            "za" => "LOWER(m.title) DESC",
            _ => "LOWER(m.title) ASC",
        };
        let genre_clean = genre.map(|g| g.trim()).filter(|g| !g.is_empty() && !g.eq_ignore_ascii_case("all"));
        let rows = if let Some(g) = genre_clean {
            let pattern = format!("%{}%", g.to_ascii_lowercase());
            let sql = format!(
                "{} WHERE m.kind = 'movie' AND LOWER(m.genres) LIKE ?1 ORDER BY {}",
                Self::BASE_SELECT, order_by
            );
            let mut stmt = conn.prepare(&sql)?;
            let mapped = stmt
                .query_map(params![pattern], Self::map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            mapped
        } else {
            let sql = format!("{} WHERE m.kind = 'movie' ORDER BY {}", Self::BASE_SELECT, order_by);
            let mut stmt = conn.prepare(&sql)?;
            let mapped = stmt
                .query_map([], Self::map_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            mapped
        };
        Ok(rows)
    }

    pub fn list_movies_missing_genres(&self) -> Result<Vec<(i64, String, Option<i64>, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, title, year, imdb_id FROM media_items WHERE kind = 'movie' AND genres = ''",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn update_movie_metadata(&self, id: i64, imdb_id: &str, genres: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE media_items SET imdb_id = ?2, genres = ?3 WHERE id = ?1",
            params![id, imdb_id, genres],
        )?;
        Ok(())
    }

    pub fn list_shows(&self) -> Result<Vec<ShowSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT COALESCE(m.show_name, m.title) as sname,
                    COUNT(*) as ep_count,
                    COUNT(DISTINCT COALESCE(m.season, 1)) as s_count,
                    SUM(CASE WHEN COALESCE(w.watched, 0) = 0 THEN 1 ELSE 0 END) as unwatched,
                    MAX(m.id) as latest_id,
                    MAX(m.mtime) as latest_mtime,
                    MAX(m.genres) as genres
             FROM media_items m
             LEFT JOIN watch_progress w ON w.media_id = m.id
             WHERE m.kind = 'episode'
             GROUP BY LOWER(sname)
             ORDER BY LOWER(sname) ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ShowSummary {
                    show_name: r.get(0)?,
                    episode_count: r.get(1)?,
                    season_count: r.get(2)?,
                    unwatched_count: r.get(3)?,
                    latest_id: r.get(4)?,
                    latest_mtime: r.get(5)?,
                    genres: r.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_shows_missing_genres(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT COALESCE(show_name, title) FROM media_items
             WHERE kind = 'episode' AND genres = ''",
        )?;
        let rows = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn update_show_genres(&self, show_name: &str, genres: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE media_items SET genres = ?2
             WHERE kind = 'episode' AND LOWER(COALESCE(show_name, title)) = LOWER(?1)",
            params![show_name, genres],
        )?;
        Ok(())
    }

    pub fn list_show_episodes(&self, show_name: &str) -> Result<Vec<MediaItem>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "{} WHERE m.kind = 'episode' AND LOWER(COALESCE(m.show_name, m.title)) = LOWER(?1)
             ORDER BY COALESCE(m.season, 1) ASC, COALESCE(m.episode, 0) ASC, LOWER(m.title) ASC",
            Self::BASE_SELECT
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![show_name], Self::map_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_continue_watching(&self, limit: i64) -> Result<Vec<MediaItem>> {
        let conn = self.conn.lock().unwrap();
        let sql = format!(
            "{} WHERE w.position_secs > 10.0 AND COALESCE(w.watched, 0) = 0
             ORDER BY w.last_played DESC LIMIT ?1",
            Self::BASE_SELECT
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![limit], Self::map_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<MediaItem>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.conn.lock().unwrap();
        let fts_query = trimmed
            .split_whitespace()
            .map(|tok| {
                let clean: String = tok.chars().filter(|c| c.is_alphanumeric()).collect();
                format!("{}*", clean)
            })
            .filter(|s| s.len() > 1)
            .collect::<Vec<_>>()
            .join(" ");

        if fts_query.is_empty() {
            return Ok(vec![]);
        }

        let sql = format!(
            "{} INNER JOIN media_fts f ON f.rowid = m.id
             WHERE media_fts MATCH ?1
             ORDER BY bm25(media_fts) ASC LIMIT ?2",
            Self::BASE_SELECT
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![fts_query, limit], Self::map_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn update_progress(&self, media_id: i64, position_secs: f64, watched: Option<bool>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let duration: f64 = conn
            .query_row(
                "SELECT duration_secs FROM media_items WHERE id = ?1",
                params![media_id],
                |r| r.get(0),
            )
            .unwrap_or(0.0);
        let is_watched = watched.unwrap_or_else(|| duration > 30.0 && position_secs >= duration * 0.92);
        conn.execute(
            "INSERT INTO watch_progress (media_id, position_secs, watched, last_played)
             VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP)
             ON CONFLICT(media_id) DO UPDATE SET
                position_secs = excluded.position_secs,
                watched = excluded.watched,
                last_played = CURRENT_TIMESTAMP",
            params![media_id, position_secs.max(0.0), if is_watched { 1 } else { 0 }],
        )?;
        Ok(())
    }

    pub fn record_download(&self, title: &str, category: &str, torrent_count: i64, status: &str) -> Result<()> {
        let conn = self.lock_conn();
        conn.execute(
            "INSERT INTO download_history (title, category, torrent_count, status) VALUES (?1, ?2, ?3, ?4)",
            params![title, category, torrent_count, status],
        )?;
        Ok(())
    }

    pub fn list_download_history(&self, limit: i64) -> Result<Vec<DownloadHistoryEntry>> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, category, torrent_count, status, created_at
             FROM download_history
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(DownloadHistoryEntry {
                id: r.get(0)?,
                title: r.get(1)?,
                category: r.get(2)?,
                torrent_count: r.get(3)?,
                status: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn stats(&self) -> Result<LibraryStats> {
        let conn = self.conn.lock().unwrap();
        let movies: i64 = conn.query_row(
            "SELECT COUNT(*) FROM media_items WHERE kind = 'movie'",
            [],
            |r| r.get(0),
        )?;
        let episodes: i64 = conn.query_row(
            "SELECT COUNT(*) FROM media_items WHERE kind = 'episode'",
            [],
            |r| r.get(0),
        )?;
        let shows: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT LOWER(COALESCE(show_name, title))) FROM media_items WHERE kind = 'episode'",
            [],
            |r| r.get(0),
        )?;
        let total_size_bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(file_size), 0) FROM media_items",
            [],
            |r| r.get(0),
        )?;
        Ok(LibraryStats {
            movies,
            episodes,
            shows,
            total_size_bytes,
        })
    }
}
