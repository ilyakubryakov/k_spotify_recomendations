//! Crate-wide error type.
//!
//! Design rules:
//!   * Every fallible boundary returns [`Result`]; the binary has exactly one
//!     `main` that converts an error into an exit code + a `tracing` record.
//!   * Errors never carry secrets. Anything derived from a token, API key or
//!     authorization code is reduced to a shape description before it lands
//!     in a variant.
//!   * Retryability is data, not a guess: [`AgentError::is_retryable`] and
//!     [`AgentError::retry_after`] drive the shared backoff logic so the
//!     Spotify and Anthropic clients cannot disagree about what to retry.

use std::time::Duration;

pub type Result<T, E = AgentError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    // ---- configuration -------------------------------------------------
    #[error("configuration error: {0}")]
    Config(String),

    #[error("missing credential `{name}`: {hint}")]
    MissingCredential { name: &'static str, hint: String },

    // ---- authorisation -------------------------------------------------
    #[error("spotify authorization required: run `spotify-agent login`")]
    NotAuthorized,

    #[error("spotify authorization failed: {0}")]
    Auth(String),

    // ---- remote APIs ---------------------------------------------------
    /// A non-retryable HTTP failure carrying the upstream status + message.
    #[error("{service} API error {status}: {message}")]
    Api {
        service: &'static str,
        status: u16,
        message: String,
    },

    /// HTTP 429. `retry_after` is the server-supplied delay when present.
    #[error("{service} rate limited (retry after {retry_after:?})")]
    RateLimited {
        service: &'static str,
        retry_after: Option<Duration>,
    },

    /// 5xx / 529 overloaded — retryable.
    #[error("{service} transient failure {status}: {message}")]
    Transient {
        service: &'static str,
        status: u16,
        message: String,
    },

    #[error("{service} request exhausted {attempts} attempts: {source}")]
    RetriesExhausted {
        service: &'static str,
        attempts: u32,
        #[source]
        source: Box<AgentError>,
    },

    // ---- model-specific ------------------------------------------------
    /// Claude returned `stop_reason: "refusal"` — the safety classifier
    /// declined. Not retryable; the caller should surface it verbatim.
    #[error("claude declined the request ({category}): {explanation}")]
    ModelRefusal {
        category: String,
        explanation: String,
    },

    #[error("claude returned {0}")]
    ModelProtocol(String),

    // ---- local ---------------------------------------------------------
    #[error("storage error: {0}")]
    Storage(#[from] rusqlite::Error),

    #[error("i/o error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("http transport error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("operation cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl AgentError {
    pub fn io(path: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// Whether the shared retry loop should try this request again.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Transient { .. } => true,
            // Connection resets / timeouts are worth another attempt; a body
            // decode failure is not.
            Self::Transport(e) => e.is_timeout() || e.is_connect() || e.is_request(),
            _ => false,
        }
    }

    /// Server-requested delay, if the response carried one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Process exit code. Distinct codes let cron/systemd alert selectively.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) | Self::MissingCredential { .. } => 78, // EX_CONFIG
            Self::NotAuthorized | Self::Auth(_) => 77,              // EX_NOPERM
            Self::RateLimited { .. } | Self::Transient { .. } | Self::RetriesExhausted { .. } => 75, // EX_TEMPFAIL
            Self::Api { .. } | Self::ModelRefusal { .. } | Self::ModelProtocol(_) => 69, // EX_UNAVAILABLE
            Self::Cancelled => 130,
            _ => 1,
        }
    }
}
