//! Schema and forward-only migrations.
//!
//! Versioning uses SQLite's own `user_version` pragma rather than a table, so
//! the very first migration has nothing to bootstrap. Each step is idempotent
//! and additive; the cache can always be deleted and rebuilt from Spotify, so
//! there is no down-migration path to maintain.

use rusqlite::Connection;

pub const CURRENT_VERSION: i32 = 2;

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    // WAL: the TUI reads while the pipeline writes. Without it, the reader
    // blocks the writer and the UI stutters.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL is the right durability/throughput trade for a rebuildable cache.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;

    let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    // Every statement in these batches is `CREATE TABLE/INDEX IF NOT EXISTS`,
    // so they are run unconditionally rather than gated on the version. That
    // costs microseconds and makes a partially-created schema self-heal —
    // which matters because the play history in here cannot be re-fetched from
    // Spotify, so "delete it and start again" is not an acceptable recovery.
    //
    // A future migration that is NOT idempotent (an ALTER, a backfill, a
    // DROP) must be gated on `version` in the usual way.
    conn.execute_batch(V1)?;
    conn.execute_batch(V2)?;
    let _ = version;

    if version != CURRENT_VERSION {
        conn.pragma_update(None, "user_version", CURRENT_VERSION)?;
    }
    Ok(())
}

