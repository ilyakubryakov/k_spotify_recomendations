//! OpenAI-compatible backend — OpenAI itself, and anything that speaks the
//! same `/v1/chat/completions` shape (Ollama, vLLM, LM Studio, llama.cpp).
//!
//! Structured output uses `response_format.json_schema` with `strict: true`,
//! which requires exactly the constraints our schema already satisfies:
//! `additionalProperties: false` on every object and every property listed in
//! `required`.
//!
//! Two differences between OpenAI proper and local servers are handled here:
//!
//! * **Token parameter.** OpenAI's current API takes `max_completion_tokens`;
//!   the compatible servers still take `max_tokens`. Sending the wrong one is
//!   a 400 on OpenAI and silently ignored locally, so it is chosen by provider.
//! * **Auth.** A local Ollama normally has no key, so the `Authorization`
//!   header is omitted rather than sent empty.

use crate::config::BackendConfig;
use crate::error::{AgentError, Result};
use crate::llm::{Completion, CurationRequest, Listener, Provider, StreamEvent};
use crate::util::retry::{RetryPolicy, parse_retry_after, with_retry};
use crate::util::secret::Secret;
use futures_util::StreamExt;
use serde_json::{Value, json};

pub struct OpenAiBackend {
    http: reqwest::Client,
    cfg: BackendConfig,
    api_key: Option<Secret>,
    policy: RetryPolicy,
}

impl OpenAiBackend {
    pub fn new(cfg: BackendConfig, http: reqwest::Client) -> Result<Self> {
        let api_key = cfg.resolve_api_key()?;
        if let Some(key) = &api_key {
            tracing::debug!(
                provider = cfg.provider.as_str(),
                model = %cfg.model,
                key = %key.hint(),
                "openai-compatible backend configured"
            );
        } else {
            tracing::debug!(
                provider = cfg.provider.as_str(),
                model = %cfg.model,
                "openai-compatible backend configured without a key"
            );
        }
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

    pub fn provider(&self) -> Provider {
        self.cfg.provider
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    fn service(&self) -> &'static str {
        match self.cfg.provider {
            Provider::Ollama => "ollama",
            _ => "openai",
        }
    }

    fn build_body(&self, request: &CurationRequest<'_>) -> Value {
        let mut body = json!({
            "model": self.cfg.model,
            "messages": [
                {"role": "system", "content": request.system_combined()},
                {"role": "user", "content": request.user},
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "curation",
                    "strict": true,
                    "schema": request.schema,
                }
            },
            "stream": self.cfg.stream,
        });

        if let Some(map) = body.as_object_mut() {
            // See the module note: the parameter name differs by server.
            let token_field = match self.cfg.provider {
                Provider::Ollama => "max_tokens",
                _ => "max_completion_tokens",
            };
            map.insert(token_field.into(), json!(self.cfg.max_tokens));

            if self.cfg.stream {
                // Without this, streamed responses carry no usage totals at all.
                map.insert("stream_options".into(), json!({"include_usage": true}));
            }
        }
        body
    }

    pub async fn complete(
        &self,
        request: &CurationRequest<'_>,
        listener: Option<Listener>,
    ) -> Result<Completion> {
        let url = format!("{}/v1/chat/completions", self.cfg.base_url());
        let body = self.build_body(request);
        let service = self.service();
        let streaming = self.cfg.stream;
        let provider = self.cfg.provider;

        let completion = with_retry(service, self.policy, |_attempt| {
            let http = self.http.clone();
            let url = url.clone();
            let body = body.clone();
            let listener = listener.clone();
            let api_key = self.api_key.clone();

            async move {
                let mut req = http
                    .post(&url)
                    .header(reqwest::header::CONTENT_TYPE, "application/json");
                if let Some(key) = &api_key {
                    req = req.bearer_auth(key.expose());
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
                    return Err(map_error(service, status.as_u16(), &text, retry_after));
                }

                if streaming {
                    parse_stream(provider, response, listener).await
                } else {
                    let value: Value = response.json().await?;
                    parse_response(provider, &value)
                }
            }
        })
        .await?;

        if completion.stop_reason == "length" {
            return Err(AgentError::ModelProtocol(format!(
                "{} truncated the response at the {} token cap; raise max_tokens or lower the playlist size",
                provider.as_str(),
                self.cfg.max_tokens
            )));
        }
        Ok(completion)
    }
}

fn map_error(
    service: &'static str,
    status: u16,
    body: &str,
    retry_after: Option<std::time::Duration>,
) -> AgentError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message").or(Some(e)))
                .and_then(|m| m.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| format!("HTTP {status}"));

    // `service` is a &'static str chosen from a closed set, so this match keeps
    // the error type's &'static str contract without leaking.
    let svc: &'static str = if service == "ollama" {
        "ollama"
    } else {
        "openai"
    };

    match status {
        429 => AgentError::RateLimited {
            service: svc,
            retry_after,
        },
        500..=599 => AgentError::Transient {
            service: svc,
            status,
            message,
        },
        401 | 403 => AgentError::MissingCredential {
            name: "llm api key",
            hint: format!("{message} — check the key for the {svc} backend"),
        },
        _ => AgentError::Api {
            service: svc,
            status,
            message,
        },
    }
}

