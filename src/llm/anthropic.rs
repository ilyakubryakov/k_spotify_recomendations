//! Anthropic backend — Messages API (`POST /v1/messages`).
//!
//! Rust has no official Anthropic SDK, so this is a deliberate raw-HTTP client
//! against the documented wire format. Notes that matter for correctness on
//! current models:
//!
//! * **No `temperature` / `top_p` / `top_k`.** Those parameters were removed
//!   on the Opus 5 / Sonnet 5 generation and are rejected with a 400. Output
//!   shape is controlled with `output_config` instead.
//! * **No `budget_tokens`.** Thinking is configured as
//!   `{"type": "adaptive"}`; the fixed-budget form is a 400 on these models.
//! * **Structured output** uses `output_config.format = {type: "json_schema",
//!   schema: …}`, which guarantees the first text block parses as JSON
//!   matching the schema. That removes the whole class of "the model wrapped
//!   the JSON in a code fence" failures.
//! * **`stop_reason` can be `"refusal"`** (HTTP 200). It is checked before the
//!   content is read.
//! * The system prompt is sent as content blocks with `cache_control` on the
//!   stable prefix, so repeated runs pay for the persona once.

use crate::config::{BackendConfig, ThinkingMode};
use crate::error::{AgentError, Result};
use crate::llm::{Completion, CurationRequest, Listener, Provider, StreamEvent};
use crate::util::retry::{RetryPolicy, parse_retry_after, with_retry};
use crate::util::secret::Secret;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

const SERVICE: &str = "anthropic";

/// Beta flag required for the `fallbacks` array form.
const FALLBACK_BETA: &str = "server-side-fallback-2026-06-01";

pub struct AnthropicBackend {
    http: reqwest::Client,
    cfg: BackendConfig,
    api_key: Secret,
    policy: RetryPolicy,
}

impl AnthropicBackend {
    pub fn new(cfg: BackendConfig, http: reqwest::Client) -> Result<Self> {
        let api_key = cfg
            .resolve_api_key()?
            .ok_or(AgentError::MissingCredential {
                name: "anthropic api key",
                hint: "export ANTHROPIC_API_KEY=… or set claude.api_key".into(),
            })?;
        tracing::debug!(model = %cfg.model, key = %api_key.hint(), "anthropic backend configured");
        let policy = RetryPolicy {
            max_attempts: cfg.max_retries.max(1),
            // Anthropic 429s are usually short; 5xx/529 benefit from a longer
            // ceiling than the Spotify default.
            max_backoff: std::time::Duration::from_secs(60),
            max_total_delay: std::time::Duration::from_secs(300),
            ..Default::default()
        };
        Ok(Self {
            http,
            cfg,
            api_key,
            policy,
        })
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// Run one structured turn.
    pub async fn complete(
        &self,
        request: &CurationRequest<'_>,
        listener: Option<Listener>,
    ) -> Result<Completion> {
        let body = self.build_body(
            request.system_stable,
            request.system_volatile,
            request.user,
            Some(request.schema.clone()),
        );
        let completion = self.execute(body, listener).await?;

        match completion.stop_reason.as_str() {
            "max_tokens" => {
                return Err(AgentError::ModelProtocol(format!(
                    "response hit the {} token cap and was truncated; raise claude.max_tokens or lower the playlist size",
                    self.cfg.max_tokens
                )));
            }
            "end_turn" | "stop_sequence" => {}
            other => tracing::debug!(stop_reason = other, "unusual stop reason"),
        }

        Ok(completion)
    }

    fn build_body(
        &self,
        system_stable: &str,
        system_volatile: &str,
        user: &str,
        schema: Option<Value>,
    ) -> Value {
        let mut system_blocks = vec![json!({
            "type": "text",
            "text": system_stable,
            // Everything before this breakpoint is reused across runs.
            "cache_control": {"type": "ephemeral"}
        })];
        if !system_volatile.trim().is_empty() {
            system_blocks.push(json!({"type": "text", "text": system_volatile}));
        }

        let mut output_config = json!({ "effort": self.cfg.effort.as_str() });
        if let (Some(schema), Some(map)) = (schema, output_config.as_object_mut()) {
            map.insert(
                "format".into(),
                json!({"type": "json_schema", "schema": schema}),
            );
        }

        let mut body = json!({
            "model": self.cfg.model,
            "max_tokens": self.cfg.max_tokens,
            "system": system_blocks,
            "messages": [{"role": "user", "content": user}],
            "output_config": output_config,
            "stream": self.cfg.stream,
        });

        if let Some(map) = body.as_object_mut() {
            match self.cfg.thinking {
                ThinkingMode::Adaptive => {
                    map.insert(
                        "thinking".into(),
                        json!({
                            "type": "adaptive",
                            "display": self.cfg.thinking_display.as_str()
                        }),
                    );
                }
                ThinkingMode::Disabled => {
                    map.insert("thinking".into(), json!({"type": "disabled"}));
                }
            }

            if !self.cfg.fallback_models.is_empty() {
                let fallbacks: Vec<Value> = self
                    .cfg
                    .fallback_models
                    .iter()
                    .map(|m| json!({"model": m}))
                    .collect();
                map.insert("fallbacks".into(), Value::Array(fallbacks));
            }
        }

        body
    }

    async fn execute(&self, body: Value, listener: Option<Listener>) -> Result<Completion> {
        let url = format!("{}/v1/messages", self.cfg.base_url());
        let streaming = self.cfg.stream;

        with_retry(SERVICE, self.policy, |_attempt| {
            let http = self.http.clone();
            let url = url.clone();
            let body = body.clone();
            let listener = listener.clone();
            let api_key = self.api_key.clone();
            let version = self.cfg.anthropic_version.clone();
            let betas = (!self.cfg.fallback_models.is_empty()).then(|| FALLBACK_BETA.to_string());

            async move {
                let mut req = http
                    .post(&url)
                    .header("x-api-key", api_key.expose())
                    .header("anthropic-version", version)
                    .header(reqwest::header::CONTENT_TYPE, "application/json");
                if let Some(beta) = betas {
                    req = req.header("anthropic-beta", beta);
                }

                let response = req.json(&body).send().await?;
                let status = response.status();

                if !status.is_success() {
                    let retry_after = parse_retry_after(
                        response
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok()),
                    );
                    let text = response.text().await.unwrap_or_default();
                    return Err(map_error(status.as_u16(), &text, retry_after));
                }

                if streaming {
                    parse_stream(response, listener).await
                } else {
                    let value: Value = response.json().await?;
                    let completion = parse_message(&value)?;
                    // Without a stream there are no incremental events, so the
                    // reasoning summary is replayed in one go for the UI.
                    if let (Some(l), false) = (&listener, completion.thinking.is_empty()) {
                        l(StreamEvent::Thinking(completion.thinking.clone()));
                    }
                    Ok(completion)
                }
            }
        })
        .await
    }
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Deserialize)]
struct ErrorBody {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    message: String,
}