/// All timestamps are Unix milliseconds (INTEGER) so that range comparisons
/// are index-friendly and timezone-free.
const V1: &str = r#"
CREATE TABLE IF NOT EXISTS tracks (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    artist_line   TEXT NOT NULL,
    album         TEXT NOT NULL DEFAULT '',
    duration_ms   INTEGER NOT NULL DEFAULT 0,
    popularity    INTEGER NOT NULL DEFAULT 0,
    explicit      INTEGER NOT NULL DEFAULT 0,
    release_year  INTEGER,
    isrc          TEXT,
    -- Cached script classification; recomputing it for 20k tracks on every
    -- profile build is measurable, and the input never changes.
    script        TEXT NOT NULL DEFAULT 'unknown',
    first_seen    INTEGER NOT NULL,
    last_seen     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS artists (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    genres      TEXT NOT NULL DEFAULT '[]',   -- JSON array
    popularity  INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS track_artists (
    track_id   TEXT NOT NULL,
    artist_id  TEXT NOT NULL,
    position   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (track_id, artist_id),
    FOREIGN KEY (track_id) REFERENCES tracks(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_track_artists_artist ON track_artists(artist_id);

-- Mirror of Liked Songs. Rebuilt wholesale on a full sync so un-liking a
-- track is reflected.
CREATE TABLE IF NOT EXISTS saved_tracks (
    track_id  TEXT PRIMARY KEY,
    synced_at INTEGER NOT NULL,
    FOREIGN KEY (track_id) REFERENCES tracks(id) ON DELETE CASCADE
);

-- /me/top/{tracks,artists} across the three windows.
CREATE TABLE IF NOT EXISTS top_items (
    kind        TEXT NOT NULL,          -- 'track' | 'artist'
    time_range  TEXT NOT NULL,          -- short_term | medium_term | long_term
    item_id     TEXT NOT NULL,
    rank        INTEGER NOT NULL,
    captured_at INTEGER NOT NULL,
    PRIMARY KEY (kind, time_range, item_id)
);

-- Local play history. Spotify keeps only the last 50 events, so this table is
-- the only long-run frequency signal that exists.
CREATE TABLE IF NOT EXISTS plays (
    track_id   TEXT NOT NULL,
    played_at  INTEGER NOT NULL,
    PRIMARY KEY (track_id, played_at)
);
CREATE INDEX IF NOT EXISTS idx_plays_at ON plays(played_at);

-- Everything ever recommended, whether or not it resolved. Drives the
-- exclusion list so the agent does not repeat itself across runs.
CREATE TABLE IF NOT EXISTS recommendations (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id       INTEGER,
    track_id     TEXT,                  -- NULL when the suggestion never resolved
    title        TEXT NOT NULL,
    artist       TEXT NOT NULL,
    reason       TEXT NOT NULL DEFAULT '',
    mood         TEXT NOT NULL DEFAULT '',
    preset       TEXT NOT NULL DEFAULT '',
    playlist_id  TEXT,
    accepted     INTEGER NOT NULL DEFAULT 0,
    reject_note  TEXT,
    created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_recs_created ON recommendations(created_at);
CREATE INDEX IF NOT EXISTS idx_recs_track   ON recommendations(track_id);

CREATE TABLE IF NOT EXISTS runs (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    preset       TEXT NOT NULL,
    started_at   INTEGER NOT NULL,
    finished_at  INTEGER,
    requested    INTEGER NOT NULL DEFAULT 0,
    suggested    INTEGER NOT NULL DEFAULT 0,
    resolved     INTEGER NOT NULL DEFAULT 0,
    written      INTEGER NOT NULL DEFAULT 0,
    playlist_id  TEXT,
    status       TEXT NOT NULL DEFAULT 'running',
    error        TEXT,
    model        TEXT,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_runs_started ON runs(started_at);

CREATE TABLE IF NOT EXISTS kv (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

/// v2 — the feedback loop and playlist lifecycle.
///
/// Additive only: a v1 cache upgrades in place with no data loss, which
/// matters because the play history it holds cannot be re-fetched from
/// Spotify (the API only ever returns the last 50 events).
const V2: &str = r#"
-- Signals about how a recommendation actually landed.
CREATE TABLE IF NOT EXISTS feedback (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id   TEXT NOT NULL,
    signal     TEXT NOT NULL,      -- liked | played | skipped | removed | kept | stale | up | down | banned
    weight     REAL NOT NULL,      -- signed; positive is approval
    source     TEXT NOT NULL,      -- auto | tui | cli
    note       TEXT,
    -- Idempotency key. Sync runs repeatedly over the same history, and without
    -- this a single skip would be re-counted on every run until it aged out.
    dedupe_key TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_feedback_track   ON feedback(track_id);
CREATE INDEX IF NOT EXISTS idx_feedback_created ON feedback(created_at);

-- Permanent artist bans, set from the TUI or the CLI.
CREATE TABLE IF NOT EXISTS banned_artists (
    name_norm  TEXT PRIMARY KEY,   -- util::text::normalize of the name
    name       TEXT NOT NULL,
    artist_id  TEXT,
    reason     TEXT,
    created_at INTEGER NOT NULL
);

-- A full copy of a playlist, taken before every destructive write.
CREATE TABLE IF NOT EXISTS playlist_snapshots (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id         TEXT NOT NULL,
    playlist_name       TEXT NOT NULL,
    spotify_snapshot_id TEXT,
    reason              TEXT NOT NULL,   -- pre-write | manual | pre-rolling
    track_count         INTEGER NOT NULL,
    taken_at            INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_snapshots_playlist ON playlist_snapshots(playlist_id, taken_at);

CREATE TABLE IF NOT EXISTS snapshot_tracks (
    snapshot_id INTEGER NOT NULL REFERENCES playlist_snapshots(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    track_id    TEXT NOT NULL,
    artist      TEXT NOT NULL,
    title       TEXT NOT NULL,
    album       TEXT NOT NULL DEFAULT '',
    duration_ms INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (snapshot_id, position)
);

-- When each track entered a managed playlist. Rolling eviction needs an age,
-- and Spotify's added_at is not exposed per track in the fields we request.
CREATE TABLE IF NOT EXISTS playlist_members (
    playlist_id TEXT NOT NULL,
    track_id    TEXT NOT NULL,
    added_at    INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, track_id)
);
CREATE INDEX IF NOT EXISTS idx_members_added ON playlist_members(playlist_id, added_at);
"#;
