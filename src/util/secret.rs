//! A string that never leaks through `Debug`, `Display` or `Serialize`.
//!
//! API keys, refresh tokens and access tokens are wrapped in [`Secret`]. The
//! only way to read the plaintext is [`Secret::expose`], which is greppable —
//! so an audit of "where do secrets escape" is `rg '\.expose\(\)'`.

use std::fmt;

#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Deliberately verbose name: every call site is an intentional disclosure.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Non-reversible fingerprint, safe to log when correlating "which key".
    pub fn hint(&self) -> String {
        let n = self.0.chars().count();
        if n <= 8 {
            format!("<redacted:{n} chars>")
        } else {
            let tail: String = self.0.chars().skip(n - 4).collect();
            format!("<redacted:{n} chars …{tail}>")
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Deserialises from a plain TOML/JSON string.
impl<'de> serde::Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Secret)
    }
}

/// Serialises as `"<redacted>"`. This is intentional: `config show` and any
/// accidental state dump can never round-trip a real secret to disk or stdout.
/// Token persistence uses [`Secret::expose`] explicitly instead.
impl serde::Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("<redacted>")
    }
}
