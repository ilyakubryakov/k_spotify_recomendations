//! Wire DTOs for the Spotify Web API.
//!
//! Several fields are declared but never read. That is deliberate: they
//! document the wire contract and make the shape reviewable against Spotify's
//! reference, and `serde` needs them present to round-trip a payload faithfully
//! if this module ever re-serialises one.
#![allow(dead_code)]
//!
//! These mirror the JSON exactly and are deliberately permissive: every field
//! that Spotify can return as `null` is an `Option`, and unknown fields are
//! ignored. Conversion into the domain types happens here so that a shape
//! change upstream is a compile error in one file.

use crate::domain::{Artist, ArtistRef, PlayEvent, Playlist, Track};
use chrono::{DateTime, Utc};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// `https://api.spotify.com/...` error envelope.
#[derive(Debug, Deserialize)]
pub struct ApiErrorEnvelope {
    pub error: ApiErrorBody,
}

#[derive(Debug, Deserialize)]
pub struct ApiErrorBody {
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub message: Option<String>,
}

/// `https://accounts.spotify.com/api/token` uses a different (OAuth) shape.
#[derive(Debug, Deserialize)]
pub struct OAuthErrorEnvelope {
    pub error: String,
    #[serde(default)]
    pub error_description: Option<String>,
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Page<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub total: Option<u32>,
}

/// `/me/player/recently-played` is cursor-paged, not offset-paged.
#[derive(Debug, Deserialize)]
pub struct CursorPage<T> {
    #[serde(default = "Vec::new")]
    pub items: Vec<T>,
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub cursors: Option<Cursors>,
}

#[derive(Debug, Deserialize)]
pub struct Cursors {
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub before: Option<String>,
}

// ---------------------------------------------------------------------------
// Tracks
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SavedTrackItem {
    pub track: Option<TrackObject>,
}

#[derive(Debug, Deserialize)]
pub struct PlayHistoryItem {
    pub track: Option<TrackObject>,
    pub played_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct PlaylistTrackItem {
    pub track: Option<TrackObject>,
}

#[derive(Debug, Deserialize)]
pub struct TrackObject {
    /// `null` for local files and for some region-restricted relinked tracks.
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default = "Vec::new")]
    pub artists: Vec<SimpleArtistObject>,
    #[serde(default)]
    pub album: Option<AlbumObject>,
    #[serde(default)]
    pub duration_ms: u32,
    #[serde(default)]
    pub popularity: Option<u8>,
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub external_ids: Option<ExternalIds>,
    #[serde(default)]
    pub is_local: bool,
    /// Only present when the request carried a `market`; `false` means the
    /// track cannot be played there and must not be added to a playlist.
    #[serde(default)]
    pub is_playable: Option<bool>,
}

