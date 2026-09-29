//! Configuration: layered TOML + environment, with secrets kept out of both
//! `Debug` output and any serialised form.
//!
//! Resolution order (last wins):
//!   1. compiled-in defaults ([`Default`] impls below)
//!   2. `~/.config/spotify-agent/config.toml` (or `--config <path>`)
//!   3. environment variables (`SPOTIFY_AGENT__*`, plus the well-known
//!      `ANTHROPIC_API_KEY` / `SPOTIFY_CLIENT_ID` / `SPOTIFY_CLIENT_SECRET`)
//!   4. CLI flags (applied by `cli::RunOverrides`, not here)

pub mod paths;
pub mod presets;

use crate::error::{AgentError, Result};
use crate::llm::Provider;
use crate::util::secret::Secret;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment prefix for generic overrides, e.g.
/// `SPOTIFY_AGENT__CLAUDE__MODEL=claude-opus-5`.
const ENV_PREFIX: &str = "SPOTIFY_AGENT__";

// ===========================================================================
// Root
// ===========================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub spotify: SpotifyConfig,
    /// Primary LLM backend. The key is `[claude]` for continuity, but it can
    /// name any supported provider via its `provider` field.
    pub claude: BackendConfig,
    /// Ordered backup backends, tried when the primary cannot answer.
    pub llm: LlmConfig,
    pub storage: StorageConfig,
    pub feedback: FeedbackConfig,
    pub notifications: NotificationConfig,
    pub defaults: RunDefaults,
    pub filters: Filters,
    /// Named presets, merged over `defaults` + `filters` at run time.
    pub presets: BTreeMap<String, Preset>,

    /// Where this config was loaded from. Not part of the file format.
    #[serde(skip)]
    pub source_path: Option<PathBuf>,

    /// Non-fatal problems found while loading.
    ///
    /// Loading happens *before* the log subscriber is installed (the config
    /// decides the log format), so warnings raised here would be emitted into
    /// the void. They are collected instead and replayed by `main` once
    /// logging is live.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

// ===========================================================================
// [general]
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// `error` | `warn` | `info` | `debug` | `trace`, or a full `RUST_LOG`
    /// directive string such as `info,spotify_agent::spotify=debug`.
    pub log_level: String,
    pub log_format: LogFormat,
    /// `auto` respects `NO_COLOR` and TTY detection.
    pub color: ColorMode,
    /// Override the platform data dir (SQLite cache + token store).
    pub data_dir: Option<PathBuf>,

    /// Interface language: `en` | `ru` | `pl` | `lt`.
    ///
    /// Setting it here pins the choice and suppresses the first-run picker.
    /// Leave it unset to choose in the UI (saved to `preferences.json`).
    pub language: Option<crate::i18n::Lang>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            log_format: LogFormat::Text,
            color: ColorMode::Auto,
            data_dir: None,
            language: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable, for interactive use.
    Text,
    /// One JSON object per line — what you want under cron/systemd/Loki.
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

// ===========================================================================
// [spotify]
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpotifyConfig {
    /// From the Spotify developer dashboard. Not a secret (it ships in the
    /// authorize URL), so it lives in plain config.
    pub client_id: Option<String>,

    /// Optional. When absent the agent uses **PKCE**, which is the correct
    /// flow for a desktop app: no secret to leak, no secret to rotate.
    /// When present, the classic Authorization Code flow is used instead.
    pub client_secret: Option<Secret>,

    /// Loopback redirect. Spotify requires an explicit-loopback IP literal
    /// here; `localhost` is rejected by the dashboard.
    pub redirect_host: String,
    pub redirect_port: u16,
    pub redirect_path: String,

    pub api_base: String,
    pub accounts_base: String,

    /// ISO-3166-1 alpha-2. Affects search results and track availability;
    /// leaving it unset makes Spotify infer it from the token, which is
    /// usually what you want.
    pub market: Option<String>,

    pub timeout_secs: u64,
    pub max_retries: u32,
    /// Concurrent in-flight requests during track resolution. Spotify's
    /// per-app rate limit is a rolling 30s window; 4–6 is a safe ceiling.
    pub concurrency: usize,
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            redirect_host: "127.0.0.1".into(),
            redirect_port: 8888,
            redirect_path: "/callback".into(),
            api_base: "https://api.spotify.com/v1".into(),
            accounts_base: "https://accounts.spotify.com".into(),
            market: None,
            timeout_secs: 30,
            max_retries: 5,
            concurrency: 4,
        }
    }
}

impl SpotifyConfig {
    pub fn redirect_uri(&self) -> String {
        format!(
            "http://{}:{}{}",
            self.redirect_host, self.redirect_port, self.redirect_path
        )
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.clamp(5, 300))
    }
}

