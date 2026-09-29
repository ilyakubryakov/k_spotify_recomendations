//! Interface localisation.
//!
//! Scope is deliberate: the **interface** is translated — TUI labels, the help
//! and setup pane, the first-run wizard, and the keybinding hints. Log records
//! and `--json` output stay in English, because those are machine-facing and
//! are read by tooling, grep and bug reports.
//!
//! Strings are a `struct` of `&'static str` fields rather than a runtime map,
//! so adding a string is a compile error in every language that has not
//! supplied it. There is no way to ship a half-translated build by accident.
//!
//! Translation status: English and Russian are authored; Polish and Lithuanian
//! are functional but would benefit from a native speaker's pass — see
//! `docs/i18n.md`.

mod lt;
mod pl;
mod ru;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    En,
    Ru,
    Pl,
    Lt,
}

impl Lang {
    pub const ALL: [Lang; 4] = [Lang::En, Lang::Ru, Lang::Pl, Lang::Lt];

    /// ISO 639-1 code, as written in config.
    pub fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ru => "ru",
            Self::Pl => "pl",
            Self::Lt => "lt",
        }
    }

    /// The language's name *in that language* — the only sensible way to
    /// render a language picker, since the reader may not know the others.
    pub fn endonym(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::Ru => "Русский",
            Self::Pl => "Polski",
            Self::Lt => "Lietuvių",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        // Accept a full locale ("ru_RU.UTF-8") by looking at the prefix only.
        let code = value
            .split(['_', '-', '.'])
            .next()
            .unwrap_or(value)
            .to_ascii_lowercase();
        match code.as_str() {
            "en" | "english" => Some(Self::En),
            "ru" | "русский" | "russian" => Some(Self::Ru),
            "pl" | "polski" | "polish" => Some(Self::Pl),
            "lt" | "lietuvių" | "lietuviu" | "lithuanian" => Some(Self::Lt),
            _ => None,
        }
    }

    /// Best guess from the environment, for the first-run default.
    pub fn from_environment() -> Option<Self> {
        for var in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
            if let Ok(value) = std::env::var(var) {
                if let Some(lang) = Lang::parse(&value) {
                    return Some(lang);
                }
            }
        }
        None
    }

    pub fn strings(self) -> &'static Strings {
        match self {
            Self::En => &EN,
            Self::Ru => &ru::RU,
            Self::Pl => &pl::PL,
            Self::Lt => &lt::LT,
        }
    }
}

/// Every translatable interface string.
///
/// Field names are grouped by where they appear. Keep them short and literal;
/// a field that needs a sentence of explanation belongs in the help pane.
#[derive(Debug)]
pub struct Strings {
    // --- header ---
    pub model: &'static str,
    pub effort: &'static str,
    pub authorised: &'static str,
    pub not_authorised: &'static str,

    // --- panels ---
    pub presets: &'static str,
    pub run_options: &'static str,
    pub tracks: &'static str,
    pub activity: &'static str,
    pub reasoning: &'static str,
    pub profile: &'static str,
    pub logs: &'static str,
    pub overview: &'static str,
    pub core_artists: &'static str,
    pub genres: &'static str,
    pub on_repeat: &'static str,
    pub pipeline: &'static str,
    pub setup: &'static str,
    pub help_title: &'static str,
    pub settings: &'static str,

    // --- run options ---
    pub size: &'static str,
    pub language: &'static str,
    pub mode: &'static str,
    pub dry_run: &'static str,
    pub discovery: &'static str,
    pub on: &'static str,
    pub off: &'static str,

    // --- profile ---
    pub library: &'static str,
    pub plays: &'static str,
    pub era: &'static str,
    pub languages: &'static str,
    pub typical: &'static str,
    pub nothing_on_repeat: &'static str,

    // --- status / hints ---
    pub idle: &'static str,
    pub working: &'static str,
    pub select_preset_hint: &'static str,
    pub empty_cache_hint: &'static str,
    pub no_profile_hint: &'static str,
    pub not_authorised_hint: &'static str,
    pub already_running: &'static str,
    pub wait_for_run: &'static str,
    pub terminal_too_small: &'static str,
    pub log_capture_off: &'static str,
    pub why_this_track: &'static str,

