//! Domain model — the vocabulary the rest of the crate speaks.
//!
//! Nothing here knows about HTTP, SQLite, TOML or the terminal. The Spotify
//! DTOs in `spotify::models` convert *into* these types at the client
//! boundary, so an upstream API change is contained to one module.

pub mod profile;
pub mod recommendation;
pub mod track;

pub use profile::{
    ArtistAffinity, EraProfile, GenreWeight, ScriptMix, TasteProfile, TrackAffinity,
};
pub use recommendation::{
    CurationResponse, RejectReason, RejectedSuggestion, ResolvedSuggestion, Suggestion,
};
pub use track::{Artist, ArtistRef, PlayEvent, Playlist, TimeRange, Track};
