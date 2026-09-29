//! Prompt construction.
//!
//! Split into three parts so prompt caching actually works:
//!
//! | part              | changes        | cached |
//! |-------------------|----------------|--------|
//! | `system_stable`   | never          | yes    |
//! | `system_volatile` | per preset     | no     |
//! | `user`            | per run        | no     |
//!
//! The cache breakpoint sits after `system_stable`, so the persona and the
//! output contract are billed once per five-minute window rather than once per
//! run.
//!
//! Content decisions worth knowing:
//!   * The profile is rendered as compact, labelled lines rather than raw
//!     JSON — the same information costs roughly half the tokens and reads
//!     better to the model.
//!   * Exclusions are sent as `Artist — Title` lines. This is the single
//!     largest block, so it is capped and ordered newest-first: the recent
//!     recommendations are the ones the model would otherwise repeat.

use crate::config::{LanguagePolicy, ResolvedPreset};
use crate::domain::TasteProfile;
use std::fmt::Write as _;

/// Invariant persona + rules + output contract. Cacheable.
pub const SYSTEM_STABLE: &str = r#"You are the curation engine inside a personal music agent. You build playlists for one specific listener whose full listening profile is given to you, and your output is consumed programmatically.

How to choose tracks:
- Read the profile as evidence, not as a shopping list. The listener's top artists tell you their sensibility; they are usually the wrong thing to recommend back. Recommend what a person with that sensibility has not yet found.
- Every pick must be defensible from something concrete in the profile — a genre cluster, an era, a recurring production style, an artist they loop. Vague appeals to a genre are not a reason.
- Respect the brief's *situation*, not just its adjectives. A playlist for driving is sequenced differently from one for concentration, even with identical genres.
- Vary the set. Repeated picks from the same label, scene, or year make a playlist feel thin even when each track is individually good.
- Sequence matters: return the tracks in the order they should be listened to, with a deliberate opening and a coherent arc.

Hard rules:
- Only real, commercially released recordings. Never invent a track, an artist, or a collaboration. If you are not confident a recording exists, leave it out — a shorter accurate list beats a padded one.
- Spell titles and artists exactly as they appear on streaming services, in their original script. Do not transliterate, translate, or append "(feat. …)", "(Remastered)", "(Radio Edit)" or any other edition annotation.
- Give the primary credited artist only, even for collaborations.
- Never repeat a track or an artist already listed under EXCLUSIONS.
- Never return the same track twice.
- Set `confidence` honestly. A low value is useful information, not a failure; it tells the resolver to try harder on that entry.

Output contract:
- Respond with the JSON document required by the schema and nothing else.
- `reason` is one sentence, written to the listener, referencing something specific about their taste. Not marketing copy.
- Produce exactly the number of tracks requested. If you genuinely cannot reach it without inventing recordings or violating the brief, return fewer and say so in `summary`."#;

/// Per-preset framing. Not cached, but small.
pub fn system_volatile(preset: &ResolvedPreset) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "ACTIVE PRESET: {} ({})", preset.name, preset.label);
    if !preset.moods.is_empty() {
        let _ = writeln!(
            s,
            "Mood vocabulary to work within: {}",
            preset.moods.join(", ")
        );
    }
    let _ = writeln!(s, "\nBRIEF\n{}", preset.brief.trim());
    s
}

pub struct PromptInput<'a> {
    pub preset: &'a ResolvedPreset,
    pub profile: &'a TasteProfile,
    /// Artists the listener demonstrably took to (from the feedback loop).
    pub resonated: &'a [String],
    /// Artists whose picks were skipped, removed or disliked.
    pub rejected: &'a [String],
    /// How many tracks to ask for (already oversampled).
    pub request_count: usize,
    /// `Artist — Title` lines the model must not return.
    pub exclusions: &'a [String],
    /// Artist names the model must not return at all.
    pub blocked_artists: &'a [String],
    /// Optional free-text steer typed by the user for this run only.
    pub extra_instructions: Option<&'a str>,
    pub market: Option<&'a str>,
}