    // --- stages ---
    pub stage_sync: &'static str,
    pub stage_analyze: &'static str,
    pub stage_prompt: &'static str,
    pub stage_model: &'static str,
    pub stage_resolve: &'static str,
    pub stage_select: &'static str,
    pub stage_publish: &'static str,
    pub stage_done: &'static str,

    // --- keys (footer + help) ---
    pub key_navigate: &'static str,
    pub key_generate: &'static str,
    pub key_sync: &'static str,
    pub key_reload: &'static str,
    pub key_dry_run: &'static str,
    pub key_view: &'static str,
    pub key_help: &'static str,
    pub key_quit: &'static str,
    pub key_review: &'static str,
    pub key_keep: &'static str,
    pub key_drop: &'static str,
    pub key_ban: &'static str,
    pub key_up: &'static str,
    pub key_down: &'static str,
    pub key_size: &'static str,
    pub key_language: &'static str,
    pub key_mode: &'static str,
    pub key_scroll: &'static str,
    pub key_mouse: &'static str,
    pub key_settings: &'static str,
    pub any_key_closes: &'static str,

    // --- help sections ---
    pub help_navigation: &'static str,
    pub help_actions: &'static str,
    pub help_moderation: &'static str,

    // --- verdicts ---
    pub verdict_keep: &'static str,
    pub verdict_drop: &'static str,
    pub verdict_ban: &'static str,
    pub verdict_up: &'static str,
    pub verdict_down: &'static str,

    // --- setup pane ---
    pub setup_intro: &'static str,
    pub setup_spotify_title: &'static str,
    pub setup_spotify_1: &'static str,
    pub setup_spotify_2: &'static str,
    pub setup_spotify_3: &'static str,
    pub setup_spotify_4: &'static str,
    pub setup_llm_title: &'static str,
    pub setup_llm_1: &'static str,
    pub setup_llm_2: &'static str,
    pub setup_llm_3: &'static str,
    pub setup_llm_local: &'static str,
    pub setup_config_at: &'static str,
    pub setup_done: &'static str,
    pub setup_missing: &'static str,

    // --- first-run wizard ---
    pub wizard_title: &'static str,
    pub wizard_prompt: &'static str,
    pub wizard_hint: &'static str,
    pub wizard_saved: &'static str,
}

