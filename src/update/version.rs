//! Release version parsing and ordering.
//!
//! Only the subset of SemVer that this project's tags actually use is
//! implemented — `MAJOR.MINOR.PATCH` with an optional `-prerelease` — but the
//! ordering rules are the real ones, because the whole point of the type is to
//! answer "is the release newer than what is running?" without guessing.
//!
//! Build metadata (`+sha`) is parsed and then ignored, as SemVer requires: two
//! versions differing only in metadata are the same version, and offering an
//! "update" between them would be a download for nothing.

use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Dot-separated pre-release identifiers. Empty means a final release.
    pub pre: Vec<String>,
}

impl Version {
    /// Parse `1.2.3`, `v1.2.3`, `1.2.3-rc.1`, `1.2.3-rc.1+abc`.
    ///
    /// Returns `None` rather than a partial parse: a tag we cannot read is a
    /// tag we must not compare against, or we would offer a downgrade.
    pub fn parse(raw: &str) -> Option<Self> {
        let text = raw.trim();
        let text = text
            .strip_prefix('v')
            .or(text.strip_prefix('V'))
            .unwrap_or(text);
        // Metadata is not part of precedence, so it is dropped here rather
        // than carried around and forgotten about at the comparison site.
        let text = text.split('+').next()?;
        let (core, pre) = match text.split_once('-') {
            // `1.2.3-` is not `1.2.3`: an empty pre-release is malformed, and
            // silently dropping the hyphen would make it compare as a final
            // release.
            Some((_, "")) => return None,
            Some((core, pre)) => (core, pre),
            None => (text, ""),
        };

        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }

        let pre = if pre.is_empty() {
            Vec::new()
        } else {
            let ids: Vec<String> = pre.split('.').map(str::to_string).collect();
            if ids.iter().any(|id| id.is_empty()) {
                return None;
            }
            ids
        };

        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// The version this binary was built as.
    pub fn current() -> Self {
        // Cargo guarantees this parses; the fallback exists only so the
        // function can stay infallible without an `expect`.
        Self::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Self {
            major: 0,
            minor: 0,
            patch: 0,
            pre: Vec::new(),
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
            .then_with(|| compare_pre(&self.pre, &other.pre))
    }
}

/// SemVer §11.4: a version with a pre-release ranks *below* the same version
/// without one, and identifiers compare field by field — numerically when both
/// are numeric, lexically otherwise, with numeric ranking below alphanumeric.
fn compare_pre(a: &[String], b: &[String]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }

    for (x, y) in a.iter().zip(b.iter()) {
        let ordering = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(xn), Ok(yn)) => xn.cmp(&yn),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    a.len().cmp(&b.len())
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(raw: &str) -> Version {
        Version::parse(raw).expect("parses")
    }

    #[test]
    fn a_leading_v_is_optional() {
        assert_eq!(v("v1.2.3"), v("1.2.3"));
    }

    #[test]
    fn ordering_is_numeric_not_lexical() {
        // The bug this guards against: "0.10.0" < "0.9.0" as strings.
        assert!(v("0.10.0") > v("0.9.0"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert!(v("0.1.10") > v("0.1.9"));
    }

    #[test]
    fn a_prerelease_ranks_below_its_release() {
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(v("1.0.0-rc.1") < v("1.0.0-rc.2"));
        assert!(v("1.0.0-alpha") < v("1.0.0-beta"));
        // Numeric identifiers rank below alphanumeric ones.
        assert!(v("1.0.0-1") < v("1.0.0-alpha"));
        // More identifiers win when the shared prefix is equal.
        assert!(v("1.0.0-rc") < v("1.0.0-rc.1"));
    }

    #[test]
    fn build_metadata_does_not_affect_precedence() {
        assert_eq!(v("1.2.3+abc"), v("1.2.3+def"));
        assert_eq!(v("1.2.3-rc.1+abc"), v("1.2.3-rc.1"));
    }

    #[test]
    fn a_tag_we_cannot_read_is_rejected_rather_than_guessed() {
        for bad in ["", "v", "1.2", "1.2.3.4", "latest", "1.2.x", "1.2.3-"] {
            assert!(Version::parse(bad).is_none(), "{bad} should not parse");
        }
    }

    #[test]
    fn display_round_trips() {
        for raw in ["1.2.3", "0.1.0-rc.1", "10.0.0-alpha.2"] {
            assert_eq!(v(raw).to_string(), raw);
        }
    }

    #[test]
    fn the_crate_version_is_parseable() {
        // If this fails, every update check would silently compare against
        // 0.0.0 and offer an update on every run.
        assert!(Version::parse(env!("CARGO_PKG_VERSION")).is_some());
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }
}