// ===========================================================================
// [claude]
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackendConfig {
    /// `anthropic` | `openai` | `ollama` | `gemini`.
    pub provider: Provider,

    /// Prefer leaving this unset and exporting the provider's key variable.
    /// If it is set here, the file must be mode 0600 (the loader warns).
    pub api_key: Option<Secret>,
    /// Environment variable holding the key. Defaults per provider
    /// (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, …).
    pub api_key_env: Option<String>,

    /// Endpoint override. Defaults per provider; this is what you point at a
    /// local Ollama, a proxy, or an Azure/Vertex-style gateway.
    pub base_url: Option<String>,

    /// Anthropic only.
    pub anthropic_version: String,

    pub model: String,

    /// Output cap.
    ///
    /// Sized for the streaming default: a 50-track playlist is oversampled to
    /// ~90 suggestions, each carrying a sentence of reasoning, which runs well
    /// past 16k output tokens. Streaming removes the HTTP-timeout reason to
    /// keep this small. If you set `stream = false`, lower it to ~16000 so a
    /// long turn cannot outlive the request timeout.
    pub max_tokens: u32,

    /// Anthropic only: `low` | `medium` | `high` | `xhigh` | `max`. Controls
    /// thinking depth and total token spend. Ignored by other providers.
    pub effort: Effort,

    /// Anthropic only.
    pub thinking: ThinkingMode,
    pub thinking_display: ThinkingDisplay,

    /// Stream the response over SSE. Gives the TUI live progress and removes
    /// any risk of an HTTP idle timeout on a long turn.
    pub stream: bool,

    /// Anthropic only: server-side refusal fallbacks, in order. This is
    /// *inside* one API call and is unrelated to `[[llm.fallbacks]]`, which
    /// switches provider entirely.
    pub fallback_models: Vec<String>,

    pub timeout_secs: u64,
    pub max_retries: u32,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            provider: Provider::Anthropic,
            api_key: None,
            api_key_env: None,
            base_url: None,
            anthropic_version: "2023-06-01".into(),
            model: "claude-opus-5".into(),
            max_tokens: 64_000,
            effort: Effort::High,
            thinking: ThinkingMode::Adaptive,
            thinking_display: ThinkingDisplay::Summarized,
            stream: true,
            fallback_models: Vec::new(),
            timeout_secs: 300,
            max_retries: 4,
        }
    }
}

impl BackendConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.clamp(30, 1800))
    }

    pub fn base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| self.provider.default_base_url().to_string())
            .trim_end_matches('/')
            .to_string()
    }

    pub fn key_env(&self) -> String {
        self.api_key_env
            .clone()
            .unwrap_or_else(|| self.provider.default_key_env().to_string())
    }

    pub fn label(&self) -> String {
        format!("{}/{}", self.provider.as_str(), self.model)
    }

    /// Resolve the API key from config or environment.
    ///
    /// `Ok(None)` is a valid outcome for providers that do not need one — a
    /// local Ollama is the normal case.
    pub fn resolve_api_key(&self) -> Result<Option<Secret>> {
        if let Some(key) = &self.api_key
            && !key.is_empty()
        {
            return Ok(Some(key.clone()));
        }
        let var = self.key_env();
        match std::env::var(&var) {
            Ok(v) if !v.trim().is_empty() => Ok(Some(Secret::new(v.trim()))),
            _ if !self.provider.requires_key() => Ok(None),
            _ => Err(AgentError::MissingCredential {
                name: "llm api key",
                hint: format!(
                    "export {var}=… or set api_key for the {} backend in the config file",
                    self.provider.as_str()
                ),
            }),
        }
    }

    fn validate(&self, context: &str) -> Result<()> {
        // `api_key_env` is the NAME of an environment variable, not the key.
        // Pasting the key into it is an easy and silent mistake: the lookup
        // simply finds nothing and the backend is dropped as "no credentials",
        // with no hint that the key was right there in the file.
        if let Some(var) = &self.api_key_env {
            let looks_like_a_name = !var.is_empty()
                && var
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !looks_like_a_name {
                return Err(AgentError::config(format!(
                    "{context}.api_key_env must be the NAME of an environment variable \
                     (e.g. \"ANTHROPIC_API_KEY\"), but it looks like a key itself. \
                     Put the key in `{context}.api_key` instead, or set the variable and \
                     name it here."
                )));
            }
        }
        if self.max_tokens < 1024 {
            return Err(AgentError::config(format!(
                "{context}.max_tokens must be at least 1024"
            )));
        }
        if self.model.trim().is_empty() {
            return Err(AgentError::config(format!(
                "{context}.model must not be empty"
            )));
        }
        if self.provider == Provider::Anthropic
            && self.thinking == ThinkingMode::Disabled
            && matches!(self.effort, Effort::Xhigh | Effort::Max)
        {
            // The API rejects this pair outright; failing here gives a far
            // better message than a 400 from the wire.
            return Err(AgentError::config(format!(
                "{context}.thinking = \"disabled\" is not accepted at effort xhigh/max — \
                 use adaptive thinking with a lower effort instead"
            )));
        }
        if let Some(url) = &self.base_url
            && url::Url::parse(url).is_err()
        {
            return Err(AgentError::config(format!(
                "{context}.base_url is not a valid URL"
            )));
        }
        Ok(())
    }
}

