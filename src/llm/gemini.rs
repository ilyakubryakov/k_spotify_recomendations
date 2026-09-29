//! Google Gemini backend — `generateContent`.
//!
//! Gemini's structured output takes an **OpenAPI-subset** schema in
//! `generationConfig.responseSchema`. That dialect rejects
//! `additionalProperties`, which our canonical schema requires for the other
//! providers, so the schema is translated by [`crate::llm::schema::for_gemini`]
//! before it goes on the wire.
//!
//! Auth is the `x-goog-api-key` header rather than a bearer token, and the
//! system prompt is a dedicated `systemInstruction` field rather than a message.

use crate::config::BackendConfig;
use crate::error::{AgentError, Result};
use crate::llm::{Completion, CurationRequest, Listener, Provider, StreamEvent};
use crate::util::retry::{RetryPolicy, parse_retry_after, with_retry};
use crate::util::secret::Secret;
use futures_util::StreamExt;
use serde_json::{Value, json};

const SERVICE: &str = "gemini";

pub struct GeminiBackend {
    http: reqwest::Client,
    cfg: BackendConfig,
    api_key: Secret,
    policy: RetryPolicy,
}

impl GeminiBackend {
    pub fn new(cfg: BackendConfig, http: reqwest::Client) -> Result<Self> {
        let api_key = cfg
            .resolve_api_key()?
            .ok_or(AgentError::MissingCredential {
                name: "gemini api key",
                hint: "export GEMINI_API_KEY=… or set api_key on the gemini backend".into(),
            })?;
        tracing::debug!(model = %cfg.model, key = %api_key.hint(), "gemini backend configured");
        let policy = RetryPolicy {
            max_attempts: cfg.max_retries.max(1),
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

    fn build_body(&self, request: &CurationRequest<'_>) -> Value {
        json!({
            "systemInstruction": {
                "parts": [{"text": request.system_combined()}]
            },
            "contents": [{
                "role": "user",
                "parts": [{"text": request.user}]
            }],
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseSchema": crate::llm::schema::for_gemini(&request.schema),
                "maxOutputTokens": self.cfg.max_tokens,
            }
        })
    }

    pub async fn complete(
        &self,
        request: &CurationRequest<'_>,
        listener: Option<Listener>,
    ) -> Result<Completion> {
        let streaming = self.cfg.stream;
        let method = if streaming {
            // `alt=sse` is what turns the streaming endpoint into Server-Sent
            // Events; without it the response is a JSON array, not a stream.
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        let url = format!(
            "{}/v1beta/models/{}:{method}",
            self.cfg.base_url(),
            self.cfg.model
        );
        let body = self.build_body(request);
        let model = self.cfg.model.clone();

        let completion = with_retry(SERVICE, self.policy, |_attempt| {
            let http = self.http.clone();
            let url = url.clone();
            let body = body.clone();
            let listener = listener.clone();
            let api_key = self.api_key.clone();
            let model = model.clone();

            async move {
                let response = http
                    .post(&url)
                    // Header rather than a `?key=` query parameter: a key in a
                    // URL ends up in proxy and server logs.
                    .header("x-goog-api-key", api_key.expose())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .json(&body)
                    .send()
                    .await?;

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
                    parse_stream(response, listener, &model).await
                } else {
                    let value: Value = response.json().await?;
                    parse_response(&value, &model)
                }
            }
        })
        .await?;

        if completion.stop_reason == "MAX_TOKENS" {
            return Err(AgentError::ModelProtocol(format!(
                "gemini truncated the response at the {} token cap; raise max_tokens or lower the playlist size",
                self.cfg.max_tokens
            )));
        }
        Ok(completion)
    }
}

fn map_error(status: u16, body: &str, retry_after: Option<std::time::Duration>) -> AgentError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("HTTP {status}"));

    match status {
        429 => AgentError::RateLimited {
            service: SERVICE,
            retry_after,
        },
        500..=599 => AgentError::Transient {
            service: SERVICE,
            status,
            message,
        },
        401 | 403 => AgentError::MissingCredential {
            name: "gemini api key",
            hint: format!("{message} — check GEMINI_API_KEY"),
        },
        _ => AgentError::Api {
            service: SERVICE,
            status,
            message,
        },
    }
}

/// Pull text out of a `candidates[0].content.parts[]` array.
fn candidate_text(candidate: &Value) -> (String, String) {
    let mut text = String::new();
    let mut thinking = String::new();
    if let Some(parts) = candidate
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
    {
        for part in parts {
            let Some(chunk) = part.get("text").and_then(Value::as_str) else {
                continue;
            };
            // Gemini marks reasoning parts with `thought: true`; they are not
            // part of the answer and must not reach the JSON parser.
            if part
                .get("thought")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                thinking.push_str(chunk);
            } else {
                text.push_str(chunk);
            }
        }
    }
    (text, thinking)
}

