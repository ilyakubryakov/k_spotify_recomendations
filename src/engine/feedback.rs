//! Deriving feedback signals from observable state.
//!
//! The agent never asks "did you like this?" — it watches what happened to the
//! tracks it chose. Four signals are derivable without any extra permission:
//!
//! | signal    | evidence                                                        |
//! |-----------|-----------------------------------------------------------------|
//! | `Liked`   | the track is now in Liked Songs, and was not when we picked it   |
//! | `Played`  | it appears in the play history and ran long enough               |
//! | `Skipped` | the *next* play started before this one could have finished      |
//! | `Removed` | it was in a playlist we wrote, and is not any more               |
//! | `Stale`   | it has sat in a playlist for weeks and has never been played     |
//!
//! ## About the skip inference
//!
//! Spotify's Web API exposes no skip event. It does expose `played_at` for the
//! last 50 plays, and the agent keeps every one of those forever, so over time
//! it accumulates a dense timeline. If track A starts at 12:00:00 and the next
//! play starts at 12:00:40 while A is 3:30 long, A was cut short.
//!
//! This is an inference, not a fact, and it is wrong in two known cases:
//! the listener paused mid-track and resumed hours later (looks like a skip),
//! and the play history has a gap because playback happened offline. Both make
//! it *over*-report skips, which is why the default weight for an inferred
//! skip is deliberately smaller in magnitude than for an explicit thumbs-down,
//! and why the signal decays.

use crate::config::FeedbackConfig;
use crate::storage::{FeedbackEntry, PlayWindow, Signal};
use std::collections::{HashMap, HashSet};

/// Everything the derivation needs, gathered by the caller.
pub struct Observations<'a> {
    /// `track_id -> first recommended at (unix ms)`.
    pub recommended: &'a HashMap<String, i64>,
    /// Current Liked Songs.
    pub saved: &'a HashSet<String>,
    /// Plays since the earliest recommendation.
    pub plays: &'a [PlayWindow],
    /// `playlist_id -> (tracks we recorded, tracks actually there now)`.
    pub playlists: &'a [PlaylistDiff],
    /// Tracks with at least one play, ever.
    pub ever_played: &'a HashSet<String>,
    pub now_ms: i64,
}

pub struct PlaylistDiff {
    pub playlist_id: String,
    /// What the agent recorded as members, with when they were added.
    pub recorded: Vec<(String, i64)>,
    /// What Spotify reports now.
    pub present: HashSet<String>,
}

/// Derive every signal. Pure: the caller persists the result.
pub fn derive(cfg: &FeedbackConfig, obs: &Observations<'_>) -> Vec<FeedbackEntry> {
    let mut out = Vec::new();
    if !cfg.enabled {
        return out;
    }

    // ---- liked ----
    for track_id in obs.saved {
        if obs.recommended.contains_key(track_id) {
            out.push(FeedbackEntry {
                track_id: track_id.clone(),
                signal: Signal::Liked,
                weight: cfg.weight_liked,
                source: "auto",
                note: Some("added to Liked Songs after being recommended".into()),
                // Liking is a one-time fact; the key carries no timestamp so it
                // is recorded exactly once however often sync runs.
                dedupe_key: format!("liked|{track_id}"),
            });
        }
    }

    // ---- played / skipped ----
    for window in obs.plays {
        let Some(recommended_at) = obs.recommended.get(&window.track_id) else {
            continue;
        };
        // Only judge plays that happened *after* we suggested the track;
        // earlier ones say nothing about our recommendation.
        if window.played_at_ms < *recommended_at {
            continue;
        }
        match classify_play(window, cfg.skip_ratio) {
            Some(PlayOutcome::Skipped { listened_ms }) => out.push(FeedbackEntry {
                track_id: window.track_id.clone(),
                signal: Signal::Skipped,
                weight: cfg.weight_skipped,
                source: "auto",
                note: Some(format!("inferred skip after {}s", listened_ms / 1000)),
                dedupe_key: format!("skipped|{}|{}", window.track_id, window.played_at_ms),
            }),
            Some(PlayOutcome::Played) => out.push(FeedbackEntry {
                track_id: window.track_id.clone(),
                signal: Signal::Played,
                weight: cfg.weight_played,
                source: "auto",
                note: None,
                dedupe_key: format!("played|{}|{}", window.track_id, window.played_at_ms),
            }),
            None => {}
        }
    }

    // ---- removed / stale ----
    let day_bucket = obs.now_ms / 86_400_000;
    for diff in obs.playlists {
        for (track_id, added_at) in &diff.recorded {
            if !diff.present.contains(track_id) {
                out.push(FeedbackEntry {
                    track_id: track_id.clone(),
                    signal: Signal::Removed,
                    weight: cfg.weight_removed,
                    source: "auto",
                    note: Some(format!("removed from playlist {}", diff.playlist_id)),
                    // Bucketed by day: the track stays absent forever, and
                    // without a bucket every later sync would re-punish it.
                    dedupe_key: format!("removed|{track_id}|{}", diff.playlist_id),
                });
                continue;
            }

            if cfg.stale_after_days == 0 || obs.ever_played.contains(track_id) {
                continue;
            }
            let age_days = (obs.now_ms - added_at) / 86_400_000;
            if age_days >= i64::from(cfg.stale_after_days) {
                out.push(FeedbackEntry {
                    track_id: track_id.clone(),
                    signal: Signal::Stale,
                    weight: cfg.weight_stale,
                    source: "auto",
                    note: Some(format!("in the playlist {age_days} days, never played")),
                    // Re-evaluated daily: a track that stays unplayed keeps
                    // accruing mild negative weight, which is the intent.
                    dedupe_key: format!("stale|{track_id}|{day_bucket}"),
                });
            }
        }
    }

    out
}

