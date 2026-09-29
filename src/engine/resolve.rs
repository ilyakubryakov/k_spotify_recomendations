//! Resolving model suggestions to real Spotify tracks.
//!
//! This is where most of the real-world loss happens, so it gets two passes:
//!
//!   1. **Strict** — a field-qualified query (`track:"…" artist:"…"`). High
//!      precision; fails on punctuation drift, transliteration, and tracks
//!      credited to a collective rather than the named artist.
//!   2. **Loose** — a bare `artist title` query for everything the strict pass
//!      missed. Lower precision, so the same similarity floor still applies.
//!
//! Both passes run with bounded concurrency through the shared client, which
//! means they share the one rate-limit budget.
//!
//! A candidate is accepted only if its blended similarity clears
//! [`MIN_MATCH_SCORE`]. Accepting a weak match is worse than dropping the
//! suggestion: a wrong track in the playlist is visible, an absent one is not.

use crate::domain::{RejectReason, RejectedSuggestion, ResolvedSuggestion, Suggestion, Track};
use crate::spotify::SpotifyClient;
use crate::util::text::{normalize, normalize_title, similarity};

/// Blended title/artist similarity required to accept a match.
const MIN_MATCH_SCORE: f32 = 0.55;

/// Candidates to request per search. More than this rarely helps: Spotify
/// ranks well, and the extra results are usually covers and karaoke versions.
const SEARCH_LIMIT: usize = 8;

#[derive(Debug, Default)]
pub struct Resolution {
    pub resolved: Vec<ResolvedSuggestion>,
    pub rejected: Vec<RejectedSuggestion>,
}

/// Resolve every suggestion. `on_progress(done, total)` fires after each pass
/// batch so a UI can advance without this module knowing what a UI is.
pub async fn resolve_all<F>(
    client: &SpotifyClient,
    suggestions: Vec<Suggestion>,
    on_progress: F,
) -> Resolution
where
    F: Fn(usize, usize),
{
    let total = suggestions.len();
    if total == 0 {
        return Resolution::default();
    }

    let mut out = Resolution::default();
    // `pending` keeps (original suggestion, index) so the loose pass can report
    // failures against the right entry.
    let mut pending: Vec<Suggestion> = Vec::new();

    // ---- pass 1: strict ----
    let queries: Vec<String> = suggestions.iter().map(Suggestion::search_query).collect();
    let results = client.search_many(queries, SEARCH_LIMIT).await;

    for (suggestion, result) in suggestions.into_iter().zip(results) {
        match result {
            Ok(candidates) => match best_match(&suggestion, &candidates) {
                Some((track, score)) => out.resolved.push(ResolvedSuggestion {
                    suggestion,
                    track,
                    match_score: score,
                }),
                None => pending.push(suggestion),
            },
            Err(e) => {
                // A search failure is not evidence the track does not exist,
                // so it still goes to the loose pass.
                tracing::debug!(query = %suggestion.display(), error = %e, "strict search failed");
                pending.push(suggestion);
            }
        }
    }
    on_progress(out.resolved.len(), total);

    if pending.is_empty() {
        return out;
    }

    // ---- pass 2: loose ----
    tracing::debug!(
        count = pending.len(),
        "retrying unresolved suggestions with a loose query"
    );
    let queries: Vec<String> = pending.iter().map(Suggestion::loose_query).collect();
    let results = client.search_many(queries, SEARCH_LIMIT).await;

    for (suggestion, result) in pending.into_iter().zip(results) {
        match result {
            Ok(candidates) if candidates.is_empty() => out.rejected.push(RejectedSuggestion {
                suggestion,
                reason: RejectReason::NotFound,
            }),
            Ok(candidates) => match best_match(&suggestion, &candidates) {
                Some((track, score)) => out.resolved.push(ResolvedSuggestion {
                    suggestion,
                    track,
                    match_score: score,
                }),
                None => {
                    // Report how close the best candidate got: a run where
                    // everything lands at 45% means the model is drifting on
                    // spelling, which is actionable. A run of true 0% means
                    // it is inventing tracks, which is a different problem.
                    let best = candidates
                        .iter()
                        .map(|c| match_score(&suggestion, c))
                        .fold(0.0f32, f32::max);
                    out.rejected.push(RejectedSuggestion {
                        suggestion,
                        reason: RejectReason::WeakMatch {
                            score_pct: (best * 100.0) as u8,
                        },
                    });
                }
            },
            Err(e) => {
                tracing::warn!(query = %suggestion.display(), error = %e, "loose search failed");
                out.rejected.push(RejectedSuggestion {
                    suggestion,
                    reason: RejectReason::NotFound,
                });
            }
        }
    }
    on_progress(out.resolved.len(), total);

    out
}