/// English — the reference translation. Every other language mirrors it.
pub static EN: Strings = Strings {
    model: "model",
    effort: "effort",
    authorised: "authorised",
    not_authorised: "not authorised",

    presets: "Presets",
    run_options: "Run options",
    tracks: "Tracks",
    activity: "Activity",
    reasoning: "Reasoning",
    profile: "Profile",
    logs: "Logs",
    overview: "Overview",
    core_artists: "Core artists",
    genres: "Genres",
    on_repeat: "On repeat",
    pipeline: "Pipeline",
    setup: "Setup",
    help_title: "Keys",
    settings: "Settings",

    size: "size",
    language: "language",
    mode: "mode",
    dry_run: "dry run",
    discovery: "discovery",
    on: "on",
    off: "off",

    library: "library",
    plays: "plays",
    era: "era",
    languages: "languages",
    typical: "typical",
    nothing_on_repeat: "nothing on repeat",

    idle: "idle",
    working: "working…",
    select_preset_hint: "Select a preset and press Enter to generate.",
    empty_cache_hint: "cache is empty — press s to sync your library",
    no_profile_hint: "No profile yet — press s to sync, then r to rebuild.",
    not_authorised_hint: "not authorised — quit and run `spotify-agent login`",
    already_running: "already running",
    wait_for_run: "wait for the run to finish",
    terminal_too_small: "Terminal too small — needs at least 70×18.",
    log_capture_off: "log capture is not active in this mode",
    why_this_track: "Why this track",

    stage_sync: "Syncing library",
    stage_analyze: "Analysing taste",
    stage_prompt: "Building prompt",
    stage_model: "Consulting the model",
    stage_resolve: "Resolving tracks",
    stage_select: "Applying filters",
    stage_publish: "Writing playlist",
    stage_done: "Done",

    key_navigate: "move",
    key_generate: "generate",
    key_sync: "sync",
    key_reload: "reload",
    key_dry_run: "dry-run",
    key_view: "view",
    key_help: "help",
    key_quit: "quit",
    key_review: "review list",
    key_keep: "save to Liked Songs",
    key_drop: "remove from playlist",
    key_ban: "ban artist",
    key_up: "thumbs up",
    key_down: "thumbs down",
    key_size: "size ±5",
    key_language: "cycle language policy",
    key_mode: "replace / append / rolling",
    key_scroll: "scroll",
    key_mouse: "click to select, wheel to scroll",
    key_settings: "settings",
    any_key_closes: "any key closes this panel",

    help_navigation: "Navigation",
    help_actions: "Actions",
    help_moderation: "Reviewing a playlist",

    verdict_keep: "saved to Liked Songs",
    verdict_drop: "removed from the playlist",
    verdict_ban: "artist banned",
    verdict_up: "thumbs up",
    verdict_down: "thumbs down",

    setup_intro: "Two credentials are needed. Neither costs anything to create.",
    setup_spotify_title: "Spotify",
    setup_spotify_1: "1. Open developer.spotify.com/dashboard and create an app.",
    setup_spotify_2: "2. In its settings add the redirect URI shown below, exactly.",
    setup_spotify_3: "3. Copy the Client ID into the config, or export SPOTIFY_CLIENT_ID.",
    setup_spotify_4: "4. Quit and run: spotify-agent login",
    setup_llm_title: "Model provider",
    setup_llm_1: "Anthropic: console.anthropic.com → API keys → export ANTHROPIC_API_KEY",
    setup_llm_2: "OpenAI: platform.openai.com/api-keys → export OPENAI_API_KEY",
    setup_llm_3: "Google: aistudio.google.com/apikey → export GEMINI_API_KEY",
    setup_llm_local: "Or run everything locally: install Ollama and add it under [[llm.fallbacks]].",
    setup_config_at: "config file",
    setup_done: "ready",
    setup_missing: "missing",

    wizard_title: "Choose your language",
    wizard_prompt: "Use ↑ ↓ or click, then press Enter.",
    wizard_hint: "You can change this later in Settings.",
    wizard_saved: "Language saved.",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_round_trip() {
        for lang in Lang::ALL {
            assert_eq!(Lang::parse(lang.code()), Some(lang));
        }
    }

    #[test]
    fn full_locales_are_accepted() {
        assert_eq!(Lang::parse("ru_RU.UTF-8"), Some(Lang::Ru));
        assert_eq!(Lang::parse("pl-PL"), Some(Lang::Pl));
        assert_eq!(Lang::parse("lt_LT.utf8"), Some(Lang::Lt));
        assert_eq!(Lang::parse("en_GB"), Some(Lang::En));
        assert_eq!(Lang::parse("de_DE"), None);
    }

    #[test]
    fn every_language_supplies_every_string() {
        // The struct makes this a compile-time guarantee; this checks the
        // weaker runtime property that nothing was filled in with a blank.
        for lang in Lang::ALL {
            let s = lang.strings();
            let fields: [(&str, &str); 18] = [
                ("presets", s.presets),
                ("tracks", s.tracks),
                ("activity", s.activity),
                ("profile", s.profile),
                ("logs", s.logs),
                ("setup", s.setup),
                ("settings", s.settings),
                ("size", s.size),
                ("language", s.language),
                ("mode", s.mode),
                ("stage_model", s.stage_model),
                ("stage_done", s.stage_done),
                ("key_generate", s.key_generate),
                ("key_quit", s.key_quit),
                ("key_ban", s.key_ban),
                ("wizard_title", s.wizard_title),
                ("setup_intro", s.setup_intro),
                ("verdict_keep", s.verdict_keep),
            ];
            for (name, value) in fields {
                assert!(
                    !value.trim().is_empty(),
                    "{} has an empty `{name}`",
                    lang.code()
                );
            }
        }
    }

    #[test]
    fn endonyms_are_distinct() {
        let names: std::collections::HashSet<&str> =
            Lang::ALL.iter().map(|l| l.endonym()).collect();
        assert_eq!(names.len(), Lang::ALL.len());
    }

    #[test]
    fn translations_differ_from_english_where_it_matters() {
        // A copy-paste that left English behind is the likeliest translation
        // bug, and it is invisible without a check like this.
        for lang in [Lang::Ru, Lang::Pl, Lang::Lt] {
            let s = lang.strings();
            assert_ne!(
                s.presets,
                EN.presets,
                "{} did not translate `presets`",
                lang.code()
            );
            assert_ne!(
                s.key_quit,
                EN.key_quit,
                "{} did not translate `key_quit`",
                lang.code()
            );
        }
    }
}
