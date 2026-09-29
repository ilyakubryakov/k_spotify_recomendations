//! One retry policy, shared by every outbound HTTP call.
//!
//! Both the Spotify and Anthropic clients funnel through [`with_retry`], so
//! their behaviour under load cannot drift apart. The policy is:
//!
//!   * honour a server-supplied `Retry-After` exactly (Spotify's 429 uses it
//!     and can ask for minutes — sleeping less is how you get soft-banned);
//!   * otherwise exponential backoff with full jitter, capped;
//!   * a hard ceiling on total wait so a cron run cannot hang forever.

use crate::error::{AgentError, Result};
use rand::Rng;
use std::future::Future;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Upper bound on cumulative sleeping across all attempts.
    pub max_total_delay: Duration,
    /// A `Retry-After` longer than this aborts instead of sleeping: better to
    /// exit 75 (EX_TEMPFAIL) and let cron retry than to hold the process.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            max_total_delay: Duration::from_secs(180),
            max_retry_after: Duration::from_secs(120),
        }
    }
}

impl RetryPolicy {
    fn backoff_for(&self, attempt: u32) -> Duration {
        // attempt is 1-based; shift saturates well before overflow.
        let exp = self
            .initial_backoff
            .saturating_mul(1u32 << attempt.min(16).saturating_sub(1));
        let capped = exp.min(self.max_backoff);
        // Full jitter (AWS "Exponential Backoff and Jitter"): uniform in
        // [0, capped]. Decorrelates retries when many searches fail at once.
        let millis = capped.as_millis().min(u128::from(u64::MAX)) as u64;
        if millis == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(rand::thread_rng().gen_range(0..=millis))
    }
}

/// Run `op` under the policy. `op` is a closure returning a fresh future each
/// attempt (requests are not clonable, so we rebuild them).
pub async fn with_retry<F, Fut, T>(
    service: &'static str,
    policy: RetryPolicy,
    mut op: F,
) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let started = Instant::now();
    let mut last: Option<AgentError> = None;

    for attempt in 1..=policy.max_attempts {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(err) => {
                if !err.is_retryable() {
                    return Err(err);
                }

                let wait = match err.retry_after() {
                    Some(d) if d > policy.max_retry_after => {
                        tracing::warn!(
                            service,
                            requested_s = d.as_secs(),
                            cap_s = policy.max_retry_after.as_secs(),
                            "Retry-After exceeds cap; aborting so the caller can reschedule"
                        );
                        return Err(err);
                    }
                    // +250ms guard: clocks and rounding differ; undershooting a
                    // Retry-After earns another 429 and a worse penalty.
                    Some(d) => d + Duration::from_millis(250),
                    None => policy.backoff_for(attempt),
                };

                if started.elapsed() + wait > policy.max_total_delay {
                    tracing::warn!(service, "retry budget exhausted");
                    return Err(AgentError::RetriesExhausted {
                        service,
                        attempts: attempt,
                        source: Box::new(err),
                    });
                }

                tracing::warn!(
                    service,
                    attempt,
                    max = policy.max_attempts,
                    backoff_ms = wait.as_millis(),
                    error = %err,
                    "retrying"
                );
                last = Some(err);
                tokio::time::sleep(wait).await;
            }
        }
    }

    Err(AgentError::RetriesExhausted {
        service,
        attempts: policy.max_attempts,
        source: Box::new(
            last.unwrap_or_else(|| AgentError::other("retry loop ended with no error")),
        ),
    })
}

/// Parse a `Retry-After` header. Spotify sends integer seconds; the HTTP spec
/// also allows an HTTP-date, which we accept via chrono.
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let raw = value?.trim();
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(Duration::from_secs(secs.min(3600)));
    }
    let when = chrono::DateTime::parse_from_rfc2822(raw).ok()?;
    let delta = when.timestamp() - chrono::Utc::now().timestamp();
    (delta > 0).then(|| Duration::from_secs((delta as u64).min(3600)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_retry_after() {
        assert_eq!(parse_retry_after(Some("30")), Some(Duration::from_secs(30)));
        assert_eq!(
            parse_retry_after(Some("  7 ")),
            Some(Duration::from_secs(7))
        );
        assert_eq!(parse_retry_after(None), None);
        assert_eq!(parse_retry_after(Some("nonsense")), None);
    }

    #[test]
    fn backoff_is_bounded() {
        let p = RetryPolicy::default();
        for attempt in 1..=10 {
            assert!(p.backoff_for(attempt) <= p.max_backoff);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stops_on_non_retryable() {
        let mut calls = 0;
        let result: Result<()> = with_retry("test", RetryPolicy::default(), |_| {
            calls += 1;
            async { Err(AgentError::other("fatal")) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
}
