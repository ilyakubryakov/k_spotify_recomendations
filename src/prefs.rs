//! Per-user interface preferences.
//!
//! Kept in `<data_dir>/preferences.json`, deliberately *not* in `config.toml`.
//! The config file is hand-written and heavily commented; rewriting it from the
//! program would destroy those comments and — because `Secret` serialises as
//! `"<redacted>"` — could silently overwrite a stored key with a placeholder.
//! Preferences are small, machine-owned, and safe to rewrite.
//!
//! `config.toml` still wins if it sets `general.language`: an explicit choice
//! in the file the user edits should not be overridable by a UI click.

use crate::error::{AgentError, Result};
use crate::i18n::Lang;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// Interface language chosen in the UI.
    pub language: Option<Lang>,
    /// Whether the first-run language wizard has been completed, so it is not
    /// shown again to someone who deliberately kept the default.
    pub language_chosen: bool,
}

impl Preferences {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("preferences.json")
    }

    /// Load, treating any problem as "no preferences yet".
    ///
    /// A corrupt preferences file must never stop the program starting — the
    /// worst outcome of ignoring it is that the language picker appears again.
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice(&bytes) {
            Ok(prefs) => prefs,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable preferences");
                Self::default()
            }
        }
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AgentError::io(parent.display().to_string(), e))?;
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&path, bytes).map_err(|e| AgentError::io(path.display().to_string(), e))
    }
}

/// Resolve the interface language, most authoritative source first.
///
/// 1. `general.language` in config.toml — an explicit, version-controllable choice
/// 2. the UI preference saved by the language picker
/// 3. the system locale (`LC_ALL`, `LANG`, …)
/// 4. English
pub fn resolve_language(config: Option<Lang>, prefs: &Preferences) -> Lang {
    config
        .or(prefs.language)
        .or_else(Lang::from_environment)
        .unwrap_or(Lang::En)
}

/// Whether the first-run language picker should be shown.
pub fn should_prompt_for_language(config: Option<Lang>, prefs: &Preferences) -> bool {
    // Never interrupt someone who pinned the language in their config, and
    // never ask twice.
    config.is_none() && !prefs.language_chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_beats_the_saved_preference() {
        let prefs = Preferences {
            language: Some(Lang::Pl),
            language_chosen: true,
        };
        assert_eq!(resolve_language(Some(Lang::Ru), &prefs), Lang::Ru);
    }

    #[test]
    fn preference_is_used_when_config_is_silent() {
        let prefs = Preferences {
            language: Some(Lang::Lt),
            language_chosen: true,
        };
        assert_eq!(resolve_language(None, &prefs), Lang::Lt);
    }

    #[test]
    fn the_wizard_is_shown_once_and_never_over_an_explicit_config() {
        let fresh = Preferences::default();
        assert!(should_prompt_for_language(None, &fresh));

        let chosen = Preferences {
            language: Some(Lang::En),
            language_chosen: true,
        };
        assert!(!should_prompt_for_language(None, &chosen));

        // Even on a fresh install, an explicit config setting suppresses it.
        assert!(!should_prompt_for_language(Some(Lang::En), &fresh));
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("spotify-agent-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let prefs = Preferences {
            language: Some(Lang::Pl),
            language_chosen: true,
        };
        prefs.save(&dir).expect("save");
        let loaded = Preferences::load(&dir);
        assert_eq!(loaded.language, Some(Lang::Pl));
        assert!(loaded.language_chosen);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_file_falls_back_to_defaults() {
        let dir =
            std::env::temp_dir().join(format!("spotify-agent-prefs-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(Preferences::path(&dir), b"{ not json").expect("write");

        let loaded = Preferences::load(&dir);
        assert!(loaded.language.is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
