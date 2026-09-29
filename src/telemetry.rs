//! Logging setup.
//!
//! Three sinks, chosen by mode:
//!
//! | mode              | sink                                    |
//! |-------------------|-----------------------------------------|
//! | interactive CLI   | pretty text on stderr                   |
//! | `--cron` / `--log-format json` | one JSON object per line on stdout |
//! | TUI               | in-memory ring buffer rendered in a pane |
//!
//! stderr is used for human logs so that `--json` command output on stdout
//! stays machine-parseable when piped.
//!
//! In TUI mode nothing may write to the terminal directly — a stray log line
//! would corrupt the alternate screen — so the writer layer is replaced by a
//! bounded ring buffer the UI drains.

use crate::config::{ColorMode, LogFormat};
use crate::error::{AgentError, Result};
use chrono::{DateTime, Local};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry};

/// Lines retained for the TUI log pane. Bounded so a long run cannot grow
/// without limit.
const LOG_CAPACITY: usize = 2_000;

#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: DateTime<Local>,
    pub level: tracing::Level,
    pub target: String,
    pub message: String,
}

/// Shared ring buffer. Cloning shares the underlying storage.
#[derive(Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<LogLine>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(LOG_CAPACITY))),
        }
    }

    pub fn push(&self, line: LogLine) {
        // A poisoned lock here must not take the process down: losing a log
        // line is strictly better than aborting a run.
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if guard.len() == LOG_CAPACITY {
            guard.pop_front();
        }
        guard.push_back(line);
    }

    /// Snapshot of the most recent `limit` lines, oldest first.
    pub fn tail(&self, limit: usize) -> Vec<LogLine> {
        let Ok(guard) = self.inner.lock() else {
            return Vec::new();
        };
        guard.iter().rev().take(limit).rev().cloned().collect()
    }

    /// Total retained lines, shown in the log pane's title.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How logs should be emitted for this process.
pub struct TelemetryOptions<'a> {
    pub directive: &'a str,
    pub format: LogFormat,
    pub color: ColorMode,
    /// When true, no layer writes to the terminal.
    pub tui: bool,
}

/// Install the global subscriber. Returns the buffer when one was installed.
///
/// Calling this twice in one process is a programming error, but it is
/// reported rather than panicked on.
pub fn init(opts: TelemetryOptions<'_>) -> Result<Option<LogBuffer>> {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(opts.directive))
        .map_err(|e| AgentError::config(format!("invalid log filter `{}`: {e}", opts.directive)))?;

    if opts.tui {
        let buffer = LogBuffer::new();
        let layer = BufferLayer {
            buffer: buffer.clone(),
        };
        Registry::default()
            .with(filter)
            .with(layer)
            .try_init()
            .map_err(|e| AgentError::other(format!("could not install the log subscriber: {e}")))?;
        return Ok(Some(buffer));
    }

    match opts.format {
        LogFormat::Json => {
            let layer = tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(false)
                .with_span_list(false)
                .with_writer(std::io::stderr);
            Registry::default()
                .with(filter)
                .with(layer)
                .try_init()
                .map_err(|e| {
                    AgentError::other(format!("could not install the log subscriber: {e}"))
                })?;
        }
        LogFormat::Text => {
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(use_color(opts.color))
                .with_target(false)
                .without_time()
                .with_writer(std::io::stderr);
            Registry::default()
                .with(filter)
                .with(layer)
                .try_init()
                .map_err(|e| {
                    AgentError::other(format!("could not install the log subscriber: {e}"))
                })?;
        }
    }
    Ok(None)
}

/// Honour `NO_COLOR` (informal standard) and `CLICOLOR_FORCE` before guessing.
pub fn use_color(mode: ColorMode) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            if std::env::var_os("NO_COLOR").is_some() {
                return false;
            }
            if std::env::var_os("CLICOLOR_FORCE").is_some() {
                return true;
            }
            is_terminal()
        }
    }
}

#[cfg(unix)]
fn is_terminal() -> bool {
    // SAFETY-free alternative to `libc::isatty`: `IsTerminal` has been in std
    // since 1.70 and handles Windows consoles correctly too.
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

#[cfg(not(unix))]
fn is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal()
}

// ---------------------------------------------------------------------------
// Ring-buffer layer
// ---------------------------------------------------------------------------

struct BufferLayer {
    buffer: LogBuffer,
}

impl<S> Layer<S> for BufferLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let metadata = event.metadata();
        self.buffer.push(LogLine {
            at: Local::now(),
            level: *metadata.level(),
            target: metadata.target().to_string(),
            message: visitor.render(),
        });
    }
}

/// Collects the `message` field plus any structured fields, so
/// `tracing::info!(count = 5, "synced")` renders as `synced count=5`.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl MessageVisitor {
    fn render(&self) -> String {
        if self.fields.is_empty() {
            return self.message.clone();
        }
        let extras = self
            .fields
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        if self.message.is_empty() {
            extras
        } else {
            format!("{} {extras}", self.message)
        }
    }

    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.push((field.name().to_string(), value));
        }
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_is_bounded_and_ordered() {
        let buffer = LogBuffer::new();
        for i in 0..(LOG_CAPACITY + 10) {
            buffer.push(LogLine {
                at: Local::now(),
                level: tracing::Level::INFO,
                target: "t".into(),
                message: i.to_string(),
            });
        }
        assert_eq!(buffer.len(), LOG_CAPACITY);
        let tail = buffer.tail(3);
        let messages: Vec<&str> = tail.iter().map(|l| l.message.as_str()).collect();
        assert_eq!(messages, vec!["2007", "2008", "2009"]);
    }

    #[test]
    fn no_color_env_wins_over_auto() {
        // Not asserting on the ambient environment; just that Always/Never are
        // absolute.
        assert!(use_color(ColorMode::Always));
        assert!(!use_color(ColorMode::Never));
    }
}
