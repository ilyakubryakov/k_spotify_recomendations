//! Update-check bookkeeping.
//!
//! Lives in `<data_dir>/update-state.json`, next to `preferences.json` and for
//! the same reason: it is machine-owned, rewritten without asking, and must
//! never be merged into the hand-commented `config.toml`.
//!
//! It exists to answer two questions politely:
//!   * has enough time passed to bother GitHub again? (unauthenticated API
//!     calls are rate limited per IP, and a background check that burns that
//!     budget would break the check for everything else on the machine)
//!   * did the user already say "not this one"? A prompt that reappears every
//!     day after being dismissed is a prompt people learn to dismiss blindly.

use super::version::Version;
use crate::error::{AgentError, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateState {
    /// When the release feed was last consulted — successfully or not.
    pub last_check: Option<DateTime<Utc>>,
    /// The newest tag seen, for reporting without a network round trip.
    pub latest_seen: Option<String>,
    /// Versions the user explicitly declined. Stored as written tags.
    pub skipped: Vec<String>,
}

impl UpdateState {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("update-state.json")
    }

    /// Load, treating any problem as "never checked".
    ///
    /// A corrupt state file must not stop the program: the worst consequence
    /// of ignoring it is one extra HTTP request and one repeated prompt.
    pub fn load(data_dir: &Path) -> Self {
        let path = Self::path(data_dir);
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::debug!(path = %path.display(), error = %e, "ignoring unreadable update state");
            Self::default()
        })
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

    /// Whether a check is due. An unset or future `last_check` counts as due —
    /// a clock that jumped backwards should not disable updates forever.
    pub fn is_due(&self, interval_hours: u64, now: DateTime<Utc>) -> bool {
        let Some(last) = self.last_check else {
            return true;
        };
        if last > now {
            return true;
        }
        let interval = Duration::try_hours(interval_hours as i64).unwrap_or(Duration::zero());
        now - last >= interval
    }

    pub fn mark_checked(&mut self, now: DateTime<Utc>, latest: Option<&str>) {
        self.last_check = Some(now);
        if let Some(tag) = latest {
            self.latest_seen = Some(tag.to_string());
        }
    }

    /// Record "not this version". Newer versions are still offered — declining
    /// 0.2.0 should not silence 0.3.0.
    pub fn skip(&mut self, tag: &str) {
        if !self.skipped.iter().any(|s| s == tag) {
            self.skipped.push(tag.to_string());
        }
    }

    pub fn is_skipped(&self, version: &Version) -> bool {
        self.skipped
            .iter()
            .filter_map(|tag| Version::parse(tag))
            .any(|skipped| skipped == *version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours_ago: i64) -> DateTime<Utc> {
        Utc::now() - Duration::try_hours(hours_ago).unwrap_or(Duration::zero())
    }

    #[test]
    fn a_fresh_state_is_always_due() {
        assert!(UpdateState::default().is_due(24, Utc::now()));
    }

    #[test]
    fn the_interval_is_respected() {
        let state = UpdateState {
            last_check: Some(at(3)),
            ..Default::default()
        };
        assert!(!state.is_due(24, Utc::now()));
        assert!(state.is_due(1, Utc::now()));
    }

    #[test]
    fn a_clock_that_jumped_backwards_does_not_disable_checking() {
        let state = UpdateState {
            last_check: Some(at(-48)), // "checked" two days in the future
            ..Default::default()
        };
        assert!(state.is_due(24, Utc::now()));
    }

    #[test]
    fn skipping_silences_one_version_and_not_its_successors() {
        let mut state = UpdateState::default();
        state.skip("v0.2.0");
        let parse = |raw| Version::parse(raw).expect("parses");
        assert!(state.is_skipped(&parse("0.2.0")));
        // The tag was written with a `v`; the comparison must not care.
        assert!(state.is_skipped(&parse("v0.2.0")));
        assert!(!state.is_skipped(&parse("0.2.1")));
        assert!(!state.is_skipped(&parse("0.3.0")));
    }

    #[test]
    fn skipping_is_idempotent() {
        let mut state = UpdateState::default();
        state.skip("v0.2.0");
        state.skip("v0.2.0");
        assert_eq!(state.skipped.len(), 1);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("sa-update-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let mut state = UpdateState::default();
        state.mark_checked(Utc::now(), Some("v9.9.9"));
        state.skip("v9.9.9");
        state.save(&dir).expect("save");

        let loaded = UpdateState::load(&dir);
        assert_eq!(loaded.latest_seen.as_deref(), Some("v9.9.9"));
        assert_eq!(loaded.skipped, vec!["v9.9.9".to_string()]);
        assert!(loaded.last_check.is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_file_falls_back_to_never_checked() {
        let dir = std::env::temp_dir().join(format!("sa-update-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(UpdateState::path(&dir), b"not json at all").expect("write");

        let loaded = UpdateState::load(&dir);
        assert!(loaded.last_check.is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