fn parse_response(value: &Value, requested_model: &str) -> Result<Completion> {
    // A prompt blocked before generation has no candidates at all.
    if let Some(reason) = value
        .get("promptFeedback")
        .and_then(|f| f.get("blockReason"))
        .and_then(Value::as_str)
    {
        return Err(AgentError::ModelRefusal {
            category: reason.to_string(),
            explanation: "gemini blocked the prompt before generating".into(),
        });
    }

    let candidate = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or_else(|| AgentError::ModelProtocol("gemini returned no candidates".into()))?;

    let finish = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .unwrap_or("STOP")
        .to_string();

    if matches!(
        finish.as_str(),
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "RECITATION"
    ) {
        return Err(AgentError::ModelRefusal {
            category: finish,
            explanation: "gemini stopped generation on a content policy".into(),
        });
    }

    let (text, thinking) = candidate_text(candidate);

    Ok(Completion {
        text,
        thinking,
        model: value
            .get("modelVersion")
            .and_then(Value::as_str)
            .unwrap_or(requested_model)
            .to_string(),
        provider: Provider::Gemini,
        stop_reason: finish,
        input_tokens: usage(value, "promptTokenCount"),
        output_tokens: usage(value, "candidatesTokenCount"),
    })
}

fn usage(value: &Value, field: &str) -> u64 {
    value
        .get("usageMetadata")
        .and_then(|u| u.get(field))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

async fn parse_stream(
    response: reqwest::Response,
    listener: Option<Listener>,
    requested_model: &str,
) -> Result<Completion> {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut model = requested_model.to_string();
    let mut stop_reason = "STOP".to_string();
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;

    let mut buffer = String::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk?));

        while let Some(idx) = buffer.find('\n') {
            let line = buffer[..idx].trim_end_matches('\r').to_string();
            buffer.drain(..=idx);

            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            let Ok(event): std::result::Result<Value, _> = serde_json::from_str(payload) else {
                tracing::debug!(payload = %payload, "skipping unparsable gemini SSE frame");
                continue;
            };

            if let Some(m) = event.get("modelVersion").and_then(Value::as_str) {
                model = m.to_string();
            }
            input_tokens = usage(&event, "promptTokenCount").max(input_tokens);
            output_tokens = usage(&event, "candidatesTokenCount").max(output_tokens);

            let Some(candidate) = event
                .get("candidates")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
            else {
                continue;
            };

            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                stop_reason = reason.to_string();
            }

            let (chunk_text, chunk_thinking) = candidate_text(candidate);
            if !chunk_text.is_empty() {
                text.push_str(&chunk_text);
                if let Some(l) = &listener {
                    l(StreamEvent::Text(chunk_text));
                }
            }
            if !chunk_thinking.is_empty() {
                thinking.push_str(&chunk_thinking);
                if let Some(l) = &listener {
                    l(StreamEvent::Thinking(chunk_thinking));
                }
            }
        }
    }

    if matches!(
        stop_reason.as_str(),
        "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "RECITATION"
    ) {
        return Err(AgentError::ModelRefusal {
            category: stop_reason,
            explanation: "gemini stopped generation on a content policy".into(),
        });
    }

    Ok(Completion {
        text,
        thinking,
        model,
        provider: Provider::Gemini,
        stop_reason,
        input_tokens,
        output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thought_parts_do_not_pollute_the_json() {
        let candidate = json!({
            "content": {"parts": [
                {"text": "considering options", "thought": true},
                {"text": "{\"a\":"},
                {"text": "1}"}
            ]}
        });
        let (text, thinking) = candidate_text(&candidate);
        assert_eq!(text, "{\"a\":1}");
        assert_eq!(thinking, "considering options");
    }

    #[test]
    fn a_blocked_prompt_is_a_refusal() {
        let value = json!({"promptFeedback": {"blockReason": "SAFETY"}});
        let error = parse_response(&value, "m").expect_err("blocked");
        assert!(matches!(error, AgentError::ModelRefusal { .. }));
    }

    #[test]
    fn safety_finish_reason_is_a_refusal() {
        let value = json!({"candidates": [{"finishReason": "SAFETY", "content": {"parts": []}}]});
        assert!(matches!(
            parse_response(&value, "m"),
            Err(AgentError::ModelRefusal { .. })
        ));
    }

    #[test]
    fn parses_text_and_usage() {
        let value = json!({
            "modelVersion": "gemini-x",
            "candidates": [{"finishReason": "STOP", "content": {"parts": [{"text": "{}"}]}}],
            "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 9}
        });
        let c = parse_response(&value, "m").expect("parses");
        assert_eq!(c.text, "{}");
        assert_eq!(c.model, "gemini-x");
        assert_eq!(c.input_tokens, 7);
        assert_eq!(c.output_tokens, 9);
    }
}