enum PlayOutcome {
    Played,
    Skipped { listened_ms: i64 },
}

/// Judge one play. `None` when there is not enough evidence either way — the
/// last play in the history has no successor, and a track with no known
/// duration cannot be judged at all.
fn classify_play(window: &PlayWindow, skip_ratio: f32) -> Option<PlayOutcome> {
    let next = window.next_played_at_ms?;
    if window.duration_ms == 0 {
        return None;
    }
    let listened_ms = next - window.played_at_ms;
    if listened_ms <= 0 {
        return None;
    }
    let threshold = (f64::from(window.duration_ms) * f64::from(skip_ratio)) as i64;

    // A gap far longer than the track means the session simply ended here;
    // that is not evidence of enjoyment or rejection, so it stays unjudged
    // rather than being counted as a full play.
    if listened_ms > i64::from(window.duration_ms) * 3 {
        return None;
    }
    if listened_ms < threshold {
        Some(PlayOutcome::Skipped { listened_ms })
    } else {
        Some(PlayOutcome::Played)
    }
}

/// Artists the listener has demonstrably taken to, and ones to steer away from.
///
/// Returned as display names for the prompt; the hard filtering downstream
/// works off ids and normalised names instead.
pub fn artist_verdicts(
    cfg: &FeedbackConfig,
    scores: &HashMap<String, f32>,
    id_to_name: &HashMap<String, String>,
    limit: usize,
) -> (Vec<String>, Vec<String>) {
    let mut liked: Vec<(String, f32)> = Vec::new();
    let mut disliked: Vec<(String, f32)> = Vec::new();

    for (key, score) in scores {
        // Prefer a real display name; skip keys we cannot name, since an
        // opaque id in the prompt is noise the model cannot act on.
        let Some(name) = id_to_name.get(key) else {
            continue;
        };
        if *score >= cfg.boost_threshold {
            liked.push((name.clone(), *score));
        } else if *score <= cfg.avoid_threshold {
            disliked.push((name.clone(), *score));
        }
    }

    liked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    disliked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    liked.truncate(limit);
    disliked.truncate(limit);

    (
        liked.into_iter().map(|(n, _)| n).collect(),
        disliked.into_iter().map(|(n, _)| n).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FeedbackConfig {
        FeedbackConfig::default()
    }

    fn window(track: &str, at: i64, duration_ms: u32, next: Option<i64>) -> PlayWindow {
        PlayWindow {
            track_id: track.into(),
            played_at_ms: at,
            duration_ms,
            next_played_at_ms: next,
        }
    }

    fn observations<'a>(
        recommended: &'a HashMap<String, i64>,
        saved: &'a HashSet<String>,
        plays: &'a [PlayWindow],
        playlists: &'a [PlaylistDiff],
        ever_played: &'a HashSet<String>,
    ) -> Observations<'a> {
        Observations {
            recommended,
            saved,
            plays,
            playlists,
            ever_played,
            now_ms: 10_000_000_000,
        }
    }

    #[test]
    fn a_short_gap_before_the_next_play_is_a_skip() {
        // 200s track, next play 30s later -> skipped.
        let w = window("t", 1_000, 200_000, Some(31_000));
        assert!(matches!(
            classify_play(&w, 0.6),
            Some(PlayOutcome::Skipped { .. })
        ));
    }

    #[test]
    fn a_full_listen_is_a_play() {
        // 200s track, next play 190s later -> played through.
        let w = window("t", 1_000, 200_000, Some(191_000));
        assert!(matches!(classify_play(&w, 0.6), Some(PlayOutcome::Played)));
    }

    #[test]
    fn an_enormous_gap_is_not_judged_at_all() {
        // The session ended here; counting it as a full play would reward
        // tracks that merely happened to be last.
        let w = window("t", 1_000, 200_000, Some(1_000 + 200_000 * 10));
        assert!(classify_play(&w, 0.6).is_none());
    }

    #[test]
    fn the_last_play_in_history_is_not_judged() {
        assert!(classify_play(&window("t", 1_000, 200_000, None), 0.6).is_none());
    }

    #[test]
    fn unknown_duration_is_not_judged() {
        assert!(classify_play(&window("t", 1_000, 0, Some(2_000)), 0.6).is_none());
    }

    #[test]
    fn plays_before_the_recommendation_are_ignored() {
        let mut recommended = HashMap::new();
        recommended.insert("t".to_string(), 5_000i64);
        let saved = HashSet::new();
        let plays = vec![window("t", 1_000, 200_000, Some(2_000))]; // before
        let playlists: Vec<PlaylistDiff> = Vec::new();
        let ever = HashSet::new();

        let out = derive(
            &cfg(),
            &observations(&recommended, &saved, &plays, &playlists, &ever),
        );
        assert!(
            out.is_empty(),
            "a play from before we suggested it proves nothing"
        );
    }

    #[test]
    fn liking_a_recommended_track_is_recorded_once() {
        let mut recommended = HashMap::new();
        recommended.insert("t".to_string(), 1i64);
        let mut saved = HashSet::new();
        saved.insert("t".to_string());
        let playlists: Vec<PlaylistDiff> = Vec::new();
        let ever = HashSet::new();

        let out = derive(
            &cfg(),
            &observations(&recommended, &saved, &[], &playlists, &ever),
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].signal, Signal::Liked);
        assert!(out[0].weight > 0.0);
        // No timestamp in the key -> idempotent across syncs.
        assert_eq!(out[0].dedupe_key, "liked|t");
    }

    #[test]
    fn a_track_that_vanished_from_the_playlist_is_negative() {
        let recommended = HashMap::new();
        let saved = HashSet::new();
        let ever = HashSet::new();
        let playlists = vec![PlaylistDiff {
            playlist_id: "pl".into(),
            recorded: vec![("gone".into(), 0), ("kept".into(), 0)],
            present: ["kept".to_string()].into_iter().collect(),
        }];

        let out = derive(
            &cfg(),
            &observations(&recommended, &saved, &[], &playlists, &ever),
        );
        let removed: Vec<&FeedbackEntry> =
            out.iter().filter(|e| e.signal == Signal::Removed).collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].track_id, "gone");
        assert!(removed[0].weight < 0.0);
    }

    #[test]
    fn an_old_never_played_track_goes_stale_but_a_played_one_does_not() {
        let recommended = HashMap::new();
        let saved = HashSet::new();
        let ever: HashSet<String> = ["played".to_string()].into_iter().collect();
        let old = 10_000_000_000i64 - 60 * 86_400_000;
        let playlists = vec![PlaylistDiff {
            playlist_id: "pl".into(),
            recorded: vec![("ignored".into(), old), ("played".into(), old)],
            present: ["ignored".to_string(), "played".to_string()]
                .into_iter()
                .collect(),
        }];

        let out = derive(
            &cfg(),
            &observations(&recommended, &saved, &[], &playlists, &ever),
        );
        let stale: Vec<&FeedbackEntry> = out.iter().filter(|e| e.signal == Signal::Stale).collect();
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].track_id, "ignored");
    }

    #[test]
    fn disabled_feedback_derives_nothing() {
        let mut c = cfg();
        c.enabled = false;
        let mut recommended = HashMap::new();
        recommended.insert("t".to_string(), 1i64);
        let saved: HashSet<String> = ["t".to_string()].into_iter().collect();
        let playlists: Vec<PlaylistDiff> = Vec::new();
        let ever = HashSet::new();
        assert!(
            derive(
                &c,
                &observations(&recommended, &saved, &[], &playlists, &ever)
            )
            .is_empty()
        );
    }

    #[test]
    fn verdicts_split_on_the_configured_thresholds() {
        let c = cfg();
        let mut scores = HashMap::new();
        scores.insert("a1".to_string(), 9.0);
        scores.insert("a2".to_string(), -9.0);
        scores.insert("a3".to_string(), 0.1);
        scores.insert("unnamed".to_string(), 9.0);

        let mut names = HashMap::new();
        names.insert("a1".to_string(), "Loved".to_string());
        names.insert("a2".to_string(), "Hated".to_string());
        names.insert("a3".to_string(), "Meh".to_string());

        let (liked, disliked) = artist_verdicts(&c, &scores, &names, 10);
        assert_eq!(liked, vec!["Loved".to_string()]);
        assert_eq!(disliked, vec!["Hated".to_string()]);
    }
}