impl TrackObject {
    /// `None` when the object cannot become a usable domain `Track`
    /// (local file, missing id, or unplayable in the requested market).
    pub fn into_domain(self) -> Option<Track> {
        if self.is_local || self.is_playable == Some(false) {
            return None;
        }
        let id = self.id?;
        if id.is_empty() {
            return None;
        }
        let album = self.album;
        Some(Track {
            id,
            name: self.name,
            artists: self
                .artists
                .into_iter()
                .filter_map(|a| a.id.map(|id| ArtistRef { id, name: a.name }))
                .collect(),
            album: album.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
            duration_ms: self.duration_ms,
            popularity: self.popularity.unwrap_or(0),
            explicit: self.explicit,
            release_year: album.and_then(|a| a.release_year()),
            isrc: self.external_ids.and_then(|e| e.isrc),
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct ExternalIds {
    #[serde(default)]
    pub isrc: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlbumObject {
    pub name: String,
    #[serde(default)]
    pub release_date: Option<String>,
    /// `year` | `month` | `day` — the date is only as precise as this says.
    #[serde(default)]
    pub release_date_precision: Option<String>,
}

impl AlbumObject {
    pub fn release_year(&self) -> Option<i32> {
        let raw = self.release_date.as_deref()?;
        raw.get(..4)?
            .parse::<i32>()
            .ok()
            .filter(|y| (1900..=2100).contains(y))
    }
}

#[derive(Debug, Deserialize)]
pub struct SimpleArtistObject {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct ArtistObject {
    pub id: String,
    pub name: String,
    #[serde(default = "Vec::new")]
    pub genres: Vec<String>,
    #[serde(default)]
    pub popularity: Option<u8>,
}

impl From<ArtistObject> for Artist {
    fn from(a: ArtistObject) -> Self {
        Artist {
            id: a.id,
            name: a.name,
            genres: a.genres,
            popularity: a.popularity.unwrap_or(0),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ArtistsResponse {
    #[serde(default = "Vec::new")]
    pub artists: Vec<Option<ArtistObject>>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    #[serde(default)]
    pub tracks: Option<Page<TrackObject>>,
}

// ---------------------------------------------------------------------------
// User & playlists
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct UserObject {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub product: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PlaylistObject {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// `null` when the playlist's visibility is not exposed to this token.
    #[serde(default)]
    pub public: Option<bool>,
    #[serde(default)]
    pub owner: Option<OwnerObject>,
    #[serde(default)]
    pub tracks: Option<TracksRef>,
    #[serde(default)]
    pub snapshot_id: Option<String>,
}

impl From<PlaylistObject> for Playlist {
    fn from(p: PlaylistObject) -> Self {
        Playlist {
            id: p.id,
            name: p.name,
            description: p.description.filter(|d| !d.is_empty()),
            public: p.public.unwrap_or(false),
            owner_id: p.owner.map(|o| o.id).unwrap_or_default(),
            track_count: p.tracks.map(|t| t.total).unwrap_or(0),
            snapshot_id: p.snapshot_id.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct OwnerObject {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct TracksRef {
    #[serde(default)]
    pub total: u32,
}

#[derive(Debug, Deserialize)]
pub struct SnapshotResponse {
    #[serde(default)]
    pub snapshot_id: Option<String>,
}

// ---------------------------------------------------------------------------
// OAuth
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    /// Absent on a refresh when Spotify chooses to keep the existing one —
    /// the caller must retain the old refresh token in that case.
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn play_events(items: Vec<PlayHistoryItem>) -> (Vec<Track>, Vec<PlayEvent>) {
    let mut tracks = Vec::with_capacity(items.len());
    let mut events = Vec::with_capacity(items.len());
    for item in items {
        let played_at = item.played_at;
        if let Some(track) = item.track.and_then(TrackObject::into_domain) {
            events.push(PlayEvent {
                track_id: track.id.clone(),
                played_at,
            });
            tracks.push(track);
        }
    }
    (tracks, events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_files_are_dropped() {
        let json = r#"{"id":null,"name":"x","artists":[],"is_local":true,"duration_ms":0}"#;
        let obj: TrackObject = serde_json::from_str(json).expect("parses");
        assert!(obj.into_domain().is_none());
    }

    #[test]
    fn unplayable_in_market_is_dropped() {
        let json = r#"{"id":"abc","name":"x","artists":[],"duration_ms":1000,"is_playable":false}"#;
        let obj: TrackObject = serde_json::from_str(json).expect("parses");
        assert!(obj.into_domain().is_none());
    }

    #[test]
    fn year_precision_is_tolerated() {
        let a = AlbumObject {
            name: "n".into(),
            release_date: Some("1994".into()),
            release_date_precision: Some("year".into()),
        };
        assert_eq!(a.release_year(), Some(1994));
        let b = AlbumObject {
            name: "n".into(),
            release_date: Some("1994-03-08".into()),
            release_date_precision: Some("day".into()),
        };
        assert_eq!(b.release_year(), Some(1994));
    }
}
