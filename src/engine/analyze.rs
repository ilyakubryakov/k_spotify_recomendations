//! Taste-profile construction.
//!
//! Pure computation over what `storage` returns — no I/O — so it is cheap to
//! test and can be re-run offline.
//!
//! The scoring model, in one place so it can be argued with:
//!
//! ```text
//! track_score  = 2.0 · top_score        (position-weighted /me/top/tracks)
//!              + 1.0 · saved            (in Liked Songs)
//!              + 0.4 · plays            (all-time local history)
//!              + 1.0 · recent_plays     (last 30 days — recency matters most)
//!
//! artist_score = Σ track_score over their tracks
//!              + 3.0 · top_artist_score (position-weighted /me/top/artists)
//! ```
//!
//! The constants are deliberately blunt. Their job is to produce a *ranking*
//! the model reads as evidence, not a calibrated metric; over-tuning them
//! buys nothing downstream.

use crate::domain::{
    Artist, ArtistAffinity, EraProfile, GenreWeight, ScriptMix, TasteProfile, TrackAffinity,
};
use crate::storage::TrackStat;
use crate::util::text::Script;
use chrono::Utc;
use std::collections::HashMap;

/// Plays in the last 30 days at or above this count marks a track as "looped".
const LOOP_THRESHOLD: u32 = 3;

pub fn build_profile(
    stats: &[TrackStat],
    artists: &HashMap<String, Artist>,
    top_artist_scores: &HashMap<String, f32>,
) -> TasteProfile {
    let mut top_tracks: Vec<TrackAffinity> = Vec::with_capacity(stats.len());
    let mut artist_scores: HashMap<String, f32> = HashMap::new();
    let mut artist_tracks: HashMap<String, u32> = HashMap::new();
    let mut artist_plays: HashMap<String, u32> = HashMap::new();

    let mut years: Vec<i32> = Vec::new();
    let mut popularity: Vec<u8> = Vec::new();
    let mut durations: Vec<u32> = Vec::new();
    let mut script_counts: HashMap<Script, usize> = HashMap::new();

    let mut saved_tracks = 0usize;
    let mut play_events = 0usize;
    let mut distinct_played = 0usize;

    for stat in stats {
        if stat.saved {
            saved_tracks += 1;
        }
        play_events += stat.plays as usize;
        if stat.plays > 0 {
            distinct_played += 1;
        }

        let score = track_score(stat);

        // Library statistics are drawn from tracks the listener actually has a
        // relationship with. Tracks that merely appeared in a playlist we read
        // would otherwise skew the era and popularity medians.
        if score > 0.0 {
            if let Some(year) = stat.track.release_year {
                years.push(year);
            }
            popularity.push(stat.track.popularity);
            durations.push(stat.track.duration_secs());
            *script_counts.entry(stat.script).or_insert(0) += 1;

            for artist in &stat.track.artists {
                if artist.id.is_empty() {
                    continue;
                }
                *artist_scores.entry(artist.id.clone()).or_insert(0.0) += score;
                *artist_tracks.entry(artist.id.clone()).or_insert(0) += 1;
                *artist_plays.entry(artist.id.clone()).or_insert(0) += stat.plays;
            }

            top_tracks.push(TrackAffinity {
                id: stat.track.id.clone(),
                name: stat.track.name.clone(),
                artist: stat.track.artist_line(),
                score,
                plays: stat.plays,
                recent_plays: stat.recent_plays,
                release_year: stat.track.release_year,
            });
        }
    }

    for (artist_id, top_score) in top_artist_scores {
        *artist_scores.entry(artist_id.clone()).or_insert(0.0) += top_score * 3.0;
    }

    // ---- artists ----
    let mut top_artists: Vec<ArtistAffinity> = artist_scores
        .iter()
        .map(|(id, score)| {
            let known = artists.get(id);
            ArtistAffinity {
                id: id.clone(),
                name: known.map(|a| a.name.clone()).unwrap_or_else(|| id.clone()),
                score: *score,
                track_count: artist_tracks.get(id).copied().unwrap_or(0),
                plays: artist_plays.get(id).copied().unwrap_or(0),
                genres: known.map(|a| a.genres.clone()).unwrap_or_default(),
            }
        })
        // An artist whose name we never hydrated would appear in the prompt as
        // a raw Spotify id, which is noise the model cannot use.
        .filter(|a| a.name != a.id)
        .collect();
    sort_desc(&mut top_artists, |a| a.score);

    // ---- genres ----
    let mut genre_weights: HashMap<String, (f32, u32)> = HashMap::new();
    for artist in &top_artists {
        for genre in &artist.genres {
            let entry = genre_weights.entry(genre.clone()).or_insert((0.0, 0));
            entry.0 += artist.score;
            entry.1 += 1;
        }
    }
    let mut genres: Vec<GenreWeight> = genre_weights
        .into_iter()
        .map(|(genre, (weight, artist_count))| GenreWeight {
            genre,
            weight,
            artist_count,
        })
        .collect();
    sort_desc(&mut genres, |g| g.weight);

    // ---- tracks ----
    sort_desc(&mut top_tracks, |t| t.score);

    let mut looped: Vec<TrackAffinity> = top_tracks
        .iter()
        .filter(|t| t.recent_plays >= LOOP_THRESHOLD)
        .cloned()
        .collect();
    looped.sort_by_key(|t| std::cmp::Reverse(t.recent_plays));
    looped.truncate(25);

    TasteProfile {
        generated_at: Utc::now(),
        saved_tracks,
        known_tracks: stats.len(),
        known_artists: artists.len(),
        play_events,
        distinct_played,
        top_artists,
        top_tracks,
        looped,
        genres,
        era: era_profile(&mut years),
        script_mix: script_mix(&script_counts),
        median_popularity: median(&mut popularity).unwrap_or(50),
        median_duration_secs: median(&mut durations).unwrap_or(210),
    }
}

