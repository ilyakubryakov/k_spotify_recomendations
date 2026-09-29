//! Claude's output and what happens to it downstream.

use super::track::Track;
use serde::{Deserialize, Serialize};

/// One suggestion, exactly as the model returns it. Field names match the JSON
/// schema in `claude::schema` — keep the two in lockstep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub title: String,
    pub artist: String,
    /// Why this track, for this listener, right now. Surfaced in the TUI and
    /// stored so a later run can avoid repeating the same rationale.
    pub reason: String,
    /// Short mood tag, e.g. "brooding", "propulsive".
    pub mood: String,
    /// Model's own 0–1 confidence that the track exists on Spotify under this
    /// exact spelling. Used only to order resolution attempts.
    #[serde(default)]
    pub confidence: Option<f32>,
    /// BCP-47-ish hint from the model ("en", "ru"). Advisory: the script
    /// heuristic in `util::text` is what actually enforces policy.
    #[serde(default)]
    pub language: Option<String>,
}

impl Suggestion {
    pub fn display(&self) -> String {
        format!("{} — {}", self.artist, self.title)
    }

    /// Spotify search query. Field-qualified (`track:`/`artist:`) because a
    /// bare concatenation matches album titles and playlist names far too
    /// eagerly.
    pub fn search_query(&self) -> String {
        let clean = |s: &str| s.replace(['"', ':'], " ");
        format!(
            "track:\"{}\" artist:\"{}\"",
            clean(&self.title).trim(),
            clean(&self.artist).trim()
        )
    }

    /// Fallback query for when the field-qualified form returns nothing —
    /// usually a punctuation or transliteration mismatch.
    pub fn loose_query(&self) -> String {
        format!("{} {}", self.artist, self.title)
    }
}

/// The whole structured response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurationResponse {
    /// Model-proposed playlist title. Used only when the configured
    /// `playlist_name` template contains `{title}`.
    #[serde(default)]
    pub playlist_title: Option<String>,
    /// One paragraph explaining the shape of the set. Shown in the TUI and
    /// written into the playlist description when it fits.
    #[serde(default)]
    pub summary: String,
    pub tracks: Vec<Suggestion>,
}

/// A suggestion that was matched to a real Spotify track.
#[derive(Debug, Clone)]
pub struct ResolvedSuggestion {
    pub suggestion: Suggestion,
    pub track: Track,
    /// 0–1 blend of title and artist similarity; see `engine::resolve`.
    pub match_score: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// Spotify search returned nothing usable.
    NotFound,
    /// Best candidate was below the similarity floor.
    WeakMatch { score_pct: u8 },
    /// Already in the listener's library / recently recommended / already in
    /// the target playlist.
    Excluded(&'static str),
    /// Failed the language policy.
    Language,
    /// Failed a genre / artist / popularity / duration filter.
    Filtered(&'static str),
    /// Would exceed `max_per_artist`.
    ArtistQuota,
    /// The model returned the same track twice.
    Duplicate,
    /// Playlist already full.
    Overflow,
}

impl RejectReason {
    pub fn label(&self) -> String {
        match self {
            Self::NotFound => "not on Spotify".into(),
            Self::WeakMatch { score_pct } => format!("weak match ({score_pct}%)"),
            Self::Excluded(what) => format!("excluded: {what}"),
            Self::Language => "language policy".into(),
            Self::Filtered(what) => format!("filtered: {what}"),
            Self::ArtistQuota => "artist quota".into(),
            Self::Duplicate => "duplicate".into(),
            Self::Overflow => "playlist full".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RejectedSuggestion {
    pub suggestion: Suggestion,
    pub reason: RejectReason,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_query_is_field_qualified_and_escaped() {
        let s = Suggestion {
            title: "Song: The \"Remix\"".into(),
            artist: "A:B".into(),
            reason: String::new(),
            mood: String::new(),
            confidence: None,
            language: None,
        };
        let q = s.search_query();
        assert!(q.starts_with("track:\"Song"));
        assert!(!q.contains("Song:"));
        assert_eq!(q.matches('"').count(), 4);
    }
}