/// The per-run user message.
pub fn user_message(input: &PromptInput<'_>) -> String {
    let mut s = String::with_capacity(8 * 1024);
    let p = input.profile;
    let run = &input.preset.run;

    // ---- listener profile -------------------------------------------
    s.push_str("=== LISTENER PROFILE ===\n");
    let _ = writeln!(
        s,
        "Library: {} saved tracks, {} distinct tracks seen, {} artists, {} logged plays.",
        p.saved_tracks, p.known_tracks, p.known_artists, p.play_events
    );
    let _ = writeln!(s, "Era: {}.", p.era.describe());
    if !p.era.decades.is_empty() {
        let decades = p
            .era
            .decades
            .iter()
            .take(4)
            .map(|(d, share)| format!("{d}s {:.0}%", share * 100.0))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(s, "Decade distribution: {decades}.");
    }
    let _ = writeln!(
        s,
        "Language mix of their library: {}.",
        p.script_mix.describe()
    );
    let _ = writeln!(
        s,
        "Typical track: {} popularity out of 100, {}:{:02} long. Popularity is a strong signal — \
this listener sits {} the mainstream, so calibrate obscurity accordingly.",
        p.median_popularity,
        p.median_duration_secs / 60,
        p.median_duration_secs % 60,
        match p.median_popularity {
            0..=34 => "well outside",
            35..=54 => "at the edge of",
            55..=69 => "inside",
            _ => "squarely in",
        }
    );

    if !p.genres.is_empty() {
        s.push_str("\n-- Genre vocabulary (weighted) --\n");
        for g in p.genres.iter().take(run.profile_top_genres) {
            let _ = writeln!(s, "{} ({} artists)", g.genre, g.artist_count);
        }
    }

    if !p.top_artists.is_empty() {
        s.push_str("\n-- Core artists (strongest affinity first) --\n");
        for a in p.top_artists.iter().take(run.profile_top_artists) {
            let genres = if a.genres.is_empty() {
                String::new()
            } else {
                format!(
                    " [{}]",
                    a.genres
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let _ = writeln!(
                s,
                "{}{} — {} tracks, {} plays",
                a.name, genres, a.track_count, a.plays
            );
        }
    }

    if !p.top_tracks.is_empty() {
        s.push_str("\n-- Signature tracks --\n");
        for t in p.top_tracks.iter().take(run.profile_top_tracks) {
            let year = t
                .release_year
                .map(|y| format!(" ({y})"))
                .unwrap_or_default();
            let _ = writeln!(s, "{} — {}{}", t.artist, t.name, year);
        }
    }

    if !p.looped.is_empty() {
        s.push_str("\n-- On heavy rotation right now (last 30 days) --\n");
        for t in p.looped.iter().take(15) {
            let _ = writeln!(s, "{} — {} ({} plays)", t.artist, t.name, t.recent_plays);
        }
        s.push_str("These are the listener's current obsessions. They are the sharpest signal in this profile.\n");
    }

    // ---- request ----------------------------------------------------
    s.push_str("\n=== REQUEST ===\n");
    let _ = writeln!(s, "Return exactly {} tracks.", input.request_count);
    let _ = writeln!(s, "{}", discovery_clause(run.discovery_level));
    let _ = writeln!(s, "{}", language_clause(run.language));
    if run.max_per_artist > 0 {
        let _ = writeln!(
            s,
            "At most {} track(s) per artist across the whole list.",
            run.max_per_artist
        );
    }
    if let Some(market) = input.market {
        let _ = writeln!(
            s,
            "The listener streams in market {market}; prefer recordings available there."
        );
    }

    let filters = &input.preset.filters;
    if !filters.genres_include.is_empty() {
        let _ = writeln!(
            s,
            "Stay within these genres: {}.",
            filters.genres_include.join(", ")
        );
    }
    if !filters.genres_exclude.is_empty() {
        let _ = writeln!(
            s,
            "Avoid these genres entirely: {}.",
            filters.genres_exclude.join(", ")
        );
    }
    if !filters.artists_allow.is_empty() {
        let _ = writeln!(
            s,
            "Only these artists are permitted: {}.",
            filters.artists_allow.join(", ")
        );
    }
    match (filters.min_popularity, filters.max_popularity) {
        (Some(lo), Some(hi)) => {
            let _ = writeln!(
                s,
                "Aim for tracks in the {lo}–{hi} Spotify popularity band."
            );
        }
        (Some(lo), None) => {
            let _ = writeln!(
                s,
                "Avoid anything more obscure than roughly {lo}/100 popularity."
            );
        }
        (None, Some(hi)) => {
            let _ = writeln!(
                s,
                "Avoid anything more mainstream than roughly {hi}/100 popularity — no chart hits."
            );
        }
        (None, None) => {}
    }
    if filters.allow_explicit == Some(false) {
        s.push_str("No explicit-tagged recordings.\n");
    }

    if let Some(extra) = input.extra_instructions.filter(|e| !e.trim().is_empty()) {
        let _ = writeln!(
            s,
            "\nAdditional instruction for this run only: {}",
            extra.trim()
        );
    }

    // ---- feedback ----------------------------------------------------
    // The single highest-value block after the profile: it is the only part of
    // the prompt derived from what actually happened to previous picks.
    if !input.resonated.is_empty() || !input.rejected.is_empty() {
        s.push_str("\n=== HOW YOUR PREVIOUS PICKS LANDED ===\n");
        if !input.resonated.is_empty() {
            let _ = writeln!(
                s,
                "Worked — the listener saved, replayed or kept these: {}.\n\
Read what these have in common and aim at that, but do not simply return more of the same artists.",
                input.resonated.join(", ")
            );
        }
        if !input.rejected.is_empty() {
            let _ = writeln!(
                s,
                "Did not work — skipped, deleted or thumbed down: {}.\n\
Avoid these artists and, more importantly, avoid whatever quality they share.",
                input.rejected.join(", ")
            );
        }
    }

    // ---- exclusions --------------------------------------------------
    if !input.blocked_artists.is_empty() {
        s.push_str("\n=== BLOCKED ARTISTS (never return these) ===\n");
        s.push_str(&input.blocked_artists.join("\n"));
        s.push('\n');
    }

    if input.exclusions.is_empty() {
        s.push_str("\n=== EXCLUSIONS ===\n(none recorded yet)\n");
    } else {
        let _ = writeln!(
            s,
            "\n=== EXCLUSIONS ({} entries — already known to the listener or recommended recently; do not return any of them) ===",
            input.exclusions.len()
        );
        s.push_str(&input.exclusions.join("\n"));
        s.push('\n');
    }

    s.push_str("\nNow produce the JSON document.");
    s
}

/// Translate the 0–10 knob into an instruction the model can act on.
///
/// Stated as *audience size* rather than as a genre or a popularity number:
/// asking for "obscure" makes models reach for the canonical obscure bands,
/// while describing who else listens to it gets a far better spread.
fn discovery_clause(level: u8) -> &'static str {
    match level {
        0..=1 => {
            "Discovery: minimal. Stay with well-known, widely-played material the listener has a good chance of recognising — established records by artists with large audiences."
        }
        2..=3 => {
            "Discovery: low. Prefer recognised records and familiar names; an occasional lesser-known track is fine, but this should feel comfortable rather than challenging."
        }
        4..=6 => {
            "Discovery: moderate. Balance records with a real audience against genuinely lesser-known work — roughly half the set should be new to someone with this profile."
        }
        7..=8 => {
            "Discovery: high. Reach past the well-known material. Favour artists a dedicated listener of this scene would know but a casual one would not: smaller labels, deeper album cuts, regional scenes."
        }
        _ => {
            "Discovery: maximum. Go underground. Small-label releases, self-published work, short-lived projects, scenes that never crossed over. Assume the listener already knows every obvious name — returning one is a failure. Accuracy still matters: every track must be a real release you are confident exists."
        }
    }
}

fn language_clause(policy: LanguagePolicy) -> &'static str {
    match policy {
        LanguagePolicy::English => {
            "Language: English-language lyrics only. Instrumental tracks are acceptable. Do not include tracks sung in any other language."
        }
        LanguagePolicy::Russian => {
            "Language: Russian-language lyrics only (русскоязычные исполнители). Instrumental tracks by artists from the Russian-speaking scene are acceptable. Do not include English-language tracks."
        }
        LanguagePolicy::Mixed => {
            "Language: deliberately mix English- and Russian-language tracks, roughly half and half, and interleave them rather than grouping by language."
        }
        LanguagePolicy::Any => "Language: no constraint.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use chrono::Utc;

    fn empty_profile() -> TasteProfile {
        TasteProfile {
            generated_at: Utc::now(),
            saved_tracks: 0,
            known_tracks: 0,
            known_artists: 0,
            play_events: 0,
            distinct_played: 0,
            top_artists: vec![],
            top_tracks: vec![],
            looped: vec![],
            genres: vec![],
            era: Default::default(),
            script_mix: Default::default(),
            median_popularity: 50,
            median_duration_secs: 210,
        }
    }

    #[test]
    fn exclusions_and_count_reach_the_prompt() {
        let cfg = Config {
            presets: crate::config::presets::builtin(),
            ..Default::default()
        };
        let preset = cfg.resolve_preset("discover").expect("preset");
        let profile = empty_profile();
        let exclusions = vec!["A — B".to_string()];
        let msg = user_message(&PromptInput {
            preset: &preset,
            profile: &profile,
            resonated: &[],
            rejected: &[],
            request_count: 42,
            exclusions: &exclusions,
            blocked_artists: &[],
            extra_instructions: None,
            market: Some("DE"),
        });
        assert!(msg.contains("Return exactly 42 tracks."));
        assert!(msg.contains("A — B"));
        assert!(msg.contains("market DE"));
    }

    #[test]
    fn feedback_verdicts_reach_the_prompt() {
        let cfg = Config {
            presets: crate::config::presets::builtin(),
            ..Default::default()
        };
        let preset = cfg.resolve_preset("discover").expect("preset");
        let profile = empty_profile();
        let resonated = vec!["Good Band".to_string()];
        let rejected = vec!["Bad Band".to_string()];
        let msg = user_message(&PromptInput {
            preset: &preset,
            profile: &profile,
            resonated: &resonated,
            rejected: &rejected,
            request_count: 10,
            exclusions: &[],
            blocked_artists: &[],
            extra_instructions: None,
            market: None,
        });
        assert!(msg.contains("Good Band"));
        assert!(msg.contains("Bad Band"));
        assert!(msg.contains("HOW YOUR PREVIOUS PICKS LANDED"));
    }

    #[test]
    fn discovery_levels_are_distinct_and_total() {
        let mut seen = std::collections::HashSet::new();
        for level in 0..=10u8 {
            seen.insert(discovery_clause(level));
        }
        assert_eq!(
            seen.len(),
            5,
            "every level must map to one of the five bands"
        );
        assert!(discovery_clause(10).contains("underground"));
        assert!(discovery_clause(0).contains("well-known"));
    }

    #[test]
    fn russian_policy_is_explicit() {
        assert!(language_clause(LanguagePolicy::Russian).contains("Russian-language"));
    }
}
