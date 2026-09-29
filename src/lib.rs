//! `spotify-agent` — an autonomous AI music-curation agent.
//!
//! The crate is split so each layer can be tested without the ones above it:
//!
//! | module      | responsibility                                          |
//! |-------------|---------------------------------------------------------|
//! | [`domain`]  | vocabulary types; no I/O                                |
//! | [`config`]  | layered TOML + env configuration, presets               |
//! | [`spotify`] | OAuth (PKCE), rate-limited Web API client               |
//! | [`llm`]     | provider-agnostic LLM chain (Anthropic / OpenAI / Gemini / Ollama), schema, prompt |
//! | [`storage`] | SQLite cache: library mirror, play history, exclusions  |
//! | [`engine`]  | orchestration: sync → analyse → curate → resolve → publish |
//! | [`cli`] / [`commands`] / [`tui`] | presentation                       |
//! | [`export`]  | M3U8 / CSV / JSON tracklists that outlive Spotify        |
//! | [`schedule`]| user-level background scheduling on all three platforms  |
//! | [`notify`]  | desktop notifications and webhooks                      |
//! | [`util`]    | secrets, retry policy, text normalisation, fs helpers   |
//!
//! Dependencies point strictly downward: `engine` knows about `spotify`,
//! `llm` and `storage`; none of those knows about `engine`, and nothing
//! below `commands`/`tui` knows a terminal exists.

// Tests legitimately use `expect`, `panic!` and direct indexing to assert
// invariants; the crate-level denials in Cargo.toml target production paths.
#![cfg_attr(
    test,
    allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )
)]

pub mod cli;
pub mod commands;
pub mod config;
pub mod domain;
pub mod engine;
pub mod error;
pub mod export;
pub mod i18n;
pub mod llm;
pub mod notify;
pub mod prefs;
pub mod schedule;
pub mod spotify;
pub mod storage;
pub mod telemetry;
pub mod tui;
pub mod util;

pub use error::{AgentError, Result};

/// Binary version, exposed for the `User-Agent` and `--version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