fn map_error(status: u16, body: &str, retry_after: Option<std::time::Duration>) -> AgentError {
    let parsed = serde_json::from_str::<ErrorEnvelope>(body).ok();
    let message = parsed
        .as_ref()
        .map(|e| {
            if e.error.message.is_empty() {
                e.error.kind.clone()
            } else {
                e.error.message.clone()
            }
        })
        .unwrap_or_else(|| format!("HTTP {status}"));

    match status {
        429 => AgentError::RateLimited {
            service: SERVICE,
            retry_after,
        },
        // 529 is Anthropic's "overloaded", inside the retryable 5xx range.
        500..=599 => AgentError::Transient {
            service: SERVICE,
            status,
            message,
        },
        401 | 403 => AgentError::MissingCredential {
            name: "anthropic api key",
            hint: format!("{message} — check ANTHROPIC_API_KEY"),
        },
        _ => AgentError::Api {
            service: SERVICE,
            status,
            message,
        },
    }
}

/// Non-streaming response.
fn parse_message(value: &Value) -> Result<Completion> {
    let stop_reason = value
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn")
        .to_string();

    // Check refusal *before* reading content: on a refusal the content is not
    // the answer, and `stop_details` is only populated in this case.
    if stop_reason == "refusal" {
        let details = value.get("stop_details");
        return Err(AgentError::ModelRefusal {
            category: details
                .and_then(|d| d.get("category"))
                .and_then(Value::as_str)
                .unwrap_or("unspecified")
                .to_string(),
            explanation: details
                .and_then(|d| d.get("explanation"))
                .and_then(Value::as_str)
                .unwrap_or("no explanation provided")
                .to_string(),
        });
    }

    let mut text = String::new();
    let mut thinking = String::new();
    if let Some(blocks) = value.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => text.push_str(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                Some("thinking") => thinking.push_str(
                    block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                ),
                _ => {}
            }
        }
    }

    let input_tokens = usage_field(value, "input_tokens");
    let output_tokens = usage_field(value, "output_tokens");

    Ok(Completion {
        text,
        thinking,
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        provider: Provider::Anthropic,
        stop_reason,
        input_tokens,
        output_tokens,
    })
}

