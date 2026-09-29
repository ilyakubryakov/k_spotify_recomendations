use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A Spotify track, normalised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Base-62 Spotify id (not the URI).
    pub id: String,
    pub name: String,
    pub artists: Vec<ArtistRef>,
    pub album: String,
    pub duration_ms: u32,
    /// 0–100, Spotify's own metric.
    pub popularity: u8,
    pub explicit: bool,
    pub release_year: Option<i32>,
    /// Present for most catalogue tracks; the most reliable cross-market
    /// identity key when the same recording has several regional ids.
    pub isrc: Option<String>,
}

impl Track {
    pub fn uri(&self) -> String {
        format!("spotify:track:{}", self.id)
    }

    /// Primary credited artist; falls back to a placeholder rather than
    /// panicking on the (malformed) empty-artists case.
    pub fn primary_artist(&self) -> &str {
        self.artists
            .first()
            .map(|a| a.name.as_str())
            .unwrap_or("Unknown Artist")
    }

    pub fn artist_line(&self) -> String {
        if self.artists.is_empty() {
            return "Unknown Artist".into();
        }
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Credited artist names joined, with **no** placeholder fallback.
    ///
    /// Script/language detection must use this rather than [`Self::artist_line`]:
    /// the latter substitutes the Latin string "Unknown Artist" for a track with
    /// no credited artists, which would misclassify it as Latin-script.
    pub fn artist_names_raw(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn duration_secs(&self) -> u32 {
        self.duration_ms / 1000
    }

    /// `mm:ss`, for the TUI table.
    pub fn duration_display(&self) -> String {
        let s = self.duration_secs();
        format!("{}:{:02}", s / 60, s % 60)
    }

    pub fn display(&self) -> String {
        format!("{} — {}", self.artist_line(), self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtistRef {
    pub id: String,
    pub name: String,
}

/// An artist with the genre tags Spotify assigns. Genres live on the artist,
/// never on the track — which is why building genre clusters requires a
/// second round of `/artists` lookups after fetching tracks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub genres: Vec<String>,
    pub popularity: u8,
}

/// One listening event from `/me/player/recently-played`.
///
/// Spotify only ever returns the last 50, so the local SQLite history is the
/// only way to build a real play-frequency signal over months.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayEvent {
    pub track_id: String,
    pub played_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub public: bool,
    pub owner_id: String,
    pub track_count: u32,
    /// Opaque version tag; passing it back on a write makes the update
    /// conflict-safe if the user edited the playlist in the app meanwhile.
    pub snapshot_id: String,
}

impl Playlist {
    pub fn uri(&self) -> String {
        format!("spotify:playlist:{}", self.id)
    }

    pub fn web_url(&self) -> String {
        format!("https://open.spotify.com/playlist/{}", self.id)
    }
}

/// Spotify's `/me/top/*` windows.
// The `*Term` suffix mirrors Spotify's own `short_term`/`medium_term`/
// `long_term` parameter values; renaming them for lint cleanliness would make
// the mapping in `as_api` harder to check.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeRange {
    /// ~4 weeks — what the listener is into *now*.
    ShortTerm,
    /// ~6 months — the stable current taste.
    MediumTerm,
    /// Several years — the long-run identity.
    LongTerm,
}

impl TimeRange {
    pub const ALL: [TimeRange; 3] = [Self::ShortTerm, Self::MediumTerm, Self::LongTerm];

    pub fn as_api(self) -> &'static str {
        match self {
            Self::ShortTerm => "short_term",
            Self::MediumTerm => "medium_term",
            Self::LongTerm => "long_term",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ShortTerm => "last 4 weeks",
            Self::MediumTerm => "last 6 months",
            Self::LongTerm => "all time",
        }
    }

    /// Weight applied when folding the three windows into one affinity score.
    /// Recency is weighted highest because the point of the agent is to match
    /// where the listener is *now*, with the long window as ballast.
    pub fn weight(self) -> f32 {
        match self {
            Self::ShortTerm => 3.0,
            Self::MediumTerm => 2.0,
            Self::LongTerm => 1.0,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "short_term" => Some(Self::ShortTerm),
            "medium_term" => Some(Self::MediumTerm),
            "long_term" => Some(Self::LongTerm),
            _ => None,
        }
    }
}
