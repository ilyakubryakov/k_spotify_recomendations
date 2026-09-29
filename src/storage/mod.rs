//! Persistent cache.
//!
//! `rusqlite` is synchronous, so every public method here is `async` and
//! wraps its work in `spawn_blocking`. The connection lives behind a
//! `Mutex` inside an `Arc`: SQLite serialises writers anyway, and the cost of
//! the mutex is irrelevant next to the I/O it guards.
//!
//! Writes that touch several tables run inside a transaction, so an
//! interrupted sync can never leave `tracks` populated but `track_artists`
//! empty.

pub mod schema;

use crate::domain::{Artist, ArtistRef, PlayEvent, TimeRange, Track};
use crate::error::{AgentError, Result};
use crate::util::text::{Script, detect_track_script};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// A track plus everything the analyser needs to score it.
#[derive(Debug, Clone)]
pub struct TrackStat {
    pub track: Track,
    pub saved: bool,
    pub plays: u32,
    pub recent_plays: u32,
    /// Folded `/me/top/tracks` weight across the three windows.
    pub top_score: f32,
    pub script: Script,
}

#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    pub tracks: u64,
    pub artists: u64,
    pub saved: u64,
    pub plays: u64,
    pub distinct_played: u64,
    pub recommendations: u64,
    pub runs: u64,
    pub db_bytes: u64,
    pub last_sync: Option<DateTime<Utc>>,
    pub oldest_play: Option<DateTime<Utc>>,
    pub newest_play: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct RunSummaryRow {
    pub id: i64,
    pub preset: String,
    pub started_at: DateTime<Utc>,
    pub status: String,
    pub written: u32,
    pub playlist_id: Option<String>,
}

/// One row of the recommendation memory, for `spotify-agent recommended`.
#[derive(Debug, Clone)]
pub struct RecommendationRow {
    pub artist: String,
    pub title: String,
    pub preset: String,
    pub mood: String,
    pub reason: String,
    pub accepted: bool,
    pub reject_note: Option<String>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Feedback
// ---------------------------------------------------------------------------

/// How a recommendation landed.
///
/// Spotify exposes no skip event, so `Skipped` is *inferred* from the play
/// history: a track whose next play began before it could plausibly have
/// finished was cut short. That inference is good but not certain, which is
/// why it carries its own configurable weight, separate from the explicit
/// signals a user gives in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// Added to Liked Songs after we recommended it. The strongest positive.
    Liked,
    /// Played through (or near enough).
    Played,
    /// Inferred skip — see the note above.
    Skipped,
    /// Disappeared from the managed playlist between runs.
    Removed,
    /// Sat in the playlist long enough to go stale without ever being played.
    Stale,
    /// Explicit thumbs up in the TUI.
    Up,
    /// Explicit thumbs down in the TUI.
    Down,
    /// The artist was banned outright.
    Banned,
}

impl Signal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Liked => "liked",
            Self::Played => "played",
            Self::Skipped => "skipped",
            Self::Removed => "removed",
            Self::Stale => "stale",
            Self::Up => "up",
            Self::Down => "down",
            Self::Banned => "banned",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "liked" => Self::Liked,
            "played" => Self::Played,
            "skipped" => Self::Skipped,
            "removed" => Self::Removed,
            "stale" => Self::Stale,
            "up" => Self::Up,
            "down" => Self::Down,
            "banned" => Self::Banned,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct FeedbackEntry {
    pub track_id: String,
    pub signal: Signal,
    pub weight: f32,
    /// `auto` | `tui` | `cli`.
    pub source: &'static str,
    pub note: Option<String>,
    /// Stable identity for this observation, so re-running a sync over the
    /// same history cannot double-count it.
    pub dedupe_key: String,
}

/// Accumulated opinion about a track and its artists.
#[derive(Debug, Clone, Default)]
pub struct FeedbackScores {
    pub by_track: HashMap<String, f32>,
    /// Keyed by artist id when known, and additionally by normalised name so
    /// unhydrated artists still score.
    pub by_artist: HashMap<String, f32>,
}