fn usage_field(value: &Value, field: &str) -> u64 {
    value
        .get("usage")
        .and_then(|u| u.get(field))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Server-sent events.
///
/// The framing is `event: <name>\n` + `data: <json>\n\n`. We only need the
/// `data` lines; the event name is duplicated inside the JSON as `type`.
async fn parse_stream(
    response: reqwest::Response,
    listener: Option<Listener>,
) -> Result<Completion> {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut model = String::new();
    let mut stop_reason = "end_turn".to_string();
    let mut stop_details: Option<Value> = None;
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;

    let mut buffer = String::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete lines only; a chunk boundary can split a line.
        while let Some(idx) = buffer.find('\n') {
            let line = buffer[..idx].trim_end_matches('\r').to_string();
            buffer.drain(..=idx);

            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(event): std::result::Result<Value, _> = serde_json::from_str(payload) else {
                tracing::debug!(payload = %payload, "skipping unparsable SSE frame");
                continue;
            };

            match event.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    if let Some(message) = event.get("message") {
                        model = message
                            .get("model")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        input_tokens = usage_field(message, "input_tokens");
                    }
                }
                Some("content_block_delta") => {
                    let delta = event.get("delta");
                    match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(t) =
                                delta.and_then(|d| d.get("text")).and_then(Value::as_str)
                            {
                                text.push_str(t);
                                if let Some(l) = &listener {
                                    l(StreamEvent::Text(t.to_string()));
                                }
                            }
                        }
                        Some("thinking_delta") => {
                            if let Some(t) = delta
                                .and_then(|d| d.get("thinking"))
                                .and_then(Value::as_str)
                            {
                                thinking.push_str(t);
                                if let Some(l) = &listener {
                                    l(StreamEvent::Thinking(t.to_string()));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Some("content_block_start") => {
                    // A `fallback` block means a refusal was rescued by a
                    // fallback model mid-turn.
                    if let Some(block) = event.get("content_block") {
                        if block.get("type").and_then(Value::as_str) == Some("fallback") {
                            let to = block
                                .get("to")
                                .and_then(|t| t.get("model"))
                                .and_then(Value::as_str)
                                .unwrap_or("fallback")
                                .to_string();
                            if let Some(l) = &listener {
                                l(StreamEvent::FellBackTo(to.clone()));
                            }
                            tracing::warn!(model = %to, "request fell back after a refusal");
                        }
                    }
                }
                Some("message_delta") => {
                    if let Some(reason) = event
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(Value::as_str)
                    {
                        stop_reason = reason.to_string();
                    }
                    if let Some(details) = event.get("delta").and_then(|d| d.get("stop_details")) {
                        if !details.is_null() {
                            stop_details = Some(details.clone());
                        }
                    }
                    output_tokens = usage_field(&event, "output_tokens").max(output_tokens);
                }
                Some("error") => {
                    let err = event.get("error");
                    let kind = err
                        .and_then(|e| e.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("error");
                    let message = err
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("stream error")
                        .to_string();
                    // `overloaded_error` mid-stream is retryable; the rest are not.
                    return Err(if kind == "overloaded_error" {
                        AgentError::Transient {
                            service: SERVICE,
                            status: 529,
                            message,
                        }
                    } else {
                        AgentError::ModelProtocol(format!("{kind}: {message}"))
                    });
                }
                _ => {}
            }
        }
    }

    if stop_reason == "refusal" {
        return Err(AgentError::ModelRefusal {
            category: stop_details
                .as_ref()
                .and_then(|d| d.get("category"))
                .and_then(Value::as_str)
                .unwrap_or("unspecified")
                .to_string(),
            explanation: stop_details
                .as_ref()
                .and_then(|d| d.get("explanation"))
                .and_then(Value::as_str)
                .unwrap_or("no explanation provided")
                .to_string(),
        });
    }

    Ok(Completion {
        text,
        thinking,
        model,
        provider: Provider::Anthropic,
        stop_reason,
        input_tokens,
        output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_is_an_error_not_content() {
        let value = json!({
            "stop_reason": "refusal",
            "stop_details": {"type": "refusal", "category": "cyber", "explanation": "nope"},
            "content": [{"type": "text", "text": "ignored"}]
        });
        match parse_message(&value) {
            Err(AgentError::ModelRefusal { category, .. }) => assert_eq!(category, "cyber"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn concatenates_text_blocks_and_usage() {
        let value = json!({
            "stop_reason": "end_turn",
            "model": "claude-opus-5",
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "{\"a\":"},
                {"type": "text", "text": "1}"}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 4}
        });
        let c = parse_message(&value).expect("parses");
        assert_eq!(c.text, "{\"a\":1}");
        assert_eq!(c.thinking, "hmm");
        assert_eq!(c.input_tokens, 10);
        assert_eq!(c.output_tokens, 4);
    }

    #[test]
    fn body_omits_removed_sampling_params() {
        let cfg = BackendConfig::default();
        let client = AnthropicBackend {
            http: reqwest::Client::new(),
            cfg: cfg.clone(),
            api_key: Secret::new("k"),
            policy: RetryPolicy::default(),
        };
        let body = client.build_body("stable", "volatile", "hi", Some(json!({"type": "object"})));
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert!(body["thinking"].get("budget_tokens").is_none());
        // Stable prefix is cacheable, volatile suffix is not.
        assert!(body["system"][0].get("cache_control").is_some());
        assert!(body["system"][1].get("cache_control").is_none());
    }
}