/// `[llm]` — the ordered backup chain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    /// Backends tried, in order, when the primary cannot answer: it is
    /// unreachable, out of retries, missing a key, refuses, or returns output
    /// that does not match the schema.
    pub fallbacks: Vec<BackendConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode {
    /// The model decides when and how deeply to think. Correct for every
    /// current model; there is no `budget_tokens` any more.
    Adaptive,
    /// Accepted on Opus 5 only at effort `high` or below.
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
}

impl ThinkingDisplay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summarized => "summarized",
            Self::Omitted => "omitted",
        }
    }
}

// ===========================================================================
// [storage]
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Defaults to `<data_dir>/history.sqlite3`.
    pub path: Option<PathBuf>,
    /// Drop play events older than this. 0 disables pruning.
    pub retain_plays_days: u32,
    /// Drop recommendation records older than this. Keep this comfortably
    /// larger than `defaults.exclude_recent_days` or exclusions go stale.
    pub retain_recommendations_days: u32,
    /// Skip a `sync` if the last successful one was this recent.
    pub sync_min_interval_mins: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            path: None,
            retain_plays_days: 730,
            // The recommendation log is what stops the agent repeating itself.
            // It is tiny (a few hundred bytes per run), so the default is to
            // keep it indefinitely.
            retain_recommendations_days: 0,
            sync_min_interval_mins: 30,
        }
    }
}

// ===========================================================================
// [defaults] — run parameters a preset may override
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunDefaults {
    pub preset: String,
    /// Final playlist length.
    pub size: usize,
    pub strategy: FillStrategy,
    /// `{preset}`, `{date}`, `{datetime}` are substituted.
    pub playlist_name: String,
    pub playlist_description: String,
    pub playlist_public: bool,
    pub language: LanguagePolicy,
    /// Do not re-suggest anything recommended within this many days.
    ///
    /// `0` means **forever**: nothing the agent has ever put in a playlist is
    /// offered again. That is the setting to use if repeats are the thing you
    /// care most about avoiding.
    pub exclude_recent_days: u32,

    /// Do not recommend *any* track by an artist used within this many days.
    ///
    /// Track-level exclusion alone still lets a run come back with a different
    /// song by the same five artists every week, which is what "going in
    /// circles" actually feels like. `0` disables the cooldown.
    pub artist_cooldown_days: u32,
    /// Exclude the user's Liked Songs from suggestions.
    pub exclude_saved: bool,
    /// Exclude anything seen in recent play history.
    pub exclude_played: bool,
    /// Cap tracks per artist so one act cannot dominate the playlist.
    pub max_per_artist: usize,
    /// 0–10. How far from the listener's established taste to reach:
    /// 0 is "play me the hits I already half-know", 10 is deep underground.
    /// Shapes the prompt *and* the popularity band applied afterwards.
    pub discovery_level: u8,

    /// Rolling strategy only: how many of the existing tracks may be evicted
    /// in one run. Keeps turnover gradual rather than replacing the whole
    /// buffer the first time feedback arrives.
    pub rolling_max_evictions: usize,

    /// Rolling strategy only: never evict a track added within this many days,
    /// so a new pick gets a fair hearing before it can be thrown out.
    pub rolling_grace_days: u32,

    /// Ask Claude for `size * oversample` candidates: some will fail to
    /// resolve on Spotify or be filtered out, and a second round-trip is the
    /// expensive thing to avoid.
    pub oversample: f32,
    /// How much of the taste profile to put in the prompt.
    pub profile_top_artists: usize,
    pub profile_top_tracks: usize,
    pub profile_top_genres: usize,
    /// Cap on the exclusion list sent to the model (newest first).
    pub prompt_exclusion_limit: usize,
}