/// A play observed in the local history, with enough context to judge a skip.
#[derive(Debug, Clone)]
pub struct PlayWindow {
    pub track_id: String,
    pub played_at_ms: i64,
    pub duration_ms: u32,
    /// Start of the next play in the history, if any.
    pub next_played_at_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct BannedArtist {
    pub name: String,
    pub name_norm: String,
    pub artist_id: Option<String>,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Playlist lifecycle
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub id: i64,
    pub playlist_id: String,
    pub playlist_name: String,
    pub reason: String,
    pub track_count: u32,
    pub taken_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct SnapshotTrack {
    pub position: u32,
    pub track_id: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub duration_ms: u32,
}

#[derive(Debug, Clone)]
pub struct PlaylistMember {
    pub track_id: String,
    pub added_at: DateTime<Utc>,
}

/// Recorded outcome for one suggestion.
#[derive(Debug, Clone)]
pub struct RecommendationRecord {
    pub track_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub reason: String,
    pub mood: String,
    pub accepted: bool,
    pub reject_note: Option<String>,
}

#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
    path: std::path::PathBuf,
}

impl Storage {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            crate::config::paths::ensure_dir(parent)?;
        }
        let owned = path.to_path_buf();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let conn = Connection::open(&owned)?;
            schema::migrate(&conn)?;
            Ok(conn)
        })
        .await
        .map_err(join_err)??;

        crate::util::fs::harden_file(path).ok();

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: path.to_path_buf(),
        })
    }

    /// Run `f` on the connection off the async runtime's worker threads.
    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            // A poisoned mutex means another thread panicked mid-write. The
            // data is still consistent (SQLite transactions are atomic), so
            // recovering the guard is correct and avoids turning one panic
            // into a permanently dead cache.
            let mut guard = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&mut guard)
        })
        .await
        .map_err(join_err)?
    }

    // -----------------------------------------------------------------
    // Writes
    // -----------------------------------------------------------------

    pub async fn upsert_tracks(&self, tracks: Vec<Track>) -> Result<usize> {
        if tracks.is_empty() {
            return Ok(0);
        }
        self.with_conn(move |conn| {
            let now = now_ms();
            let tx = conn.transaction()?;
            {
                let mut track_stmt = tx.prepare_cached(
                    "INSERT INTO tracks
                        (id, name, artist_line, album, duration_ms, popularity, explicit,
                         release_year, isrc, script, first_seen, last_seen)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)
                     ON CONFLICT(id) DO UPDATE SET
                        name        = excluded.name,
                        artist_line = excluded.artist_line,
                        album       = excluded.album,
                        popularity  = excluded.popularity,
                        release_year= COALESCE(excluded.release_year, tracks.release_year),
                        isrc        = COALESCE(excluded.isrc, tracks.isrc),
                        last_seen   = excluded.last_seen",
                )?;
                let mut link_stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO track_artists (track_id, artist_id, position)
                     VALUES (?1, ?2, ?3)",
                )?;

                for track in &tracks {
                    let script = detect_track_script(&track.name, &track.artist_names_raw());
                    track_stmt.execute(params![
                        track.id,
                        track.name,
                        track.artist_line(),
                        track.album,
                        track.duration_ms,
                        track.popularity,
                        track.explicit as i32,
                        track.release_year,
                        track.isrc,
                        script_to_str(script),
                        now,
                    ])?;
                    for (position, artist) in track.artists.iter().enumerate() {
                        link_stmt.execute(params![track.id, artist.id, position as i64])?;
                    }
                }
            }
            tx.commit()?;
            Ok(tracks.len())
        })
        .await
    }

    pub async fn upsert_artists(&self, artists: Vec<Artist>) -> Result<usize> {
        if artists.is_empty() {
            return Ok(0);
        }
        self.with_conn(move |conn| {
            let now = now_ms();
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO artists (id, name, genres, popularity, updated_at)
                     VALUES (?1,?2,?3,?4,?5)
                     ON CONFLICT(id) DO UPDATE SET
                        name = excluded.name,
                        genres = excluded.genres,
                        popularity = excluded.popularity,
                        updated_at = excluded.updated_at",
                )?;
                for artist in &artists {
                    let genres =
                        serde_json::to_string(&artist.genres).unwrap_or_else(|_| "[]".into());
                    stmt.execute(params![
                        artist.id,
                        artist.name,
                        genres,
                        artist.popularity,
                        now
                    ])?;
                }
            }
            tx.commit()?;
            Ok(artists.len())
        })
        .await
    }

    /// Replace the Liked Songs mirror wholesale, so un-liked tracks disappear.
    pub async fn replace_saved(&self, track_ids: Vec<String>) -> Result<usize> {
        self.with_conn(move |conn| {
            let now = now_ms();
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM saved_tracks", [])?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO saved_tracks (track_id, synced_at) VALUES (?1, ?2)",
                )?;
                for id in &track_ids {
                    stmt.execute(params![id, now])?;
                }
            }
            tx.commit()?;
            Ok(track_ids.len())
        })
        .await
    }

    pub async fn replace_top(
        &self,
        kind: &'static str,
        range: TimeRange,
        ids: Vec<String>,
    ) -> Result<usize> {
        self.with_conn(move |conn| {
            let now = now_ms();
            let range_key = range.as_api();
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM top_items WHERE kind = ?1 AND time_range = ?2",
                params![kind, range_key],
            )?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR REPLACE INTO top_items (kind, time_range, item_id, rank, captured_at)
                     VALUES (?1,?2,?3,?4,?5)",
                )?;
                for (rank, id) in ids.iter().enumerate() {
                    stmt.execute(params![kind, range_key, id, rank as i64, now])?;
                }
            }
            tx.commit()?;
            Ok(ids.len())
        })
        .await
    }

    /// Insert play events, ignoring ones already recorded. Returns how many
    /// were new — the useful number for "did this sync learn anything".
    pub async fn insert_plays(&self, events: Vec<PlayEvent>) -> Result<usize> {
        if events.is_empty() {
            return Ok(0);
        }
        self.with_conn(move |conn| {
            let tx = conn.transaction()?;
            let mut inserted = 0usize;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR IGNORE INTO plays (track_id, played_at) VALUES (?1, ?2)",
                )?;
                for event in &events {
                    inserted +=
                        stmt.execute(params![event.track_id, event.played_at.timestamp_millis()])?;
                }
            }
            tx.commit()?;
            Ok(inserted)
        })
        .await
    }

    pub async fn newest_play_ms(&self) -> Result<Option<i64>> {
        self.with_conn(|conn| {
            let value: Option<i64> = conn
                .query_row("SELECT MAX(played_at) FROM plays", [], |row| row.get(0))
                .optional()?
                .flatten();
            Ok(value)
        })
        .await
    }

    // -----------------------------------------------------------------
    // Runs & recommendations
    // -----------------------------------------------------------------

    pub async fn start_run(&self, preset: String, requested: u32, model: String) -> Result<i64> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO runs (preset, started_at, requested, status, model)
                 VALUES (?1, ?2, ?3, 'running', ?4)",
                params![preset, now_ms(), requested, model],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn finish_run(
        &self,
        run_id: i64,
        status: &'static str,
        suggested: u32,
        resolved: u32,
        written: u32,
        playlist_id: Option<String>,
        error: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE runs SET finished_at = ?1, status = ?2, suggested = ?3, resolved = ?4,
                                 written = ?5, playlist_id = ?6, error = ?7,
                                 input_tokens = ?8, output_tokens = ?9
                 WHERE id = ?10",
                params![
                    now_ms(),
                    status,
                    suggested,
                    resolved,
                    written,
                    playlist_id,
                    error,
                    input_tokens as i64,
                    output_tokens as i64,
                    run_id
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn record_recommendations(
        &self,
        run_id: i64,
        preset: String,
        playlist_id: Option<String>,
        records: Vec<RecommendationRecord>,
    ) -> Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        self.with_conn(move |conn| {
            let now = now_ms();
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO recommendations
                        (run_id, track_id, title, artist, reason, mood, preset, playlist_id,
                         accepted, reject_note, created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                )?;
                for r in &records {
                    stmt.execute(params![
                        run_id,
                        r.track_id,
                        r.title,
                        r.artist,
                        r.reason,
                        r.mood,
                        preset,
                        playlist_id,
                        r.accepted as i32,
                        r.reject_note,
                        now
                    ])?;
                }
            }
            tx.commit()?;
            Ok(records.len())
        })
        .await
    }

    pub async fn recent_runs(&self, limit: usize) -> Result<Vec<RunSummaryRow>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, preset, started_at, status, written, playlist_id
                 FROM runs ORDER BY started_at DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit as i64], |row| {
                    Ok(RunSummaryRow {
                        id: row.get(0)?,
                        preset: row.get(1)?,
                        started_at: from_ms(row.get::<_, i64>(2)?),
                        status: row.get(3)?,
                        written: row.get::<_, i64>(4)? as u32,
                        playlist_id: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    // -----------------------------------------------------------------
    // Exclusions
    // -----------------------------------------------------------------

    /// Track ids the resolver must drop.
    pub async fn excluded_track_ids(
        &self,
        recent_days: u32,
        include_saved: bool,
        include_played: bool,
    ) -> Result<HashSet<String>> {
        self.with_conn(move |conn| {
            let mut set = HashSet::new();
            let cutoff = cutoff_ms(recent_days);

            let mut stmt = conn.prepare(
                "SELECT track_id FROM recommendations
                 WHERE track_id IS NOT NULL AND accepted = 1 AND created_at >= ?1",
            )?;
            for id in stmt.query_map(params![cutoff], |row| row.get::<_, String>(0))? {
                set.insert(id?);
            }

            if include_saved {
                let mut stmt = conn.prepare("SELECT track_id FROM saved_tracks")?;
                for id in stmt.query_map([], |row| row.get::<_, String>(0))? {
                    set.insert(id?);
                }
            }

            if include_played {
                let mut stmt = conn.prepare("SELECT DISTINCT track_id FROM plays")?;
                for id in stmt.query_map([], |row| row.get::<_, String>(0))? {
                    set.insert(id?);
                }
            }

            Ok(set)
        })
        .await
    }

    /// `Artist — Title` lines for the prompt's EXCLUSIONS block, newest first.
    ///
    /// Drawn from both prior recommendations and the listener's own library:
    /// the model should not suggest something already saved any more than
    /// something already recommended.
    pub async fn exclusion_lines(
        &self,
        recent_days: u32,
        include_saved: bool,
        limit: usize,
    ) -> Result<Vec<String>> {
        self.with_conn(move |conn| {
            let cutoff = cutoff_ms(recent_days);
            let mut lines = Vec::with_capacity(limit);
            let mut seen = HashSet::new();

            let mut stmt = conn.prepare(
                "SELECT artist, title FROM recommendations
                 WHERE created_at >= ?1
                 ORDER BY created_at DESC LIMIT ?2",
            )?;
            for row in stmt.query_map(params![cutoff, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (artist, title) = row?;
                let line = format!("{artist} — {title}");
                if seen.insert(line.to_lowercase()) {
                    lines.push(line);
                }
            }

            if include_saved && lines.len() < limit {
                let remaining = limit - lines.len();
                let mut stmt = conn.prepare(
                    "SELECT t.artist_line, t.name FROM saved_tracks s
                     JOIN tracks t ON t.id = s.track_id
                     ORDER BY t.popularity DESC LIMIT ?1",
                )?;
                for row in stmt.query_map(params![remaining as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })? {
                    let (artist, title) = row?;
                    let line = format!("{artist} — {title}");
                    if seen.insert(line.to_lowercase()) {
                        lines.push(line);
                    }
                }
            }

            Ok(lines)
        })
        .await
    }

    /// Artists used by an accepted recommendation within `days`.
    ///
    /// Returns both Spotify artist ids and normalised artist names, because a
    /// track resolved before the artist rows were hydrated has no usable id —
    /// and a cooldown that silently does nothing is worse than none.
    pub async fn artists_on_cooldown(&self, days: u32) -> Result<HashSet<String>> {
        if days == 0 {
            return Ok(HashSet::new());
        }
        self.with_conn(move |conn| {
            let cutoff = cutoff_ms(days);
            let mut set = HashSet::new();

            let mut stmt = conn.prepare(
                "SELECT DISTINCT ta.artist_id
                 FROM recommendations r
                 JOIN track_artists ta ON ta.track_id = r.track_id
                 WHERE r.accepted = 1 AND r.created_at >= ?1",
            )?;
            for id in stmt.query_map(params![cutoff], |row| row.get::<_, String>(0))? {
                let id = id?;
                if !id.is_empty() {
                    set.insert(id);
                }
            }

            // The model's own spelling, normalised — covers suggestions that
            // never resolved and artists we never hydrated.
            let mut stmt = conn.prepare(
                "SELECT DISTINCT artist FROM recommendations
                 WHERE accepted = 1 AND created_at >= ?1",
            )?;
            for name in stmt.query_map(params![cutoff], |row| row.get::<_, String>(0))? {
                set.insert(crate::util::text::normalize(&name?));
            }

            Ok(set)
        })
        .await
    }

    /// Distinct artist names recommended within `days`, newest first.
    ///
    /// Sent to the model as a do-not-use list so it spends its slots on
    /// something new instead of having picks silently dropped downstream.
    pub async fn recent_recommended_artists(&self, days: u32, limit: usize) -> Result<Vec<String>> {
        if days == 0 || limit == 0 {
            return Ok(Vec::new());
        }
        self.with_conn(move |conn| {
            let cutoff = cutoff_ms(days);
            let mut stmt = conn.prepare(
                "SELECT artist, MAX(created_at) AS last_used
                 FROM recommendations
                 WHERE accepted = 1 AND created_at >= ?1
                 GROUP BY LOWER(artist)
                 ORDER BY last_used DESC
                 LIMIT ?2",
            )?;
            let names = stmt
                .query_map(params![cutoff, limit as i64], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(names)
        })
        .await
    }

    // -----------------------------------------------------------------
    // Analysis input
    // -----------------------------------------------------------------

    /// Everything the analyser needs, in one pass.
    pub async fn track_stats(&self) -> Result<Vec<TrackStat>> {
        self.with_conn(|conn| {
            let recent_cutoff = now_ms() - 30 * 86_400_000;

            let mut stmt = conn.prepare(
                "SELECT
                    t.id, t.name, t.artist_line, t.album, t.duration_ms, t.popularity,
                    t.explicit, t.release_year, t.isrc, t.script,
                    EXISTS(SELECT 1 FROM saved_tracks s WHERE s.track_id = t.id) AS saved,
                    (SELECT COUNT(*) FROM plays p WHERE p.track_id = t.id) AS plays,
                    (SELECT COUNT(*) FROM plays p WHERE p.track_id = t.id AND p.played_at >= ?1) AS recent_plays,
                    COALESCE((
                        SELECT SUM(
                            CASE ti.time_range
                                WHEN 'short_term'  THEN 3.0
                                WHEN 'medium_term' THEN 2.0
                                ELSE 1.0
                            END * (1.0 - (ti.rank * 1.0 / 50.0))
                        )
                        FROM top_items ti
                        WHERE ti.kind = 'track' AND ti.item_id = t.id
                    ), 0.0) AS top_score
                 FROM tracks t",
            )?;

            let rows = stmt.query_map(params![recent_cutoff], |row| {
                let id: String = row.get(0)?;
                Ok(TrackStat {
                    track: Track {
                        id: id.clone(),
                        name: row.get(1)?,
                        // Filled in below from `track_artists`; the joined
                        // form would multiply rows and complicate the counts.
                        artists: Vec::new(),
                        album: row.get(3)?,
                        duration_ms: row.get::<_, i64>(4)? as u32,
                        popularity: row.get::<_, i64>(5)? as u8,
                        explicit: row.get::<_, i64>(6)? != 0,
                        release_year: row.get(7)?,
                        isrc: row.get(8)?,
                    },
                    script: script_from_str(&row.get::<_, String>(9)?),
                    saved: row.get::<_, i64>(10)? != 0,
                    plays: row.get::<_, i64>(11)? as u32,
                    recent_plays: row.get::<_, i64>(12)? as u32,
                    top_score: row.get::<_, f64>(13)? as f32,
                })
            })?;

            let mut stats: Vec<TrackStat> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

            // Second pass for the artist links, keyed into a map so this stays
            // O(n) instead of one query per track.
            let mut links: HashMap<String, Vec<ArtistRef>> = HashMap::new();
            let mut stmt = conn.prepare(
                "SELECT ta.track_id, ta.artist_id, COALESCE(a.name, '')
                 FROM track_artists ta
                 LEFT JOIN artists a ON a.id = ta.artist_id
                 ORDER BY ta.track_id, ta.position",
            )?;
            for row in stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })? {
                let (track_id, artist_id, name) = row?;
                links.entry(track_id).or_default().push(ArtistRef { id: artist_id, name });
            }

            // Fall back to the denormalised artist_line when the artist rows
            // have not been hydrated yet (first sync, before /artists runs).
            let mut fallback_names: HashMap<String, String> = HashMap::new();
            let mut stmt = conn.prepare("SELECT id, artist_line FROM tracks")?;
            for row in stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })? {
                let (id, line) = row?;
                fallback_names.insert(id, line);
            }

            for stat in &mut stats {
                if let Some(refs) = links.remove(&stat.track.id) {
                    stat.track.artists = refs
                        .into_iter()
                        .map(|mut r| {
                            if r.name.is_empty() {
                                r.name = fallback_names
                                    .get(&stat.track.id)
                                    .cloned()
                                    .unwrap_or_else(|| "Unknown Artist".into());
                            }
                            r
                        })
                        .collect();
                }
                if stat.track.artists.is_empty()
                    && let Some(line) = fallback_names.get(&stat.track.id) {
                        stat.track.artists = vec![ArtistRef { id: String::new(), name: line.clone() }];
                    }
            }

            Ok(stats)
        })
        .await
    }

    pub async fn artists_map(&self) -> Result<HashMap<String, Artist>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id, name, genres, popularity FROM artists")?;
            let mut map = HashMap::new();
            for row in stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })? {
                let (id, name, genres, popularity) = row?;
                map.insert(
                    id.clone(),
                    Artist {
                        id,
                        name,
                        genres: serde_json::from_str(&genres).unwrap_or_default(),
                        popularity: popularity as u8,
                    },
                );
            }
            Ok(map)
        })
        .await
    }

    /// Artist ids referenced by tracks but not yet hydrated with genres.
    pub async fn artists_missing_genres(&self) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT ta.artist_id
                 FROM track_artists ta
                 LEFT JOIN artists a ON a.id = ta.artist_id
                 WHERE ta.artist_id <> '' AND (a.id IS NULL OR a.genres = '[]')",
            )?;
            let ids = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ids)
        })
        .await
    }

    /// Weighted top-artist scores from `/me/top/artists`, keyed by artist id.
    pub async fn top_artist_scores(&self) -> Result<HashMap<String, f32>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT item_id, time_range, rank FROM top_items WHERE kind = 'artist'")?;
            let mut map: HashMap<String, f32> = HashMap::new();
            for row in stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })? {
                let (id, range, rank) = row?;
                let weight = TimeRange::parse(&range)
                    .map(TimeRange::weight)
                    .unwrap_or(1.0);
                let positional = 1.0 - (rank as f32 / 50.0);
                *map.entry(id).or_insert(0.0) += weight * positional.max(0.0);
            }
            Ok(map)
        })
        .await
    }

    /// Recommendation memory, newest first.
    pub async fn recommendations(
        &self,
        limit: usize,
        preset: Option<String>,
        accepted_only: bool,
    ) -> Result<Vec<RecommendationRow>> {
        self.with_conn(move |conn| {
            let mut sql = String::from(
                "SELECT artist, title, preset, mood, reason, accepted, reject_note, created_at
                 FROM recommendations WHERE 1 = 1",
            );
            if accepted_only {
                sql.push_str(" AND accepted = 1");
            }
            if preset.is_some() {
                sql.push_str(" AND preset = ?2");
            }
            sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?1");

            let mut stmt = conn.prepare(&sql)?;
            let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<RecommendationRow> {
                Ok(RecommendationRow {
                    artist: row.get(0)?,
                    title: row.get(1)?,
                    preset: row.get(2)?,
                    mood: row.get(3)?,
                    reason: row.get(4)?,
                    accepted: row.get::<_, i64>(5)? != 0,
                    reject_note: row.get(6)?,
                    created_at: from_ms(row.get::<_, i64>(7)?),
                })
            };

            let rows = match &preset {
                Some(p) => stmt
                    .query_map(params![limit as i64, p], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => stmt
                    .query_map(params![limit as i64], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            };
            Ok(rows)
        })
        .await
    }

    /// Drop remembered recommendations for one artist so they become eligible
    /// again. Matching is case-insensitive on the stored artist spelling.
    pub async fn forget_artist(&self, artist: String) -> Result<usize> {
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "DELETE FROM recommendations WHERE LOWER(artist) = LOWER(?1)",
                params![artist],
            )?)
        })
        .await
    }

    /// Drop the entire recommendation memory. The library mirror and play
    /// history are untouched — this only makes the agent willing to repeat
    /// itself.
    pub async fn forget_all_recommendations(&self) -> Result<usize> {
        self.with_conn(|conn| Ok(conn.execute("DELETE FROM recommendations", [])?))
            .await
    }

    // -----------------------------------------------------------------
    // Feedback
    // -----------------------------------------------------------------

    /// Record signals, ignoring ones already seen. Returns how many were new.
    pub async fn record_feedback(&self, entries: Vec<FeedbackEntry>) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        self.with_conn(move |conn| {
            let now = now_ms();
            let tx = conn.transaction()?;
            let mut inserted = 0usize;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR IGNORE INTO feedback
                        (track_id, signal, weight, source, note, dedupe_key, created_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                )?;
                for entry in &entries {
                    inserted += stmt.execute(params![
                        entry.track_id,
                        entry.signal.as_str(),
                        entry.weight,
                        entry.source,
                        entry.note,
                        entry.dedupe_key,
                        now,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(inserted)
        })
        .await
    }

    /// Fold every signal into per-track and per-artist scores.
    ///
    /// Signals decay: an opinion from a year ago should not outweigh one from
    /// last week. `half_life_days` of 0 disables decay.
    pub async fn feedback_scores(&self, half_life_days: f32) -> Result<FeedbackScores> {
        self.with_conn(move |conn| {
            let now = now_ms();
            let mut scores = FeedbackScores::default();

            let mut stmt = conn.prepare(
                "SELECT track_id, SUM(weight) AS total, MAX(created_at) AS latest,
                        AVG(created_at) AS avg_at
                 FROM feedback GROUP BY track_id",
            )?;
            for row in stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)? as f32,
                    row.get::<_, f64>(3)? as i64,
                ))
            })? {
                let (track_id, total, avg_at) = row?;
                let weight = decay(total, now - avg_at, half_life_days);
                scores.by_track.insert(track_id, weight);
            }

            // Project the track scores onto artists, by id and by name.
            let mut stmt = conn.prepare(
                "SELECT f.track_id, SUM(f.weight) AS total, AVG(f.created_at) AS avg_at,
                        COALESCE(ta.artist_id, ''), COALESCE(t.artist_line, '')
                 FROM feedback f
                 LEFT JOIN track_artists ta ON ta.track_id = f.track_id
                 LEFT JOIN tracks t ON t.id = f.track_id
                 GROUP BY f.track_id, ta.artist_id",
            )?;
            for row in stmt.query_map([], |row| {
                Ok((
                    row.get::<_, f64>(1)? as f32,
                    row.get::<_, f64>(2)? as i64,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })? {
                let (total, avg_at, artist_id, artist_line) = row?;
                let weight = decay(total, now - avg_at, half_life_days);
                if !artist_id.is_empty() {
                    *scores.by_artist.entry(artist_id).or_insert(0.0) += weight;
                }
                if !artist_line.is_empty() {
                    let key = crate::util::text::normalize(&artist_line);
                    *scores.by_artist.entry(key).or_insert(0.0) += weight;
                }
            }

            Ok(scores)
        })
        .await
    }

    /// Plays since `since_ms`, each paired with the start of the next play, so
    /// a caller can tell whether the track was allowed to finish.
    pub async fn play_windows(&self, since_ms: i64) -> Result<Vec<PlayWindow>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT p.track_id, p.played_at, COALESCE(t.duration_ms, 0),
                        (SELECT MIN(p2.played_at) FROM plays p2 WHERE p2.played_at > p.played_at)
                 FROM plays p
                 LEFT JOIN tracks t ON t.id = p.track_id
                 WHERE p.played_at >= ?1
                 ORDER BY p.played_at",
            )?;
            let rows = stmt
                .query_map(params![since_ms], |row| {
                    Ok(PlayWindow {
                        track_id: row.get(0)?,
                        played_at_ms: row.get(1)?,
                        duration_ms: row.get::<_, i64>(2)? as u32,
                        next_played_at_ms: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Track ids we recommended and accepted, with when we did so.
    pub async fn accepted_recommendations(&self) -> Result<Vec<(String, i64)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT track_id, MIN(created_at) FROM recommendations
                 WHERE accepted = 1 AND track_id IS NOT NULL
                 GROUP BY track_id",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Every track with at least one recorded play.
    pub async fn played_track_ids(&self) -> Result<HashSet<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT DISTINCT track_id FROM plays")?;
            let mut set = HashSet::new();
            for id in stmt.query_map([], |row| row.get::<_, String>(0))? {
                set.insert(id?);
            }
            Ok(set)
        })
        .await
    }

    /// Playlists the agent has written to, and therefore tracks the state of.
    pub async fn managed_playlists(&self) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT DISTINCT playlist_id FROM playlist_members")?;
            let ids = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(ids)
        })
        .await
    }

    // -----------------------------------------------------------------
    // Artist bans
    // -----------------------------------------------------------------

    pub async fn ban_artist(
        &self,
        name: String,
        artist_id: Option<String>,
        reason: Option<String>,
    ) -> Result<()> {
        self.with_conn(move |conn| {
            let norm = crate::util::text::normalize(&name);
            conn.execute(
                "INSERT INTO banned_artists (name_norm, name, artist_id, reason, created_at)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(name_norm) DO UPDATE SET
                    name = excluded.name,
                    artist_id = COALESCE(excluded.artist_id, banned_artists.artist_id),
                    reason = COALESCE(excluded.reason, banned_artists.reason)",
                params![norm, name, artist_id, reason, now_ms()],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn unban_artist(&self, name: String) -> Result<bool> {
        self.with_conn(move |conn| {
            let norm = crate::util::text::normalize(&name);
            let removed = conn.execute(
                "DELETE FROM banned_artists WHERE name_norm = ?1",
                params![norm],
            )?;
            Ok(removed > 0)
        })
        .await
    }

    pub async fn banned_artists(&self) -> Result<Vec<BannedArtist>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT name, name_norm, artist_id, reason, created_at
                 FROM banned_artists ORDER BY created_at DESC",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(BannedArtist {
                        name: row.get(0)?,
                        name_norm: row.get(1)?,
                        artist_id: row.get(2)?,
                        reason: row.get(3)?,
                        created_at: from_ms(row.get::<_, i64>(4)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Ban keys — normalised names and artist ids — for the selection filter.
    pub async fn ban_keys(&self) -> Result<HashSet<String>> {
        Ok(self
            .banned_artists()
            .await?
            .into_iter()
            .flat_map(|b| {
                let mut keys = vec![b.name_norm];
                if let Some(id) = b.artist_id {
                    keys.push(id);
                }
                keys
            })
            .collect())
    }

    // -----------------------------------------------------------------
    // Playlist snapshots & membership
    // -----------------------------------------------------------------

    /// Copy a playlist's contents into the local database.
    ///
    /// Called before every destructive write, so an overwrite can always be
    /// undone even though Spotify itself offers no such guarantee.
    pub async fn snapshot_playlist(
        &self,
        playlist_id: String,
        playlist_name: String,
        spotify_snapshot_id: Option<String>,
        reason: &'static str,
        tracks: Vec<Track>,
    ) -> Result<i64> {
        self.with_conn(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO playlist_snapshots
                    (playlist_id, playlist_name, spotify_snapshot_id, reason, track_count, taken_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    playlist_id,
                    playlist_name,
                    spotify_snapshot_id,
                    reason,
                    tracks.len() as i64,
                    now_ms()
                ],
            )?;
            let id = tx.last_insert_rowid();
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO snapshot_tracks
                        (snapshot_id, position, track_id, artist, title, album, duration_ms)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                )?;
                for (position, track) in tracks.iter().enumerate() {
                    stmt.execute(params![
                        id,
                        position as i64,
                        track.id,
                        track.artist_line(),
                        track.name,
                        track.album,
                        track.duration_ms,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(id)
        })
        .await
    }

    pub async fn snapshots(
        &self,
        limit: usize,
        playlist_id: Option<String>,
    ) -> Result<Vec<SnapshotRow>> {
        self.with_conn(move |conn| {
            let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<SnapshotRow> {
                Ok(SnapshotRow {
                    id: row.get(0)?,
                    playlist_id: row.get(1)?,
                    playlist_name: row.get(2)?,
                    reason: row.get(3)?,
                    track_count: row.get::<_, i64>(4)? as u32,
                    taken_at: from_ms(row.get::<_, i64>(5)?),
                })
            };
            let rows = match &playlist_id {
                Some(id) => conn
                    .prepare(
                        "SELECT id, playlist_id, playlist_name, reason, track_count, taken_at
                         FROM playlist_snapshots WHERE playlist_id = ?2
                         ORDER BY taken_at DESC LIMIT ?1",
                    )?
                    .query_map(params![limit as i64, id], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => conn
                    .prepare(
                        "SELECT id, playlist_id, playlist_name, reason, track_count, taken_at
                         FROM playlist_snapshots ORDER BY taken_at DESC LIMIT ?1",
                    )?
                    .query_map(params![limit as i64], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            };
            Ok(rows)
        })
        .await
    }

    pub async fn snapshot_tracks(&self, snapshot_id: i64) -> Result<Vec<SnapshotTrack>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT position, track_id, artist, title, album, duration_ms
                 FROM snapshot_tracks WHERE snapshot_id = ?1 ORDER BY position",
            )?;
            let rows = stmt
                .query_map(params![snapshot_id], |row| {
                    Ok(SnapshotTrack {
                        position: row.get::<_, i64>(0)? as u32,
                        track_id: row.get(1)?,
                        artist: row.get(2)?,
                        title: row.get(3)?,
                        album: row.get(4)?,
                        duration_ms: row.get::<_, i64>(5)? as u32,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Record which tracks are in a managed playlist and when they arrived.
    /// Existing rows keep their original `added_at` — that age is what rolling
    /// eviction sorts on.
    pub async fn record_members(&self, playlist_id: String, track_ids: Vec<String>) -> Result<()> {
        self.with_conn(move |conn| {
            let now = now_ms();
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR IGNORE INTO playlist_members (playlist_id, track_id, added_at)
                     VALUES (?1,?2,?3)",
                )?;
                for id in &track_ids {
                    stmt.execute(params![playlist_id, id, now])?;
                }
            }
            // Drop rows for tracks no longer present, so the table tracks
            // reality rather than accumulating forever.
            if track_ids.is_empty() {
                tx.execute(
                    "DELETE FROM playlist_members WHERE playlist_id = ?1",
                    params![playlist_id],
                )?;
            } else {
                let placeholders = vec!["?"; track_ids.len()].join(",");
                let sql = format!(
                    "DELETE FROM playlist_members WHERE playlist_id = ?1 AND track_id NOT IN ({placeholders})"
                );
                let mut args: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(track_ids.len() + 1);
                args.push(&playlist_id);
                for id in &track_ids {
                    args.push(id);
                }
                tx.execute(&sql, args.as_slice())?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn members(&self, playlist_id: String) -> Result<Vec<PlaylistMember>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT track_id, added_at FROM playlist_members
                 WHERE playlist_id = ?1 ORDER BY added_at",
            )?;
            let rows = stmt
                .query_map(params![playlist_id], |row| {
                    Ok(PlaylistMember {
                        track_id: row.get(0)?,
                        added_at: from_ms(row.get::<_, i64>(1)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    // -----------------------------------------------------------------
    // Housekeeping
    // -----------------------------------------------------------------

    pub async fn stats(&self) -> Result<CacheStats> {
        let path = self.path.clone();
        self.with_conn(move |conn| {
            let count = |sql: &str| -> rusqlite::Result<u64> {
                conn.query_row(sql, [], |row| row.get::<_, i64>(0))
                    .map(|v| v as u64)
            };
            let last_sync: Option<i64> = conn
                .query_row(
                    "SELECT value FROM kv WHERE key = 'last_sync_ms'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .and_then(|v| v.parse().ok());

            let (oldest, newest): (Option<i64>, Option<i64>) = conn
                .query_row(
                    "SELECT MIN(played_at), MAX(played_at) FROM plays",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or((None, None));

            Ok(CacheStats {
                tracks: count("SELECT COUNT(*) FROM tracks")?,
                artists: count("SELECT COUNT(*) FROM artists")?,
                saved: count("SELECT COUNT(*) FROM saved_tracks")?,
                plays: count("SELECT COUNT(*) FROM plays")?,
                distinct_played: count("SELECT COUNT(DISTINCT track_id) FROM plays")?,
                recommendations: count("SELECT COUNT(*) FROM recommendations")?,
                runs: count("SELECT COUNT(*) FROM runs")?,
                db_bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                last_sync: last_sync.map(from_ms),
                oldest_play: oldest.map(from_ms),
                newest_play: newest.map(from_ms),
            })
        })
        .await
    }

    pub async fn set_kv(&self, key: &'static str, value: String) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_kv(&self, key: &'static str) -> Result<Option<String>> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?)
        })
        .await
    }

    pub async fn mark_synced(&self) -> Result<()> {
        self.set_kv("last_sync_ms", now_ms().to_string()).await
    }

    pub async fn last_sync(&self) -> Result<Option<DateTime<Utc>>> {
        Ok(self
            .get_kv("last_sync_ms")
            .await?
            .and_then(|v| v.parse::<i64>().ok())
            .map(from_ms))
    }

    /// Apply retention and reclaim space.
    pub async fn prune(
        &self,
        retain_plays_days: u32,
        retain_recs_days: u32,
    ) -> Result<(usize, usize)> {
        self.with_conn(move |conn| {
            let mut plays = 0;
            let mut recs = 0;
            if retain_plays_days > 0 {
                let cutoff = now_ms() - i64::from(retain_plays_days) * 86_400_000;
                plays = conn.execute("DELETE FROM plays WHERE played_at < ?1", params![cutoff])?;
            }
            if retain_recs_days > 0 {
                let cutoff = now_ms() - i64::from(retain_recs_days) * 86_400_000;
                recs = conn.execute(
                    "DELETE FROM recommendations WHERE created_at < ?1",
                    params![cutoff],
                )?;
            }
            // VACUUM cannot run inside a transaction; it is safe here because
            // `with_conn` holds the only handle.
            conn.execute_batch("VACUUM")?;
            Ok((plays, recs))
        })
        .await
    }

    /// Drop every cached row but keep the schema (and the token store, which
    /// lives in a different file).
    pub async fn clear(&self) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute_batch(
                "BEGIN;
                 DELETE FROM track_artists;
                 DELETE FROM saved_tracks;
                 DELETE FROM top_items;
                 DELETE FROM plays;
                 DELETE FROM recommendations;
                 DELETE FROM runs;
                 DELETE FROM tracks;
                 DELETE FROM artists;
                 DELETE FROM kv;
                 COMMIT;
                 VACUUM;",
            )?;
            Ok(())
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// Exponential recency decay. `half_life_days == 0` disables it.
///
/// Feedback is an opinion with a shelf life: what the listener skipped a year
/// ago says much less about today than last week's skip does.
fn decay(weight: f32, age_ms: i64, half_life_days: f32) -> f32 {
    if half_life_days <= 0.0 || age_ms <= 0 {
        return weight;
    }
    let age_days = age_ms as f32 / 86_400_000.0;
    weight * 0.5f32.powf(age_days / half_life_days)
}

/// Cutoff timestamp for a retention window. `days == 0` means "no cutoff" —
/// i.e. remember forever — so it returns the epoch rather than "now".
fn cutoff_ms(days: u32) -> i64 {
    if days == 0 {
        0
    } else {
        now_ms() - i64::from(days) * 86_400_000
    }
}

fn from_ms(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Utc::now)
}

fn join_err(e: tokio::task::JoinError) -> AgentError {
    AgentError::other(format!("storage task failed: {e}"))
}

fn script_to_str(script: Script) -> &'static str {
    match script {
        Script::Latin => "latin",
        Script::Cyrillic => "cyrillic",
        Script::Cjk => "cjk",
        Script::Other => "other",
        Script::Unknown => "unknown",
    }
}

fn script_from_str(value: &str) -> Script {
    match value {
        "latin" => Script::Latin,
        "cyrillic" => Script::Cyrillic,
        "cjk" => Script::Cjk,
        "other" => Script::Other,
        _ => Script::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ArtistRef;

    fn track(id: &str, name: &str, artist: &str) -> Track {
        Track {
            id: id.into(),
            name: name.into(),
            artists: vec![ArtistRef {
                id: format!("ar-{id}"),
                name: artist.into(),
            }],
            album: "Album".into(),
            duration_ms: 200_000,
            popularity: 50,
            explicit: false,
            release_year: Some(2015),
            isrc: None,
        }
    }

    async fn temp_storage() -> (Storage, tempdir::TempPath) {
        let path = tempdir::TempPath::new("spotify-agent-test");
        let storage = Storage::open(&path.db_path()).await.expect("open");
        (storage, path)
    }

    /// Minimal temp-dir helper so the crate does not take a dev-dependency
    /// just for three tests.
    mod tempdir {
        use std::path::PathBuf;
        pub struct TempPath(PathBuf);
        impl TempPath {
            pub fn new(prefix: &str) -> Self {
                let mut dir = std::env::temp_dir();
                let unique = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                dir.push(format!("{prefix}-{unique}-{}", std::process::id()));
                std::fs::create_dir_all(&dir).ok();
                Self(dir)
            }
            pub fn db_path(&self) -> PathBuf {
                self.0.join("test.sqlite3")
            }
        }
        impl Drop for TempPath {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }
    }

    #[tokio::test]
    async fn upserts_and_reads_back_stats() {
        let (storage, _guard) = temp_storage().await;
        storage
            .upsert_tracks(vec![track("t1", "Song", "Artist")])
            .await
            .expect("upsert");
        storage
            .replace_saved(vec!["t1".into()])
            .await
            .expect("saved");
        storage
            .insert_plays(vec![PlayEvent {
                track_id: "t1".into(),
                played_at: Utc::now(),
            }])
            .await
            .expect("plays");

        let stats = storage.track_stats().await.expect("stats");
        assert_eq!(stats.len(), 1);
        let s = stats.first().expect("one row");
        assert!(s.saved);
        assert_eq!(s.plays, 1);
        assert_eq!(s.recent_plays, 1);
        assert_eq!(s.track.artists.len(), 1);
    }

    #[tokio::test]
    async fn duplicate_plays_are_ignored() {
        let (storage, _guard) = temp_storage().await;
        storage
            .upsert_tracks(vec![track("t1", "S", "A")])
            .await
            .expect("upsert");
        let at = Utc::now();
        let event = PlayEvent {
            track_id: "t1".into(),
            played_at: at,
        };
        assert_eq!(
            storage
                .insert_plays(vec![event.clone()])
                .await
                .expect("first"),
            1
        );
        assert_eq!(storage.insert_plays(vec![event]).await.expect("second"), 0);
    }

    #[tokio::test]
    async fn an_incomplete_schema_is_repaired_on_open() {
        // A database that is missing tables — a half-finished migration, or a
        // v1 file from an older build — must heal rather than error, because
        // the play history inside it cannot be re-fetched from Spotify.
        let guard = tempdir::TempPath::new("spotify-agent-partial");
        let path = guard.db_path();
        {
            let conn = rusqlite::Connection::open(&path).expect("open");
            conn.execute_batch(
                "CREATE TABLE tracks (id TEXT PRIMARY KEY, name TEXT NOT NULL,
                    artist_line TEXT NOT NULL, album TEXT NOT NULL DEFAULT '',
                    duration_ms INTEGER NOT NULL DEFAULT 0, popularity INTEGER NOT NULL DEFAULT 0,
                    explicit INTEGER NOT NULL DEFAULT 0, release_year INTEGER, isrc TEXT,
                    script TEXT NOT NULL DEFAULT 'unknown',
                    first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL);
                 CREATE TABLE plays (track_id TEXT NOT NULL, played_at INTEGER NOT NULL,
                    PRIMARY KEY (track_id, played_at));
                 INSERT INTO tracks VALUES ('t1','Old','Artist','A',1000,1,0,1999,NULL,'latin',1,1);
                 INSERT INTO plays VALUES ('t1', 1700000000000);
                 PRAGMA user_version = 1;",
            )
            .expect("seed");
        }

        let storage = Storage::open(&path).await.expect("open repairs the schema");

        // The missing tables now exist…
        assert!(storage.stats().await.is_ok(), "kv and friends were created");
        // …and nothing was lost.
        let stats = storage.track_stats().await.expect("stats");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].plays, 1);
    }

    #[tokio::test]
    async fn snapshots_round_trip() {
        let (storage, _guard) = temp_storage().await;
        let tracks = vec![track("t1", "One", "A"), track("t2", "Two", "B")];
        storage.upsert_tracks(tracks.clone()).await.expect("upsert");

        let id = storage
            .snapshot_playlist("pl".into(), "My List".into(), None, "pre-write", tracks)
            .await
            .expect("snapshot");

        let rows = storage.snapshot_tracks(id).await.expect("read back");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].position, 0);
        assert_eq!(rows[0].track_id, "t1");

        let listed = storage
            .snapshots(10, Some("pl".into()))
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].track_count, 2);
        assert_eq!(listed[0].reason, "pre-write");
    }

    #[tokio::test]
    async fn membership_tracks_arrivals_and_departures() {
        let (storage, _guard) = temp_storage().await;
        storage
            .record_members("pl".into(), vec!["a".into(), "b".into()])
            .await
            .expect("record");
        let first = storage.members("pl".into()).await.expect("members");
        assert_eq!(first.len(), 2);
        let a_added = first
            .iter()
            .find(|m| m.track_id == "a")
            .map(|m| m.added_at)
            .expect("a is present");

        // "b" leaves, "c" arrives. "a" must keep its original timestamp —
        // that age is what rolling eviction sorts on.
        storage
            .record_members("pl".into(), vec!["a".into(), "c".into()])
            .await
            .expect("record");
        let second = storage.members("pl".into()).await.expect("members");
        let ids: Vec<&str> = second.iter().map(|m| m.track_id.as_str()).collect();
        assert!(ids.contains(&"a") && ids.contains(&"c"));
        assert!(!ids.contains(&"b"), "a departed track must be dropped");
        assert_eq!(
            second
                .iter()
                .find(|m| m.track_id == "a")
                .map(|m| m.added_at),
            Some(a_added),
            "an incumbent must not have its age reset"
        );
    }

    #[tokio::test]
    async fn feedback_is_idempotent_and_decays() {
        let (storage, _guard) = temp_storage().await;
        storage
            .upsert_tracks(vec![track("t1", "S", "A")])
            .await
            .expect("upsert");

        let entry = FeedbackEntry {
            track_id: "t1".into(),
            signal: Signal::Liked,
            weight: 3.0,
            source: "auto",
            note: None,
            dedupe_key: "liked|t1".into(),
        };
        assert_eq!(
            storage
                .record_feedback(vec![entry.clone()])
                .await
                .expect("first"),
            1
        );
        assert_eq!(
            storage.record_feedback(vec![entry]).await.expect("second"),
            0,
            "the same observation must not be counted twice"
        );

        let fresh = storage.feedback_scores(0.0).await.expect("no decay");
        assert!((fresh.by_track.get("t1").copied().unwrap_or(0.0) - 3.0).abs() < 0.01);
        assert!(fresh.by_artist.contains_key("ar-t1"));

        // With a 1-day half-life a signal written now is still ~full weight;
        // the decay maths is exercised directly in the unit for `decay`.
        let decayed = storage.feedback_scores(1.0).await.expect("decay");
        assert!(decayed.by_track.get("t1").copied().unwrap_or(0.0) <= 3.01);
    }

    #[test]
    fn decay_halves_over_the_half_life() {
        assert!((decay(4.0, 0, 30.0) - 4.0).abs() < 0.001);
        assert!((decay(4.0, 30 * 86_400_000, 30.0) - 2.0).abs() < 0.001);
        assert!((decay(4.0, 60 * 86_400_000, 30.0) - 1.0).abs() < 0.001);
        // Disabled decay leaves the weight untouched however old it is.
        assert!((decay(4.0, 10_000 * 86_400_000, 0.0) - 4.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn remembers_recommendations_forever_when_the_window_is_zero() {
        let (storage, _guard) = temp_storage().await;
        storage
            .upsert_tracks(vec![track("t1", "S", "A")])
            .await
            .expect("upsert");
        let run = storage
            .start_run("p".into(), 1, "m".into())
            .await
            .expect("run");
        storage
            .record_recommendations(
                run,
                "p".into(),
                None,
                vec![RecommendationRecord {
                    track_id: Some("t1".into()),
                    title: "S".into(),
                    artist: "A".into(),
                    reason: String::new(),
                    mood: String::new(),
                    accepted: true,
                    reject_note: None,
                }],
            )
            .await
            .expect("record");

        // A one-day window would already cover a just-written row, so the test
        // that actually distinguishes "forever" uses a backdated row.
        storage
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE recommendations SET created_at = ?1",
                    rusqlite::params![now_ms() - 400 * 86_400_000],
                )?;
                Ok(())
            })
            .await
            .expect("backdate");

        let recent = storage
            .excluded_track_ids(30, false, false)
            .await
            .expect("recent");
        assert!(
            !recent.contains("t1"),
            "a 30-day window must not see a 400-day-old row"
        );

        let forever = storage
            .excluded_track_ids(0, false, false)
            .await
            .expect("forever");
        assert!(forever.contains("t1"), "0 must mean forever, not zero days");
    }

    #[tokio::test]
    async fn artist_cooldown_covers_ids_and_names() {
        let (storage, _guard) = temp_storage().await;
        storage
            .upsert_tracks(vec![track("t1", "Song", "Boards of Canada")])
            .await
            .expect("upsert");
        let run = storage
            .start_run("p".into(), 1, "m".into())
            .await
            .expect("run");
        storage
            .record_recommendations(
                run,
                "p".into(),
                None,
                vec![RecommendationRecord {
                    track_id: Some("t1".into()),
                    title: "Song".into(),
                    artist: "Boards of Canada".into(),
                    reason: String::new(),
                    mood: String::new(),
                    accepted: true,
                    reject_note: None,
                }],
            )
            .await
            .expect("record");

        let cooling = storage.artists_on_cooldown(30).await.expect("cooldown");
        assert!(cooling.contains("ar-t1"), "artist id should be on cooldown");
        assert!(
            cooling.contains("boards of canada"),
            "normalised name should be too"
        );

        let names = storage
            .recent_recommended_artists(30, 10)
            .await
            .expect("names");
        assert_eq!(names, vec!["Boards of Canada".to_string()]);

        // Disabled cooldown must be a genuine no-op, not a full-table scan.
        assert!(
            storage
                .artists_on_cooldown(0)
                .await
                .expect("off")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn exclusions_include_saved_library() {
        let (storage, _guard) = temp_storage().await;
        storage
            .upsert_tracks(vec![track("t1", "S", "A")])
            .await
            .expect("upsert");
        storage
            .replace_saved(vec!["t1".into()])
            .await
            .expect("saved");
        let ids = storage
            .excluded_track_ids(30, true, false)
            .await
            .expect("excluded");
        assert!(ids.contains("t1"));
        let lines = storage.exclusion_lines(30, true, 10).await.expect("lines");
        assert_eq!(lines, vec!["A — S".to_string()]);
    }
}
