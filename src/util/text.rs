//! Text normalisation, script detection and fuzzy matching.
//!
//! Used in two places that both need to be forgiving but not sloppy:
//!   * resolving a Claude-suggested `(title, artist)` pair to a real Spotify
//!     track, where the model may differ in punctuation, feat. spelling,
//!     remaster suffixes or diacritics;
//!   * enforcing the language policy, where we classify a track by the script
//!     of its title and artist name.

use std::collections::HashSet;
use unicode_normalization::UnicodeNormalization;

/// Parenthetical/bracketed noise Spotify adds to titles. Stripped before
/// comparison so "Song (2011 Remaster)" matches "Song".
const NOISE_MARKERS: &[&str] = &[
    "remaster",
    "remastered",
    "radio edit",
    "album version",
    "single version",
    "bonus track",
    "deluxe",
    "live at",
    "mono version",
    "stereo version",
    "anniversary edition",
    "explicit",
    "feat.",
    "feat ",
    "featuring",
    "with ",
];

/// Lowercase, strip diacritics, collapse punctuation and whitespace.
pub fn normalize(input: &str) -> String {
    let decomposed: String = input.nfkd().collect();
    let mut out = String::with_capacity(decomposed.len());
    let mut last_space = true;
    for ch in decomposed.chars() {
        // Combining marks (U+0300..U+036F) are the diacritics NFKD split off.
        if ('\u{0300}'..='\u{036F}').contains(&ch) {
            continue;
        }
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

/// Normalisation plus removal of edition/version noise. Used for the strict
/// "is this the same song" comparison.
pub fn normalize_title(input: &str) -> String {
    let stripped = strip_bracketed(input);
    let mut n = normalize(&stripped);
    for marker in NOISE_MARKERS {
        if let Some(idx) = n.find(marker) {
            n.truncate(idx);
        }
    }
    n.trim().to_string()
}

/// Remove `(...)`, `[...]` and everything after a ` - ` suffix that contains a
/// noise marker (Spotify's canonical "Song - 2011 Remaster" shape).
fn strip_bracketed(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut depth = 0usize;
    for ch in input.chars() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    if let Some(idx) = out.find(" - ") {
        let tail = out[idx..].to_lowercase();
        if NOISE_MARKERS.iter().any(|m| tail.contains(m)) {
            out.truncate(idx);
        }
    }
    out
}

/// Token-set Jaccard similarity in `[0.0, 1.0]` over normalised words.
///
/// Chosen over edit distance because the dominant failure mode is *word*
/// level (extra "feat. X", reordered artist credit), not character level.
pub fn similarity(a: &str, b: &str) -> f32 {
    let at: HashSet<&str> = a.split_whitespace().collect();
    let bt: HashSet<&str> = b.split_whitespace().collect();
    if at.is_empty() || bt.is_empty() {
        return if at.is_empty() && bt.is_empty() {
            1.0
        } else {
            0.0
        };
    }
    let inter = at.intersection(&bt).count() as f32;
    let union = at.union(&bt).count() as f32;
    inter / union
}

// ---------------------------------------------------------------------------
// Script / language classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum Script {
    Latin,
    Cyrillic,
    Cjk,
    Other,
    /// No cased letters at all (e.g. a purely numeric title).
    Unknown,
}

/// Classify by dominant script of the cased characters.
///
/// This is a heuristic, and it is honest about that: it identifies the writing
/// system, not the language. It reliably separates Russian from English — the
/// distinction the language policy actually cares about — and defers anything
/// subtler to Claude via prompt instructions.
pub fn detect_script(input: &str) -> Script {
    let (mut latin, mut cyr, mut cjk, mut other) = (0usize, 0usize, 0usize, 0usize);
    for ch in input.chars() {
        if !ch.is_alphabetic() {
            continue;
        }
        match ch {
            'a'..='z' | 'A'..='Z' | '\u{00C0}'..='\u{024F}' => latin += 1,
            '\u{0400}'..='\u{04FF}' | '\u{0500}'..='\u{052F}' => cyr += 1,
            '\u{3040}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}' | '\u{AC00}'..='\u{D7AF}' => cjk += 1,
            _ => other += 1,
        }
    }
    let total = latin + cyr + cjk + other;
    if total == 0 {
        return Script::Unknown;
    }
    let max = latin.max(cyr).max(cjk).max(other);
    if max == cyr {
        Script::Cyrillic
    } else if max == cjk {
        Script::Cjk
    } else if max == latin {
        Script::Latin
    } else {
        Script::Other
    }
}

/// Classify a track using title first, artist as a tiebreaker.
///
/// Title wins because a Cyrillic-named artist can release an English song and
/// vice versa; the sung language tracks the title far more often.
pub fn detect_track_script(title: &str, artist: &str) -> Script {
    match detect_script(title) {
        Script::Unknown => detect_script(artist),
        s => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_remaster_noise() {
        assert_eq!(normalize_title("Kashmir - 2012 Remaster"), "kashmir");
        assert_eq!(normalize_title("Money (Remastered 2011)"), "money");
        assert_eq!(normalize_title("Björk — Jóga"), "bjork joga");
    }

    #[test]
    fn similarity_is_order_insensitive() {
        let a = normalize("Daft Punk feat. Pharrell");
        let b = normalize("Pharrell, Daft Punk");
        assert!(similarity(&a, &b) > 0.4);
    }

    #[test]
    fn detects_scripts() {
        assert_eq!(detect_script("Молчат Дома"), Script::Cyrillic);
        assert_eq!(detect_script("Massive Attack"), Script::Latin);
        assert_eq!(
            detect_track_script("1979", "Smashing Pumpkins"),
            Script::Latin
        );
    }
}
