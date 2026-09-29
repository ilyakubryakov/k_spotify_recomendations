//! The listener's taste profile — the analytical artefact that goes into the
//! prompt.
//!
//! Everything here is derived from the local SQLite mirror, not fetched live,
//! so a profile can be recomputed offline and a `generate` run does not depend
//! on Spotify being reachable for analysis (only for resolution and writing).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TasteProfile {
    pub generated_at: DateTime<Utc>,

    /// Counts from the local mirror.
    pub saved_tracks: usize,
    pub known_tracks: usize,
    pub known_artists: usize,
    pub play_events: usize,
    pub distinct_played: usize,

    /// Ranked by the folded short/medium/long-term score.
    pub top_artists: Vec<ArtistAffinity>,
    pub top_tracks: Vec<TrackAffinity>,
    /// Tracks with an unusually high recent play count — current obsessions.
    pub looped: Vec<TrackAffinity>,
    /// Genre vocabulary, weighted by the affinity of the artists carrying it.
    pub genres: Vec<GenreWeight>,

    pub era: EraProfile,
    pub script_mix: ScriptMix,

    /// Median popularity of the library. A listener sitting at 35 wants
    /// different recommendations from one sitting at 75, and telling the model
    /// this is far more effective than asking it to guess.
    pub median_popularity: u8,
    /// Median track length in seconds.
    pub median_duration_secs: u32,
}

impl TasteProfile {
    pub fn is_empty(&self) -> bool {
        self.known_tracks == 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistAffinity {
    pub id: String,
    pub name: String,
    /// Folded weight across the three top-term windows plus play history.
    pub score: f32,
    /// Number of the listener's saved/top tracks credited to this artist.
    pub track_count: u32,
    pub plays: u32,
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackAffinity {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub score: f32,
    pub plays: u32,
    /// Plays within the last 30 days — the "is this on repeat right now" signal.
    pub recent_plays: u32,
    pub release_year: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenreWeight {
    pub genre: String,
    pub weight: f32,
    pub artist_count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EraProfile {
    pub median_year: Option<i32>,
    pub p10_year: Option<i32>,
    pub p90_year: Option<i32>,
    /// `(decade, share)` sorted descending by share, e.g. `(2010, 0.42)`.
    pub decades: Vec<(i32, f32)>,
}

impl EraProfile {
    pub fn describe(&self) -> String {
        match (self.p10_year, self.median_year, self.p90_year) {
            (Some(lo), Some(mid), Some(hi)) => {
                format!("mostly {lo}–{hi}, centred on {mid}")
            }
            (_, Some(mid), _) => format!("centred on {mid}"),
            _ => "unknown".into(),
        }
    }
}

/// Share of the library by writing system. Drives both the default language
/// policy hint and the sanity check that a "russian" run is actually viable
/// for this listener.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScriptMix {
    pub latin: f32,
    pub cyrillic: f32,
    pub cjk: f32,
    pub other: f32,
}

impl ScriptMix {
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for (label, share) in [
            ("latin-script", self.latin),
            ("cyrillic-script", self.cyrillic),
            ("cjk", self.cjk),
            ("other", self.other),
        ] {
            if share >= 0.02 {
                parts.push(format!("{label} {:.0}%", share * 100.0));
            }
        }
        if parts.is_empty() {
            "unknown".into()
        } else {
            parts.join(", ")
        }
    }
}
