//! Post-resolution filtering.
//!
//! Claude is asked to respect the constraints, and mostly does — but "mostly"
//! is not a guarantee, and some constraints (Spotify popularity, explicit
//! flags, artist genre tags) are not knowable to the model at all. So every
//! rule is enforced here, against real Spotify metadata, after resolution.
//!
//! Order matters: the cheap, decisive checks run before the ones that need the
//! artist genre map, so a blocked artist never costs a lookup.

use crate::config::{Filters, LanguagePolicy};
use crate::domain::{Artist, RejectReason, Track};
use crate::util::text::{Script, detect_track_script, normalize};
use std::collections::HashMap;

pub struct FilterContext<'a> {
    pub filters: &'a Filters,
    pub language: LanguagePolicy,
    pub artists: &'a HashMap<String, Artist>,
}

impl FilterContext<'_> {
    /// `None` = keep.
    pub fn reject(&self, track: &Track) -> Option<RejectReason> {
        if let Some(reason) = self.check_artist_lists(track) {
            return Some(reason);
        }
        if let Some(reason) = self.check_language(track) {
            return Some(reason);
        }
        if let Some(reason) = self.check_scalars(track) {
            return Some(reason);
        }
        self.check_genres(track)
    }

    fn check_artist_lists(&self, track: &Track) -> Option<RejectReason> {
        let names: Vec<String> = track.artists.iter().map(|a| normalize(&a.name)).collect();

        if !self.filters.artists_block.is_empty() {
            let blocked: Vec<String> = self
                .filters
                .artists_block
                .iter()
                .map(|a| normalize(a))
                .collect();
            // Substring rather than equality: users write "Nickelback" and
            // expect "Nickelback & Friends" to be caught too.
            if names.iter().any(|n| {
                blocked
                    .iter()
                    .any(|b| !b.is_empty() && n.contains(b.as_str()))
            }) {
                return Some(RejectReason::Filtered("blocked artist"));
            }
        }

        if !self.filters.artists_allow.is_empty() {
            let allowed: Vec<String> = self
                .filters
                .artists_allow
                .iter()
                .map(|a| normalize(a))
                .collect();
            if !names.iter().any(|n| {
                allowed
                    .iter()
                    .any(|a| !a.is_empty() && n.contains(a.as_str()))
            }) {
                return Some(RejectReason::Filtered("not on the artist allow-list"));
            }
        }

        None
    }

    fn check_language(&self, track: &Track) -> Option<RejectReason> {
        let script = detect_track_script(&track.name, &track.artist_names_raw());
        let ok = match self.language {
            LanguagePolicy::Any => true,
            // Unknown (e.g. a purely numeric title) is accepted under every
            // policy: rejecting "1979" for having no detectable script would
            // be worse than the occasional miss.
            LanguagePolicy::English => matches!(script, Script::Latin | Script::Unknown),
            LanguagePolicy::Russian => matches!(script, Script::Cyrillic | Script::Unknown),
            LanguagePolicy::Mixed => {
                matches!(script, Script::Latin | Script::Cyrillic | Script::Unknown)
            }
        };
        (!ok).then_some(RejectReason::Language)
    }

    fn check_scalars(&self, track: &Track) -> Option<RejectReason> {
        if let Some(min) = self.filters.min_popularity {
            if track.popularity < min {
                return Some(RejectReason::Filtered("below minimum popularity"));
            }
        }
        if let Some(max) = self.filters.max_popularity {
            if track.popularity > max {
                return Some(RejectReason::Filtered("above maximum popularity"));
            }
        }
        if self.filters.allow_explicit == Some(false) && track.explicit {
            return Some(RejectReason::Filtered("explicit"));
        }
        let secs = track.duration_secs();
        if self.filters.min_duration_secs > 0 && secs < self.filters.min_duration_secs {
            return Some(RejectReason::Filtered("too short"));
        }
        if self.filters.max_duration_secs > 0 && secs > self.filters.max_duration_secs {
            return Some(RejectReason::Filtered("too long"));
        }
        None
    }

    fn check_genres(&self, track: &Track) -> Option<RejectReason> {
        if self.filters.genres_include.is_empty() && self.filters.genres_exclude.is_empty() {
            return None;
        }

        let genres: Vec<String> = track
            .artists
            .iter()
            .filter_map(|a| self.artists.get(&a.id))
            .flat_map(|a| a.genres.iter())
            .map(|g| g.to_lowercase())
            .collect();

        if !self.filters.genres_exclude.is_empty() {
            let excluded = self
                .filters
                .genres_exclude
                .iter()
                .map(|g| g.to_lowercase())
                .any(|needle| genres.iter().any(|g| g.contains(&needle)));
            if excluded {
                return Some(RejectReason::Filtered("excluded genre"));
            }
        }

        if !self.filters.genres_include.is_empty() {
            // An artist with no genre tags at all is common on Spotify
            // (smaller acts). Rejecting those would quietly filter out exactly
            // the obscure material a discovery preset is asking for, so an
            // untagged artist passes.
            if genres.is_empty() {
                return None;
            }
            let included = self
                .filters
                .genres_include
                .iter()
                .map(|g| g.to_lowercase())
                .any(|needle| genres.iter().any(|g| g.contains(&needle)));
            if !included {
                return Some(RejectReason::Filtered("outside the allowed genres"));
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ArtistRef;

    fn track(name: &str, artist: &str, popularity: u8, explicit: bool) -> Track {
        Track {
            id: "t".into(),
            name: name.into(),
            artists: vec![ArtistRef {
                id: "a1".into(),
                name: artist.into(),
            }],
            album: "A".into(),
            duration_ms: 200_000,
            popularity,
            explicit,
            release_year: Some(2020),
            isrc: None,
        }
    }

    #[test]
    fn language_policy_uses_script() {
        let artists = HashMap::new();
        let filters = Filters::default();
        let ctx = FilterContext {
            filters: &filters,
            language: LanguagePolicy::Russian,
            artists: &artists,
        };
        assert!(
            ctx.reject(&track("Хочу", "Молчат Дома", 40, false))
                .is_none()
        );
        assert_eq!(
            ctx.reject(&track("Wish", "Nine Inch Nails", 40, false)),
            Some(RejectReason::Language)
        );
    }

    #[test]
    fn numeric_titles_survive_every_policy() {
        let artists = HashMap::new();
        let filters = Filters::default();
        for policy in [
            LanguagePolicy::English,
            LanguagePolicy::Russian,
            LanguagePolicy::Mixed,
        ] {
            let ctx = FilterContext {
                filters: &filters,
                language: policy,
                artists: &artists,
            };
            // Artist is Latin, so this actually classifies as Latin; the case
            // that matters is a title with no letters and no artist letters.
            let mut t = track("1979", "1979", 40, false);
            t.artists.clear();
            assert!(
                ctx.reject(&t).is_none(),
                "{policy:?} rejected an unscripted title"
            );
        }
    }

    #[test]
    fn blocked_artist_matches_substring() {
        let artists = HashMap::new();
        let filters = Filters {
            artists_block: vec!["nickelback".into()],
            ..Default::default()
        };
        let ctx = FilterContext {
            filters: &filters,
            language: LanguagePolicy::Any,
            artists: &artists,
        };
        assert!(
            ctx.reject(&track("Any", "Nickelback & Friends", 50, false))
                .is_some()
        );
    }

    #[test]
    fn untagged_artist_passes_an_include_list() {
        let artists = HashMap::new(); // no genre data at all
        let filters = Filters {
            genres_include: vec!["shoegaze".into()],
            ..Default::default()
        };
        let ctx = FilterContext {
            filters: &filters,
            language: LanguagePolicy::Any,
            artists: &artists,
        };
        assert!(
            ctx.reject(&track("Song", "Obscure Act", 12, false))
                .is_none()
        );
    }

    #[test]
    fn tagged_artist_outside_include_list_is_rejected() {
        let mut artists = HashMap::new();
        artists.insert(
            "a1".to_string(),
            Artist {
                id: "a1".into(),
                name: "X".into(),
                genres: vec!["death metal".into()],
                popularity: 40,
            },
        );
        let filters = Filters {
            genres_include: vec!["shoegaze".into()],
            ..Default::default()
        };
        let ctx = FilterContext {
            filters: &filters,
            language: LanguagePolicy::Any,
            artists: &artists,
        };
        assert!(ctx.reject(&track("Song", "X", 40, false)).is_some());
    }

    #[test]
    fn popularity_and_explicit_bounds_apply() {
        let artists = HashMap::new();
        let filters = Filters {
            max_popularity: Some(60),
            allow_explicit: Some(false),
            ..Default::default()
        };
        let ctx = FilterContext {
            filters: &filters,
            language: LanguagePolicy::Any,
            artists: &artists,
        };
        assert!(ctx.reject(&track("Hit", "Pop Star", 90, false)).is_some());
        assert!(ctx.reject(&track("Song", "Act", 30, true)).is_some());
        assert!(ctx.reject(&track("Song", "Act", 30, false)).is_none());
    }
}
