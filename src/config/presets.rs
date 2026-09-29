//! Built-in presets.
//!
//! A preset is mostly a *brief*: the natural-language paragraph handed to
//! Claude. Everything else (size, language policy, artist caps) is mechanical
//! filtering around it. Briefs are written in the second person and describe
//! the listening situation rather than naming genres, because naming genres
//! collapses the model's range — it starts returning the canonical five bands
//! of that genre instead of reading the user's actual profile.

use super::{FillStrategy, LanguagePolicy, Preset};
use std::collections::BTreeMap;

pub fn builtin() -> BTreeMap<String, Preset> {
    let mut m = BTreeMap::new();

    m.insert(
        "discover".into(),
        Preset {
            label: "Discover — adjacent to taste, mostly unheard".into(),
            brief: "Expand the listener's taste outward by one step. Favour artists they have \
demonstrably not heard, but that sit plausibly next to their strongest affinities — a shared \
scene, producer, label, era or sonic signature. Avoid the obvious chart-topping representative \
of each genre; the listener already knows those. Aim for a set that feels like a knowledgeable \
friend's recommendation rather than an algorithmic 'more like this'."
                .into(),
            moods: vec!["curious".into(), "fresh".into()],
            size: Some(30),
            max_per_artist: Some(1),
            ..Default::default()
        },
    );

    m.insert(
        "road_trip".into(),
        Preset {
            label: "Road trip — long-drive momentum".into(),
            brief: "Build a driving playlist for a multi-hour daytime drive. It needs forward \
momentum and a steady pulse: mid-to-up tempo, strong rhythm section, choruses that carry over \
road noise. Sequence it with a lift — start warm and open, build energy through the middle, and \
leave the last few tracks slightly more expansive. Avoid ambient, sparse ballads, and anything \
that demands close listening."
                .into(),
            moods: vec!["driving".into(), "anthemic".into(), "open-road".into()],
            size: Some(50),
            max_per_artist: Some(2),
            ..Default::default()
        },
    );

    m.insert(
        "focus".into(),
        Preset {
            label: "Focus — deep work, low lyrical load".into(),
            brief: "Assemble a background set for several hours of concentrated work. Prioritise \
consistent texture and low attentional cost: instrumental or near-instrumental, minimal vocal \
hooks, no abrupt dynamic jumps, no tracks that beg to be sung along to. Steady tempo and a \
coherent timbral palette across the whole playlist matter more than variety. Ambient, modern \
classical, downtempo electronic, post-rock and jazz all qualify if they stay unobtrusive."
                .into(),
            moods: vec!["calm".into(), "sustained".into(), "textural".into()],
            size: Some(40),
            max_per_artist: Some(3),
            ..Default::default()
        },
    );

    m.insert(
        "workout".into(),
        Preset {
            label: "Workout — high intensity".into(),
            brief: "Build a training playlist for high-intensity interval work. Every track needs \
a clear, driving beat in roughly the 120–175 BPM range, an aggressive or euphoric energy, and \
an early payoff — no two-minute intros. Front-load the hardest-hitting material. Avoid slow \
builds, ballads and anything melancholic."
                .into(),
            moods: vec!["aggressive".into(), "euphoric".into(), "relentless".into()],
            size: Some(35),
            max_per_artist: Some(2),
            ..Default::default()
        },
    );

    m.insert(
        "late_night".into(),
        Preset {
            label: "Late night — dim, slow, intimate".into(),
            brief: "Curate for the hours after midnight. Dim, spacious and intimate: slower \
tempos, warm low end, restrained production, vocals that sit close to the microphone. Melancholy \
is welcome; bombast is not. The set should feel continuous, as though it were one long dimly lit \
room rather than a sequence of singles."
                .into(),
            moods: vec!["nocturnal".into(), "intimate".into(), "hazy".into()],
            size: Some(30),
            max_per_artist: Some(2),
            ..Default::default()
        },
    );

    m.insert(
        "ru_wave".into(),
        Preset {
            label: "Русская волна — Russian-language selection".into(),
            brief: "Curate a Russian-language set. Draw on the full breadth of the \
Russian-speaking scene — post-punk, indie, rap, electronic, singer-songwriter — rather than \
defaulting to the handful of internationally known acts. Lyrics must be predominantly in \
Russian. Read the listener's profile for which registers they actually respond to and stay \
inside that emotional range."
                .into(),
            moods: vec!["атмосферный".into(), "меланхоличный".into()],
            size: Some(30),
            language: Some(LanguagePolicy::Russian),
            max_per_artist: Some(2),
            ..Default::default()
        },
    );

    m.insert(
        "on_repeat".into(),
        Preset {
            label: "On repeat — deepen current obsessions".into(),
            brief: "The listener is currently fixated on a small number of tracks and artists \
(visible in the 'looped' section of the profile). Go deeper into exactly that vein: other work \
by the same scene, the records those artists themselves cite, the tracks a fan of this specific \
sound would reach for next. This is depth, not breadth — a narrow, coherent set is the goal."
                .into(),
            moods: vec!["obsessive".into(), "focused".into()],
            size: Some(25),
            strategy: Some(FillStrategy::Append),
            max_per_artist: Some(3),
            // This preset exists to go *deeper* into current obsessions, so
            // the global artist cooldown is explicitly switched off for it.
            artist_cooldown_days: Some(0),
            ..Default::default()
        },
    );

    m
}

#[cfg(test)]
mod tests {
    #[test]
    fn builtin_presets_have_briefs() {
        for (name, preset) in super::builtin() {
            assert!(!preset.brief.trim().is_empty(), "{name} has no brief");
            assert!(!preset.label.trim().is_empty(), "{name} has no label");
        }
    }
}
