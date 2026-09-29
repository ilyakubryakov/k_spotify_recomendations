//! Rolling playlists — a fixed-size buffer that turns over gradually.
//!
//! `replace` rebuilds the playlist every run, so anything the listener has not
//! got to yet is gone. `append` grows without bound. `rolling` keeps the
//! playlist at exactly `size` by adding the new picks and evicting the *least
//! engaging* incumbents to make room.
//!
//! Two guards keep the turnover honest:
//!
//! * **A grace period.** A track added days ago has not had a fair hearing;
//!   it cannot be evicted until `grace_days` have passed. Without this, a
//!   track added on Monday could be thrown out on Tuesday purely for being
//!   unplayed, which is exactly the "unheard material washed away" failure the
//!   rolling buffer is meant to prevent.
//! * **An eviction cap.** At most `max_evictions` tracks leave per run, so one
//!   burst of negative feedback cannot flush the whole playlist.
//!
//! Eviction order is by engagement ascending: removed/skipped material goes
//! first, then never-played, then played-but-unloved. Ties break toward the
//! oldest, so the buffer drains front-to-back.

/// A track already in the playlist.
#[derive(Debug, Clone)]
pub struct Incumbent {
    pub track_id: String,
    pub display: String,
    pub added_at_ms: i64,
    /// Accumulated feedback for this track; negative means poorly received.
    pub feedback: f32,
    pub plays: u32,
}

impl Incumbent {
    /// Lower is more evictable.
    ///
    /// A play is worth a modest positive on its own: the listener at least got
    /// to it, which is more than an untouched track can say.
    fn engagement(&self) -> f32 {
        self.feedback + (self.plays as f32 * 0.5).min(3.0)
    }