fn parse_response(provider: Provider, value: &Value) -> Result<Completion> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or_else(|| AgentError::ModelProtocol("response contained no choices".into()))?;

    let message = choice.get("message");
    let text = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // Reasoning models expose a summary under a non-standard key; surface it
    // when present rather than pretending there is none.
    let thinking = message
        .and_then(|m| m.get("reasoning").or_else(|| m.get("reasoning_content")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    if let Some(refusal) = message
        .and_then(|m| m.get("refusal"))
        .and_then(Value::as_str)
        && !refusal.is_empty()
    {
        return Err(AgentError::ModelRefusal {
            category: "refusal".into(),
            explanation: refusal.to_string(),
        });
    }

    Ok(Completion {
        text,
        thinking,
        model: value
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        provider,
        stop_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop")
            .to_string(),
        input_tokens: usage(value, "prompt_tokens"),
        output_tokens: usage(value, "completion_tokens"),
    })
}

fn usage(value: &Value, field: &str) -> u64 {
    value
        .get("usage")
        .and_then(|u| u.get(field))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

async fn parse_stream(
    provider: Provider,
    response: reqwest::Response,
    listener: Option<Listener>,
) -> Result<Completion> {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut model = String::new();
    let mut stop_reason = "stop".to_string();
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
            // OpenAI terminates the stream with a literal sentinel, not an event.
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(event): std::result::Result<Value, _> = serde_json::from_str(payload) else {
                tracing::debug!(payload = %payload, "skipping unparsable SSE frame");
                continue;
            };

            if let Some(m) = event.get("model").and_then(Value::as_str) {
                model = m.to_string();
            }
            if event.get("usage").is_some() {
                input_tokens = usage(&event, "prompt_tokens").max(input_tokens);
                output_tokens = usage(&event, "completion_tokens").max(output_tokens);
            }

            let Some(choice) = event
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
            else {
                continue;
            };

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                stop_reason = reason.to_string();
            }

            let delta = choice.get("delta");
            if let Some(content) = delta.and_then(|d| d.get("content")).and_then(Value::as_str) {
                text.push_str(content);
                if let Some(l) = &listener {
                    l(StreamEvent::Text(content.to_string()));
                }
            }
            if let Some(reasoning) = delta
                .and_then(|d| d.get("reasoning").or_else(|| d.get("reasoning_content")))
                .and_then(Value::as_str)
            {
                thinking.push_str(reasoning);
                if let Some(l) = &listener {
                    l(StreamEvent::Thinking(reasoning.to_string()));
                }
            }
        }
    }

    Ok(Completion {
        text,
        thinking,
        model,
        provider,
        stop_reason,
        input_tokens,
        output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(provider: Provider) -> OpenAiBackend {
        OpenAiBackend {
            http: reqwest::Client::new(),
            cfg: BackendConfig {
                provider,
                model: "m".into(),
                ..Default::default()
            },
            api_key: Some(Secret::new("k")),
            policy: RetryPolicy::default(),
        }
    }

    fn request() -> CurationRequest<'static> {
        CurationRequest {
            system_stable: "S",
            system_volatile: "V",
            user: "U",
            schema: crate::llm::schema::curation_schema(),
        }
    }

    #[test]
    fn openai_uses_max_completion_tokens_and_ollama_uses_max_tokens() {
        let body = backend(Provider::Openai).build_body(&request());
        assert!(body.get("max_completion_tokens").is_some());
        assert!(body.get("max_tokens").is_none());

        let body = backend(Provider::Ollama).build_body(&request());
        assert!(body.get("max_tokens").is_some());
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn schema_is_sent_strict() {
        let body = backend(Provider::Openai).build_body(&request());
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn both_system_halves_reach_the_single_system_message() {
        let body = backend(Provider::Openai).build_body(&request());
        let system = body["messages"][0]["content"].as_str().unwrap_or_default();
        assert!(system.contains('S') && system.contains('V'));
    }

    #[test]
    fn refusal_is_surfaced_as_an_error() {
        let value = json!({
            "model": "m",
            "choices": [{"finish_reason": "stop", "message": {"refusal": "cannot help"}}]
        });
        let error = parse_response(Provider::Openai, &value).expect_err("refusal");
        assert!(error.to_string().contains("cannot help"), "{error}");
    }

    #[test]
    fn parses_content_and_usage() {
        let value = json!({
            "model": "gpt-x",
            "choices": [{"finish_reason": "stop", "message": {"content": "{\"a\":1}"}}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 22}
        });
        let c = parse_response(Provider::Openai, &value).expect("parses");
        assert_eq!(c.text, "{\"a\":1}");
        assert_eq!(c.input_tokens, 11);
        assert_eq!(c.output_tokens, 22);
    }
}