impl Default for RunDefaults {
    fn default() -> Self {
        Self {
            preset: "discover".into(),
            size: 30,
            strategy: FillStrategy::Replace,
            playlist_name: "AI · {preset}".into(),
            playlist_description: "Curated by spotify-agent · {datetime}".into(),
            playlist_public: false,
            language: LanguagePolicy::Any,
            exclude_recent_days: 0,
            artist_cooldown_days: 21,
            exclude_saved: true,
            exclude_played: false,
            max_per_artist: 2,
            discovery_level: 5,
            rolling_max_evictions: 10,
            rolling_grace_days: 7,
            oversample: 1.8,
            profile_top_artists: 40,
            profile_top_tracks: 40,
            profile_top_genres: 20,
            prompt_exclusion_limit: 400,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FillStrategy {
    /// Replace the playlist contents wholesale.
    Replace,
    /// Append, skipping anything already in the playlist.
    Append,
    /// Keep the playlist at exactly `size` tracks: add the new picks and evict
    /// the least-engaging incumbents to make room. The playlist becomes a
    /// rolling buffer that turns over gradually instead of being rebuilt.
    Rolling,
}

impl FillStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Append => "append",
            Self::Rolling => "rolling",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LanguagePolicy {
    /// Latin-script titles only.
    English,
    /// Cyrillic-script titles only.
    Russian,
    /// Both, roughly balanced — the model is asked for a mix and the filter
    /// accepts either script.
    Mixed,
    /// No constraint.
    Any,
}

impl LanguagePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::English => "english",
            Self::Russian => "russian",
            Self::Mixed => "mixed",
            Self::Any => "any",
        }
    }
}

// ===========================================================================
// [filters]
// ===========================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Filters {
    /// Genre substrings that must match at least one of the artist's genres.
    /// Empty = no restriction. Matching is case-insensitive substring, because
    /// Spotify's genre vocabulary is long-tailed ("russian post-punk").
    pub genres_include: Vec<String>,
    pub genres_exclude: Vec<String>,
    /// If non-empty, only these artists are allowed through.
    pub artists_allow: Vec<String>,
    pub artists_block: Vec<String>,
    pub min_popularity: Option<u8>,
    pub max_popularity: Option<u8>,
    pub allow_explicit: Option<bool>,
    /// Reject tracks shorter/longer than these bounds (0 = unset).
    pub min_duration_secs: u32,
    pub max_duration_secs: u32,
}

impl Filters {
    /// Preset filters override list fields when non-empty and scalar fields
    /// when `Some`, so a preset can narrow but does not have to restate the
    /// global configuration.
    pub fn merged_with(&self, over: &Filters) -> Filters {
        Filters {
            genres_include: pick_vec(&self.genres_include, &over.genres_include),
            genres_exclude: union_vec(&self.genres_exclude, &over.genres_exclude),
            artists_allow: pick_vec(&self.artists_allow, &over.artists_allow),
            artists_block: union_vec(&self.artists_block, &over.artists_block),
            min_popularity: over.min_popularity.or(self.min_popularity),
            max_popularity: over.max_popularity.or(self.max_popularity),
            allow_explicit: over.allow_explicit.or(self.allow_explicit),
            min_duration_secs: if over.min_duration_secs > 0 {
                over.min_duration_secs
            } else {
                self.min_duration_secs
            },
            max_duration_secs: if over.max_duration_secs > 0 {
                over.max_duration_secs
            } else {
                self.max_duration_secs
            },
        }
    }
}

fn pick_vec(base: &[String], over: &[String]) -> Vec<String> {
    if over.is_empty() {
        base.to_vec()
    } else {
        over.to_vec()
    }
}

/// Blocklists are additive: a preset must not be able to silently un-block an
/// artist the user blocked globally.
fn union_vec(base: &[String], over: &[String]) -> Vec<String> {
    let mut out = base.to_vec();
    for item in over {
        if !out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(item))
        {
            out.push(item.clone());
        }
    }
    out
}

// ===========================================================================
// [presets.*]
// ===========================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preset {
    /// One-line label for the TUI selector.
    pub label: String,
    /// Free-text brief handed to Claude verbatim. This is the highest-leverage
    /// field in the whole config: it is what actually shapes the selection.
    pub brief: String,
    /// Optional seed adjectives, used for the playlist description and to
    /// nudge the model's mood vocabulary.
    pub moods: Vec<String>,

    // --- overrides over [defaults] ---
    pub size: Option<usize>,
    pub strategy: Option<FillStrategy>,
    pub playlist_name: Option<String>,
    pub playlist_description: Option<String>,
    pub playlist_public: Option<bool>,
    pub language: Option<LanguagePolicy>,
    pub exclude_recent_days: Option<u32>,
    pub artist_cooldown_days: Option<u32>,
    pub exclude_saved: Option<bool>,
    pub exclude_played: Option<bool>,
    pub max_per_artist: Option<usize>,
    pub discovery_level: Option<u8>,
    pub rolling_max_evictions: Option<usize>,
    pub rolling_grace_days: Option<u32>,
    pub oversample: Option<f32>,

    // --- overrides over [filters] ---
    #[serde(default)]
    pub filters: Filters,
}