    fn evictable(&self, now_ms: i64, grace_days: u32) -> bool {
        if grace_days == 0 {
            return true;
        }
        let age_days = (now_ms - self.added_at_ms) / 86_400_000;
        age_days >= i64::from(grace_days)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RollingOptions {
    pub size: usize,
    pub max_evictions: usize,
    pub grace_days: u32,
    pub now_ms: i64,
}

#[derive(Debug)]
pub struct RollingPlan {
    /// Final playlist contents, in order: survivors first, new picks appended.
    pub final_ids: Vec<String>,
    pub evicted: Vec<Incumbent>,
    /// How many of the offered additions were actually used.
    pub added: usize,
    /// Additions that did not fit because nothing could be evicted.
    pub deferred: usize,
}

/// Decide what stays, what goes, and how many new tracks fit.
///
/// `incumbents` must be in playlist order; `additions` are the new track ids
/// in the order the model sequenced them.
pub fn plan(
    incumbents: Vec<Incumbent>,
    additions: Vec<String>,
    opts: RollingOptions,
) -> RollingPlan {
    let size = opts.size.max(1);

    // How many must leave for the additions to fit.
    let overflow = (incumbents.len() + additions.len()).saturating_sub(size);

    // Rank the evictable incumbents worst-first.
    let mut candidates: Vec<usize> = incumbents
        .iter()
        .enumerate()
        .filter(|(_, inc)| inc.evictable(opts.now_ms, opts.grace_days))
        .map(|(index, _)| index)
        .collect();
    candidates.sort_by(|a, b| {
        // `get` rather than indexing: the indices come from `incumbents`
        // itself so they are always valid, but a total comparator is cheaper
        // to reason about than a panic that "cannot happen".
        match (incumbents.get(*a), incumbents.get(*b)) {
            (Some(ia), Some(ib)) => ia
                .engagement()
                .partial_cmp(&ib.engagement())
                .unwrap_or(std::cmp::Ordering::Equal)
                // Oldest first on a tie, so the buffer drains front-to-back.
                .then_with(|| ia.added_at_ms.cmp(&ib.added_at_ms)),
            _ => std::cmp::Ordering::Equal,
        }
    });

    let evict_count = overflow.min(opts.max_evictions).min(candidates.len());
    let evicted_indices: std::collections::HashSet<usize> =
        candidates.into_iter().take(evict_count).collect();

    let mut evicted = Vec::with_capacity(evict_count);
    let mut final_ids = Vec::with_capacity(size);
    for (index, incumbent) in incumbents.into_iter().enumerate() {
        if evicted_indices.contains(&index) {
            evicted.push(incumbent);
        } else {
            final_ids.push(incumbent.track_id);
        }
    }

    // Whatever room is left goes to the new picks, in the model's order.
    let room = size.saturating_sub(final_ids.len());
    let added = room.min(additions.len());
    let deferred = additions.len() - added;
    final_ids.extend(additions.into_iter().take(added));

    RollingPlan {
        final_ids,
        evicted,
        added,
        deferred,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 1_000 * DAY;

    fn incumbent(id: &str, age_days: i64, feedback: f32, plays: u32) -> Incumbent {
        Incumbent {
            track_id: id.into(),
            display: id.into(),
            added_at_ms: NOW - age_days * DAY,
            feedback,
            plays,
        }
    }

    fn opts(size: usize, max_evictions: usize, grace_days: u32) -> RollingOptions {
        RollingOptions {
            size,
            max_evictions,
            grace_days,
            now_ms: NOW,
        }
    }

    #[test]
    fn keeps_the_playlist_at_exactly_size() {
        let incumbents: Vec<Incumbent> = (0..50)
            .map(|i| incumbent(&format!("old{i}"), 30, 0.0, 0))
            .collect();
        let additions: Vec<String> = (0..5).map(|i| format!("new{i}")).collect();

        let plan = plan(incumbents, additions, opts(50, 10, 7));
        assert_eq!(plan.final_ids.len(), 50);
        assert_eq!(plan.evicted.len(), 5);
        assert_eq!(plan.added, 5);
    }

    #[test]
    fn evicts_the_worst_received_first() {
        let incumbents = vec![
            incumbent("loved", 30, 5.0, 10),
            incumbent("hated", 30, -4.0, 0),
            incumbent("ignored", 30, 0.0, 0),
        ];
        let plan = plan(incumbents, vec!["new".into()], opts(3, 10, 7));

        let evicted: Vec<&str> = plan.evicted.iter().map(|i| i.track_id.as_str()).collect();
        assert_eq!(evicted, vec!["hated"]);
        assert!(plan.final_ids.contains(&"loved".to_string()));
    }

    #[test]
    fn a_recent_track_is_protected_by_the_grace_period() {
        // "fresh" is the worst-scoring but was added yesterday, so it must
        // survive; the older, better-scoring track goes instead.
        let incumbents = vec![
            incumbent("fresh", 1, -5.0, 0),
            incumbent("old", 60, -1.0, 0),
        ];
        let plan = plan(incumbents, vec!["new".into()], opts(2, 10, 7));

        let evicted: Vec<&str> = plan.evicted.iter().map(|i| i.track_id.as_str()).collect();
        assert_eq!(evicted, vec!["old"]);
        assert!(plan.final_ids.contains(&"fresh".to_string()));
    }

    #[test]
    fn the_eviction_cap_defers_additions_rather_than_overflowing() {
        let incumbents: Vec<Incumbent> = (0..50)
            .map(|i| incumbent(&format!("old{i}"), 30, -1.0, 0))
            .collect();
        let additions: Vec<String> = (0..20).map(|i| format!("new{i}")).collect();

        let plan = plan(incumbents, additions, opts(50, 5, 7));
        assert_eq!(plan.evicted.len(), 5, "the cap must hold");
        assert_eq!(plan.added, 5);
        assert_eq!(plan.deferred, 15);
        assert_eq!(plan.final_ids.len(), 50, "size is still exact");
    }

    #[test]
    fn nothing_evictable_means_nothing_added() {
        // Every incumbent is inside the grace period.
        let incumbents: Vec<Incumbent> = (0..10)
            .map(|i| incumbent(&format!("fresh{i}"), 0, -5.0, 0))
            .collect();
        let plan = plan(incumbents, vec!["new".into()], opts(10, 10, 7));

        assert!(plan.evicted.is_empty());
        assert_eq!(plan.added, 0);
        assert_eq!(plan.deferred, 1);
        assert_eq!(plan.final_ids.len(), 10);
    }

    #[test]
    fn an_underfull_playlist_just_fills_up() {
        let incumbents = vec![incumbent("a", 30, 0.0, 0)];
        let additions: Vec<String> = (0..5).map(|i| format!("new{i}")).collect();

        let plan = plan(incumbents, additions, opts(10, 10, 7));
        assert!(
            plan.evicted.is_empty(),
            "no need to evict when there is room"
        );
        assert_eq!(plan.added, 5);
        assert_eq!(plan.final_ids.len(), 6);
    }

    #[test]
    fn survivors_keep_their_order_and_new_tracks_go_last() {
        let incumbents = vec![
            incumbent("a", 30, 5.0, 0),
            incumbent("b", 30, -9.0, 0),
            incumbent("c", 30, 5.0, 0),
        ];
        let plan = plan(incumbents, vec!["new".into()], opts(3, 10, 7));
        assert_eq!(
            plan.final_ids,
            vec!["a".to_string(), "c".to_string(), "new".to_string()]
        );
    }
}