fn best_match(suggestion: &Suggestion, candidates: &[Track]) -> Option<(Track, f32)> {
    candidates
        .iter()
        .map(|track| (track, match_score(suggestion, track)))
        .filter(|(_, score)| *score >= MIN_MATCH_SCORE)
        .max_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Tie-break on popularity: between two equally-good title
                // matches, the canonical release beats the karaoke cover.
                .then_with(|| a.0.popularity.cmp(&b.0.popularity))
        })
        .map(|(track, score)| (track.clone(), score))
}

/// Blended similarity in `[0, 1]`.
///
/// Weighted 60/40 toward the title: the artist field is where the model is
/// most likely to differ (collective vs. member, feat. credits), while a
/// wrong title means a wrong song.
pub fn match_score(suggestion: &Suggestion, track: &Track) -> f32 {
    let want_title = normalize_title(&suggestion.title);
    let got_title = normalize_title(&track.name);
    let title = if want_title == got_title {
        1.0
    } else {
        similarity(&want_title, &got_title)
    };

    let want_artist = normalize(&suggestion.artist);
    // Any credited artist may match, not just the primary one.
    let artist = track
        .artists
        .iter()
        .map(|a| {
            let got = normalize(&a.name);
            if got == want_artist {
                1.0
            } else {
                similarity(&want_artist, &got)
            }
        })
        .fold(0.0f32, f32::max);

    0.6 * title + 0.4 * artist
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ArtistRef;

    fn suggestion(title: &str, artist: &str) -> Suggestion {
        Suggestion {
            title: title.into(),
            artist: artist.into(),
            reason: String::new(),
            mood: String::new(),
            confidence: None,
            language: None,
        }
    }

    fn track(name: &str, artist: &str, popularity: u8) -> Track {
        Track {
            id: format!("{name}-{artist}"),
            name: name.into(),
            artists: vec![ArtistRef {
                id: "a".into(),
                name: artist.into(),
            }],
            album: "A".into(),
            duration_ms: 200_000,
            popularity,
            explicit: false,
            release_year: Some(2000),
            isrc: None,
        }
    }

    #[test]
    fn remaster_suffix_still_matches() {
        let s = suggestion("Kashmir", "Led Zeppelin");
        let t = track("Kashmir - 2012 Remaster", "Led Zeppelin", 70);
        assert!(match_score(&s, &t) > 0.95);
    }

    #[test]
    fn diacritics_do_not_break_matching() {
        let s = suggestion("Joga", "Bjork");
        let t = track("Jóga", "Björk", 60);
        assert!(match_score(&s, &t) > 0.95);
    }

    #[test]
    fn wrong_song_is_below_threshold() {
        let s = suggestion("Paranoid Android", "Radiohead");
        let t = track("Creep", "Radiohead", 80);
        assert!(match_score(&s, &t) < MIN_MATCH_SCORE);
    }

    #[test]
    fn popularity_breaks_ties_toward_the_canonical_release() {
        let s = suggestion("Song", "Artist");
        let candidates = vec![track("Song", "Artist", 10), track("Song", "Artist", 80)];
        let (best, _) = best_match(&s, &candidates).expect("a match");
        assert_eq!(best.popularity, 80);
    }

    #[test]
    fn featured_artist_credit_still_matches() {
        let s = suggestion("Get Lucky", "Daft Punk");
        let mut t = track("Get Lucky", "Pharrell Williams", 75);
        t.artists.push(ArtistRef {
            id: "b".into(),
            name: "Daft Punk".into(),
        });
        assert!(match_score(&s, &t) > MIN_MATCH_SCORE);
    }
}