/// A preset flattened against the global defaults — what the engine consumes.
#[derive(Debug, Clone)]
pub struct ResolvedPreset {
    pub name: String,
    pub label: String,
    pub brief: String,
    pub moods: Vec<String>,
    pub run: RunDefaults,
    pub filters: Filters,
}

impl Config {
    /// Flatten `presets[name]` over `[defaults]` and `[filters]`.
    pub fn resolve_preset(&self, name: &str) -> Result<ResolvedPreset> {
        let preset = self.presets.get(name).ok_or_else(|| {
            let known = self.presets.keys().cloned().collect::<Vec<_>>().join(", ");
            AgentError::config(format!("unknown preset `{name}` (known: {known})"))
        })?;

        let d = &self.defaults;
        let run = RunDefaults {
            preset: name.to_string(),
            size: preset.size.unwrap_or(d.size),
            strategy: preset.strategy.unwrap_or(d.strategy),
            playlist_name: preset
                .playlist_name
                .clone()
                .unwrap_or_else(|| d.playlist_name.clone()),
            playlist_description: preset
                .playlist_description
                .clone()
                .unwrap_or_else(|| d.playlist_description.clone()),
            playlist_public: preset.playlist_public.unwrap_or(d.playlist_public),
            language: preset.language.unwrap_or(d.language),
            exclude_recent_days: preset.exclude_recent_days.unwrap_or(d.exclude_recent_days),
            artist_cooldown_days: preset
                .artist_cooldown_days
                .unwrap_or(d.artist_cooldown_days),
            exclude_saved: preset.exclude_saved.unwrap_or(d.exclude_saved),
            exclude_played: preset.exclude_played.unwrap_or(d.exclude_played),
            max_per_artist: preset.max_per_artist.unwrap_or(d.max_per_artist),
            discovery_level: preset.discovery_level.unwrap_or(d.discovery_level).min(10),
            rolling_max_evictions: preset
                .rolling_max_evictions
                .unwrap_or(d.rolling_max_evictions),
            rolling_grace_days: preset.rolling_grace_days.unwrap_or(d.rolling_grace_days),
            oversample: preset.oversample.unwrap_or(d.oversample),
            profile_top_artists: d.profile_top_artists,
            profile_top_tracks: d.profile_top_tracks,
            profile_top_genres: d.profile_top_genres,
            prompt_exclusion_limit: d.prompt_exclusion_limit,
        };

        Ok(ResolvedPreset {
            name: name.to_string(),
            label: if preset.label.is_empty() {
                name.to_string()
            } else {
                preset.label.clone()
            },
            brief: preset.brief.clone(),
            moods: preset.moods.clone(),
            run,
            filters: self.filters.merged_with(&preset.filters),
        })
    }

    pub fn preset_names(&self) -> Vec<String> {
        self.presets.keys().cloned().collect()
    }
}

// ===========================================================================
// [feedback]
// ===========================================================================

/// The closed feedback loop: what the agent infers from how its picks fared.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeedbackConfig {
    pub enabled: bool,

    /// Fraction of a track that must play before it counts as listened rather
    /// than skipped. Spotify exposes no skip event, so this is inferred from
    /// the gap to the next play — see `engine::feedback` for the caveats.
    pub skip_ratio: f32,

    /// A track sitting unplayed in a managed playlist this long earns a mild
    /// negative signal. 0 disables the staleness check.
    pub stale_after_days: u32,

    /// Signals lose half their weight over this many days. 0 disables decay.
    /// Taste moves; a skip from last year should not still be steering picks.
    pub half_life_days: f32,

    // Signal weights. Positive is approval. The explicit TUI signals are
    // stronger than the inferred ones on purpose.
    pub weight_liked: f32,
    pub weight_played: f32,
    pub weight_skipped: f32,
    pub weight_removed: f32,
    pub weight_stale: f32,
    pub weight_up: f32,
    pub weight_down: f32,

    /// Artist score at or above this is described to the model as resonating.
    pub boost_threshold: f32,
    /// Artist score at or below this is named as one to avoid.
    pub avoid_threshold: f32,

    /// Put the verdicts in the prompt. Turning this off keeps collecting
    /// signals but stops them influencing generation.
    pub apply_to_prompt: bool,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            skip_ratio: 0.6,
            stale_after_days: 21,
            half_life_days: 120.0,
            weight_liked: 3.0,
            weight_played: 0.6,
            // Smaller magnitude than an explicit thumbs-down: the skip is
            // inferred and over-reports (a long pause looks like a skip).
            weight_skipped: -1.0,
            weight_removed: -3.0,
            weight_stale: -0.4,
            weight_up: 2.5,
            weight_down: -2.5,
            boost_threshold: 2.0,
            avoid_threshold: -2.0,
            apply_to_prompt: true,
        }
    }
}