fn track_score(stat: &TrackStat) -> f32 {
    2.0 * stat.top_score
        + if stat.saved { 1.0 } else { 0.0 }
        + 0.4 * stat.plays as f32
        + 1.0 * stat.recent_plays as f32
}

/// Sort descending by a key, with NaN-safe comparison.
fn sort_desc<T, F: Fn(&T) -> f32>(items: &mut [T], key: F) {
    items.sort_by(|a, b| {
        key(b)
            .partial_cmp(&key(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

fn era_profile(years: &mut [i32]) -> EraProfile {
    if years.is_empty() {
        return EraProfile::default();
    }
    years.sort_unstable();

    let mut decades: HashMap<i32, usize> = HashMap::new();
    for year in years.iter() {
        *decades.entry(year - year.rem_euclid(10)).or_insert(0) += 1;
    }
    let total = years.len() as f32;
    let mut decades: Vec<(i32, f32)> = decades
        .into_iter()
        .map(|(decade, count)| (decade, count as f32 / total))
        .collect();
    decades.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    EraProfile {
        median_year: percentile(years, 0.5),
        p10_year: percentile(years, 0.10),
        p90_year: percentile(years, 0.90),
        decades,
    }
}

fn script_mix(counts: &HashMap<Script, usize>) -> ScriptMix {
    let total: usize = counts.values().sum();
    if total == 0 {
        return ScriptMix::default();
    }
    let share = |s: Script| counts.get(&s).copied().unwrap_or(0) as f32 / total as f32;
    ScriptMix {
        latin: share(Script::Latin),
        cyrillic: share(Script::Cyrillic),
        cjk: share(Script::Cjk),
        other: share(Script::Other) + share(Script::Unknown),
    }
}

/// `values` must already be sorted.
fn percentile(values: &[i32], q: f32) -> Option<i32> {
    if values.is_empty() {
        return None;
    }
    let idx = ((values.len() as f32 - 1.0) * q).round() as usize;
    values.get(idx.min(values.len() - 1)).copied()
}

fn median<T: Ord + Copy>(values: &mut [T]) -> Option<T> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    values.get(values.len() / 2).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ArtistRef, Track};

    fn stat(
        id: &str,
        artist_id: &str,
        saved: bool,
        plays: u32,
        recent: u32,
        top: f32,
    ) -> TrackStat {
        TrackStat {
            track: Track {
                id: id.into(),
                name: format!("Track {id}"),
                artists: vec![ArtistRef {
                    id: artist_id.into(),
                    name: format!("Artist {artist_id}"),
                }],
                album: "A".into(),
                duration_ms: 200_000,
                popularity: 60,
                explicit: false,
                release_year: Some(2010),
                isrc: None,
            },
            saved,
            plays,
            recent_plays: recent,
            top_score: top,
            script: Script::Latin,
        }
    }

    #[test]
    fn recency_outranks_raw_play_count() {
        let stats = vec![
            stat("old", "a1", true, 20, 0, 0.0), // 1.0 + 8.0 = 9.0
            stat("now", "a2", true, 4, 9, 0.0),  // 1.0 + 1.6 + 9.0 = 11.6
        ];
        let profile = build_profile(&stats, &HashMap::new(), &HashMap::new());
        assert_eq!(
            profile.top_tracks.first().map(|t| t.id.as_str()),
            Some("now")
        );
        assert_eq!(profile.looped.len(), 1);
    }

    #[test]
    fn unhydrated_artists_are_not_leaked_as_ids() {
        let stats = vec![stat("t", "unknown-id", true, 0, 0, 0.0)];
        let profile = build_profile(&stats, &HashMap::new(), &HashMap::new());
        assert!(profile.top_artists.is_empty());
    }

    #[test]
    fn genres_aggregate_across_artists() {
        let mut artists = HashMap::new();
        artists.insert(
            "a1".to_string(),
            Artist {
                id: "a1".into(),
                name: "One".into(),
                genres: vec!["shoegaze".into()],
                popularity: 40,
            },
        );
        artists.insert(
            "a2".to_string(),
            Artist {
                id: "a2".into(),
                name: "Two".into(),
                genres: vec!["shoegaze".into(), "dream pop".into()],
                popularity: 30,
            },
        );
        let stats = vec![
            stat("t1", "a1", true, 0, 0, 0.0),
            stat("t2", "a2", true, 0, 0, 0.0),
        ];
        let profile = build_profile(&stats, &artists, &HashMap::new());
        let top = profile.genres.first().expect("a genre");
        assert_eq!(top.genre, "shoegaze");
        assert_eq!(top.artist_count, 2);
    }

    #[test]
    fn era_percentiles_are_ordered() {
        let stats: Vec<TrackStat> = (1990..2020)
            .map(|year| {
                let mut s = stat(&year.to_string(), "a1", true, 0, 0, 0.0);
                s.track.release_year = Some(year);
                s
            })
            .collect();
        let profile = build_profile(&stats, &HashMap::new(), &HashMap::new());
        let era = &profile.era;
        assert!(era.p10_year <= era.median_year);
        assert!(era.median_year <= era.p90_year);
    }
}