// ===========================================================================
// [notifications]
// ===========================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationConfig {
    /// Desktop notification when a background run finishes.
    pub desktop: bool,
    pub on_success: bool,
    pub on_failure: bool,
    /// Telegram bot API, Discord webhook, Slack webhook, or any endpoint that
    /// accepts a JSON POST. Leave empty to disable.
    pub webhook_url: Option<Secret>,
    pub webhook_kind: WebhookKind,
    /// Required for `telegram`.
    pub telegram_chat_id: Option<String>,
    pub timeout_secs: u64,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            desktop: true,
            on_success: true,
            on_failure: true,
            webhook_url: None,
            webhook_kind: WebhookKind::Auto,
            telegram_chat_id: None,
            timeout_secs: 15,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebhookKind {
    /// Detect from the URL host.
    Auto,
    Telegram,
    Discord,
    Slack,
    /// POST `{"text": "..."}` and let the receiver decide.
    Generic,
}

// ===========================================================================
// Loading
// ===========================================================================

impl Config {
    /// Load from `explicit` or the platform default path. A missing file is
    /// not an error — defaults + environment may be enough to run.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => paths::config_file()?,
        };

        let mut cfg = if path.exists() {
            let permission_warning = crate::util::fs::world_readable_warning(&path);
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| AgentError::io(path.display().to_string(), e))?;
            let mut parsed: Config = toml::from_str(&raw)
                .map_err(|e| AgentError::config(format!("{}: {e}", path.display())))?;
            parsed.source_path = Some(path.clone());
            parsed.warnings.extend(permission_warning);
            parsed
        } else if explicit.is_some() {
            return Err(AgentError::config(format!(
                "config file not found: {}",
                path.display()
            )));
        } else {
            Config::default()
        };

        if cfg.presets.is_empty() {
            cfg.presets = presets::builtin();
        } else {
            // User presets extend rather than replace the built-ins, so a
            // config that defines one preset does not lose the other five.
            for (name, preset) in presets::builtin() {
                cfg.presets.entry(name).or_insert(preset);
            }
        }

        cfg.apply_env()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Environment overrides. Two families are supported:
    ///   * well-known names (`ANTHROPIC_API_KEY`, `SPOTIFY_CLIENT_ID`, …)
    ///   * `SPOTIFY_AGENT__<SECTION>__<FIELD>` for everything else
    fn apply_env(&mut self) -> Result<()> {
        if let Ok(v) = std::env::var("SPOTIFY_CLIENT_ID")
            && !v.trim().is_empty()
        {
            self.spotify.client_id = Some(v.trim().to_string());
        }
        if let Ok(v) = std::env::var("SPOTIFY_CLIENT_SECRET")
            && !v.trim().is_empty()
        {
            self.spotify.client_secret = Some(Secret::new(v.trim()));
        }
        if let Ok(v) = std::env::var("SPOTIFY_REDIRECT_URI") {
            self.apply_redirect_uri(&v)?;
        }

        // `ANTHROPIC_API_KEY` is read lazily by `resolve_api_key` so it is
        // never copied into a long-lived struct unnecessarily.

        // Collected first so the loop does not hold an iterator over the
        // environment while `set_field` mutates `self`.
        let overrides: Vec<(String, String)> = std::env::vars().collect();
        for (key, value) in overrides {
            let Some(rest) = key.strip_prefix(ENV_PREFIX) else {
                continue;
            };
            let lowered = rest.to_ascii_lowercase();
            let mut parts = lowered.split("__");
            let (Some(section), Some(field)) = (parts.next(), parts.next()) else {
                self.warnings.push(format!(
                    "ignoring env override {key}: expected {ENV_PREFIX}SECTION__FIELD"
                ));
                continue;
            };
            if let Err(e) = self.set_field(section, field, &value) {
                self.warnings
                    .push(format!("ignoring env override {key}: {e}"));
            }
        }
        Ok(())
    }

    fn apply_redirect_uri(&mut self, raw: &str) -> Result<()> {
        let parsed = url::Url::parse(raw.trim())
            .map_err(|e| AgentError::config(format!("SPOTIFY_REDIRECT_URI: {e}")))?;
        self.spotify.redirect_host = parsed
            .host_str()
            .ok_or_else(|| AgentError::config("SPOTIFY_REDIRECT_URI has no host"))?
            .to_string();
        self.spotify.redirect_port = parsed.port().unwrap_or(80);
        self.spotify.redirect_path = parsed.path().to_string();
        Ok(())
    }

    /// Typed setter used by the env-override walker. Kept explicit rather than
    /// reflective: a typo'd env var produces a warning, never a silent no-op.
    fn set_field(&mut self, section: &str, field: &str, value: &str) -> Result<()> {
        let bad = |what: &str| AgentError::config(format!("cannot parse `{value}` as {what}"));
        match (section, field) {
            ("general", "log_level") => self.general.log_level = value.into(),
            ("general", "log_format") => {
                self.general.log_format = match value {
                    "json" => LogFormat::Json,
                    "text" => LogFormat::Text,
                    _ => return Err(bad("log format (text|json)")),
                }
            }
            ("general", "data_dir") => self.general.data_dir = Some(PathBuf::from(value)),
            ("general", "language") => {
                self.general.language = Some(
                    crate::i18n::Lang::parse(value).ok_or_else(|| bad("language (en|ru|pl|lt)"))?,
                )
            }

            ("spotify", "client_id") => self.spotify.client_id = Some(value.into()),
            ("spotify", "client_secret") => self.spotify.client_secret = Some(Secret::new(value)),
            ("spotify", "market") => self.spotify.market = Some(value.to_uppercase()),
            ("spotify", "redirect_port") => {
                self.spotify.redirect_port = value.parse().map_err(|_| bad("port"))?
            }
            ("spotify", "concurrency") => {
                self.spotify.concurrency = value.parse().map_err(|_| bad("integer"))?
            }

            ("claude", "api_key") => self.claude.api_key = Some(Secret::new(value)),
            ("claude", "api_key_env") => self.claude.api_key_env = Some(value.into()),
            ("claude", "model") => self.claude.model = value.into(),
            ("claude", "base_url") => {
                self.claude.base_url = Some(value.trim_end_matches('/').into())
            }
            ("claude", "max_tokens") => {
                self.claude.max_tokens = value.parse().map_err(|_| bad("integer"))?
            }
            ("claude", "stream") => {
                self.claude.stream = parse_bool(value).ok_or_else(|| bad("bool"))?
            }
            ("claude", "provider") => {
                self.claude.provider = match value {
                    "anthropic" => Provider::Anthropic,
                    "openai" => Provider::Openai,
                    "ollama" => Provider::Ollama,
                    "gemini" => Provider::Gemini,
                    _ => return Err(bad("provider (anthropic|openai|ollama|gemini)")),
                }
            }
            ("claude", "effort") => {
                self.claude.effort = match value {
                    "low" => Effort::Low,
                    "medium" => Effort::Medium,
                    "high" => Effort::High,
                    "xhigh" => Effort::Xhigh,
                    "max" => Effort::Max,
                    _ => return Err(bad("effort (low|medium|high|xhigh|max)")),
                }
            }

            ("storage", "path") => self.storage.path = Some(PathBuf::from(value)),

            ("defaults", "preset") => self.defaults.preset = value.into(),
            ("defaults", "size") => {
                self.defaults.size = value.parse().map_err(|_| bad("integer"))?
            }
            ("defaults", "playlist_name") => self.defaults.playlist_name = value.into(),
            ("defaults", "language") => {
                self.defaults.language = parse_language(value).ok_or_else(|| bad("language"))?
            }
            ("defaults", "strategy") => {
                self.defaults.strategy = match value {
                    "replace" => FillStrategy::Replace,
                    "append" => FillStrategy::Append,
                    _ => return Err(bad("strategy (replace|append)")),
                }
            }

            _ => {
                return Err(AgentError::config(format!(
                    "unknown setting `{section}.{field}`"
                )));
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.defaults.size == 0 || self.defaults.size > 500 {
            return Err(AgentError::config(
                "defaults.size must be between 1 and 500 (Spotify caps a playlist write at 100 per request; larger sizes are chunked)",
            ));
        }
        if self.defaults.discovery_level > 10 {
            return Err(AgentError::config("defaults.discovery_level must be 0–10"));
        }
        if self.feedback.skip_ratio <= 0.0 || self.feedback.skip_ratio >= 1.0 {
            return Err(AgentError::config(
                "feedback.skip_ratio must be between 0 and 1 (exclusive) — it is the fraction of a track that counts as listened",
            ));
        }
        if !(1.0..=5.0).contains(&self.defaults.oversample) {
            return Err(AgentError::config(
                "defaults.oversample must be in [1.0, 5.0]",
            ));
        }
        if self.spotify.concurrency == 0 || self.spotify.concurrency > 16 {
            return Err(AgentError::config("spotify.concurrency must be in [1, 16]"));
        }
        self.claude.validate("claude")?;
        for (index, backend) in self.llm.fallbacks.iter().enumerate() {
            backend.validate(&format!("llm.fallbacks[{index}]"))?;
        }
        if let Some(market) = &self.spotify.market
            && (market.len() != 2 || !market.chars().all(|c| c.is_ascii_alphabetic()))
        {
            return Err(AgentError::config(
                "spotify.market must be an ISO-3166-1 alpha-2 code, e.g. \"DE\"",
            ));
        }
        if self.defaults.exclude_recent_days == 0 && self.storage.retain_recommendations_days != 0 {
            // Pruning the recommendation log is what makes the agent forget;
            // with `exclude_recent_days = 0` the user asked it never to
            // forget, so the two settings contradict each other.
            return Err(AgentError::config(
                "defaults.exclude_recent_days = 0 means \"never repeat a recommendation\", but \
                 storage.retain_recommendations_days would delete the history that enforces it — \
                 set storage.retain_recommendations_days = 0 to keep it forever",
            ));
        }
        if !self.presets.contains_key(&self.defaults.preset) {
            return Err(AgentError::config(format!(
                "defaults.preset = `{}` is not defined",
                self.defaults.preset
            )));
        }
        Ok(())
    }

    /// Spotify client id, or a directed error.
    pub fn require_client_id(&self) -> Result<&str> {
        self.spotify
            .client_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| AgentError::MissingCredential {
                name: "spotify client_id",
                hint: "create an app at https://developer.spotify.com/dashboard, then set spotify.client_id or export SPOTIFY_CLIENT_ID".into(),
            })
    }

    pub fn data_dir(&self) -> Result<PathBuf> {
        match &self.general.data_dir {
            Some(p) => Ok(p.clone()),
            None => paths::data_dir(),
        }
    }

    pub fn database_path(&self) -> Result<PathBuf> {
        match &self.storage.path {
            Some(p) => Ok(p.clone()),
            None => Ok(self.data_dir()?.join("history.sqlite3")),
        }
    }

    pub fn token_path(&self) -> Result<PathBuf> {
        Ok(self.data_dir()?.join("tokens.json"))
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_language(v: &str) -> Option<LanguagePolicy> {
    match v.to_ascii_lowercase().as_str() {
        "english" | "en" => Some(LanguagePolicy::English),
        "russian" | "ru" => Some(LanguagePolicy::Russian),
        "mixed" => Some(LanguagePolicy::Mixed),
        "any" => Some(LanguagePolicy::Any),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_builtin_presets() -> Config {
        Config {
            presets: presets::builtin(),
            ..Default::default()
        }
    }

    #[test]
    fn defaults_validate() {
        assert!(with_builtin_presets().validate().is_ok());
    }

    #[test]
    fn a_key_pasted_into_api_key_env_is_caught() {
        // The field takes a variable NAME; a key there fails silently at
        // lookup time, which is a miserable thing to debug.
        let mut cfg = with_builtin_presets();
        cfg.claude.api_key_env = Some("sk-ant-api03-abcdef".into());
        let error = cfg.validate().expect_err("should be rejected");
        let rendered = error.to_string();
        assert!(rendered.contains("api_key_env"), "{rendered}");
        assert!(rendered.contains("api_key"), "should point at the right field: {rendered}");

        // A real variable name is fine.
        cfg.claude.api_key_env = Some("ANTHROPIC_API_KEY".into());
        assert!(cfg.validate().is_ok());

        // So is leaving it unset.
        cfg.claude.api_key_env = None;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn a_gemini_style_key_in_api_key_env_is_caught_too() {
        let mut cfg = with_builtin_presets();
        cfg.llm.fallbacks = vec![BackendConfig {
            provider: Provider::Gemini,
            api_key_env: Some("AQ.Ab8RN6LHyH1GSc6G2XUVDonXyYBvD1mlWq".into()),
            ..Default::default()
        }];
        let error = cfg.validate().expect_err("should be rejected");
        assert!(error.to_string().contains("llm.fallbacks[0].api_key_env"), "{error}");
    }

    #[test]
    fn rejects_disabled_thinking_at_max_effort() {
        let mut cfg = with_builtin_presets();
        cfg.claude.thinking = ThinkingMode::Disabled;
        cfg.claude.effort = Effort::Max;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn preset_blocklists_are_additive() {
        let base = Filters {
            artists_block: vec!["A".into()],
            ..Default::default()
        };
        let over = Filters {
            artists_block: vec!["B".into()],
            ..Default::default()
        };
        let merged = base.merged_with(&over);
        assert_eq!(merged.artists_block, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn preset_overrides_defaults() {
        let mut cfg = with_builtin_presets();
        cfg.defaults.size = 30;
        cfg.presets.insert(
            "tiny".into(),
            Preset {
                size: Some(5),
                ..Default::default()
            },
        );
        let resolved = cfg.resolve_preset("tiny").expect("preset resolves");
        assert_eq!(resolved.run.size, 5);
        assert_eq!(resolved.run.playlist_name, cfg.defaults.playlist_name);
    }
}
