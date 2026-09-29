//! Provider-agnostic LLM layer.
//!
//! The agent's curation step is one structured-JSON turn, which every current
//! frontier API can serve. This module defines that turn once and implements
//! it per provider, so a run can fall back down an ordered chain of models
//! instead of failing when one provider is down, rate-limited, or refuses.
//!
//! ```text
//!   [claude]              primary backend  (provider = anthropic by default)
//!   [[llm.fallbacks]]     tried in order when the primary cannot answer
//! ```
//!
//! Provider differences that actually matter here:
//!
//! | provider  | structured output                          | auth                |
//! |-----------|--------------------------------------------|---------------------|
//! | anthropic | `output_config.format` (`json_schema`)     | `x-api-key`         |
//! | openai    | `response_format.json_schema` (`strict`)   | `Authorization`     |
//! | ollama    | same as openai (compat endpoint)           | usually none        |
//! | gemini    | `generationConfig.responseSchema` (OpenAPI)| `x-goog-api-key`    |
//!
//! Gemini's schema dialect is an OpenAPI subset that rejects
//! `additionalProperties`, so [`schema::for_gemini`] translates it.

pub mod anthropic;
pub mod chain;
pub mod gemini;
pub mod openai;
pub mod prompt;
pub mod schema;

use crate::error::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// Which API dialect a backend speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Anthropic,
    /// OpenAI's own API.
    Openai,
    /// Any OpenAI-compatible endpoint that is not OpenAI — Ollama, vLLM,
    /// LM Studio, llama.cpp's server. Differs from `Openai` only in the token
    /// parameter name and in not requiring a key.
    Ollama,
    Gemini,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::Ollama => "ollama",
            Self::Gemini => "gemini",
        }
    }

    /// Default endpoint for the provider.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com",
            Self::Openai => "https://api.openai.com",
            Self::Ollama => "http://127.0.0.1:11434",
            Self::Gemini => "https://generativelanguage.googleapis.com",
        }
    }

    /// Environment variable consulted when the config does not name one.
    pub fn default_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Openai => "OPENAI_API_KEY",
            Self::Ollama => "OLLAMA_API_KEY",
            Self::Gemini => "GEMINI_API_KEY",
        }
    }

    /// Whether a missing key is fatal. A local Ollama normally has none.
    pub fn requires_key(self) -> bool {
        !matches!(self, Self::Ollama)
    }
}

/// Incremental signal from a streaming turn.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A chunk of the model's reasoning summary, when the provider exposes one.
    Thinking(String),
    /// A chunk of the answer. Under a JSON schema this is raw JSON, so it is a
    /// progress heartbeat rather than displayable prose.
    Text(String),
    /// The turn was rerouted to another model *inside* one provider's API
    /// (Anthropic's server-side refusal fallback).
    FellBackTo(String),
}

pub type Listener = Arc<dyn Fn(StreamEvent) + Send + Sync>;

/// What a completed turn produced.
#[derive(Debug, Clone)]
pub struct Completion {
    /// Concatenated text. Under a JSON schema this is the JSON document.
    pub text: String,
    /// Reasoning summary, where the provider returns one.
    pub thinking: String,
    /// Model id as reported by the provider (may differ from the requested one).
    pub model: String,
    pub provider: Provider,
    pub stop_reason: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One structured-output turn, expressed provider-neutrally.
pub struct CurationRequest<'a> {
    /// Stable prefix — cacheable where the provider supports prompt caching.
    pub system_stable: &'a str,
    /// Per-run system context.
    pub system_volatile: &'a str,
    pub user: &'a str,
    /// JSON Schema the response must satisfy.
    pub schema: Value,
}

impl CurationRequest<'_> {
    /// Providers with a single system slot get the two halves concatenated.
    pub fn system_combined(&self) -> String {
        if self.system_volatile.trim().is_empty() {
            self.system_stable.to_string()
        } else {
            format!("{}\n\n{}", self.system_stable, self.system_volatile)
        }
    }
}

/// A configured, ready-to-call backend.
pub enum Backend {
    Anthropic(anthropic::AnthropicBackend),
    OpenAi(openai::OpenAiBackend),
    Gemini(gemini::GeminiBackend),
}

impl Backend {
    pub fn provider(&self) -> Provider {
        match self {
            Self::Anthropic(_) => Provider::Anthropic,
            Self::OpenAi(b) => b.provider(),
            Self::Gemini(_) => Provider::Gemini,
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Self::Anthropic(b) => b.model(),
            Self::OpenAi(b) => b.model(),
            Self::Gemini(b) => b.model(),
        }
    }

    /// Human label for logs and the TUI, e.g. `anthropic/claude-opus-5`.
    pub fn label(&self) -> String {
        format!("{}/{}", self.provider().as_str(), self.model())
    }

    pub async fn complete(
        &self,
        request: &CurationRequest<'_>,
        listener: Option<Listener>,
    ) -> Result<Completion> {
        match self {
            Self::Anthropic(b) => b.complete(request, listener).await,
            Self::OpenAi(b) => b.complete(request, listener).await,
            Self::Gemini(b) => b.complete(request, listener).await,
        }
    }
}

/// Parse a structured completion into a JSON value, with a diagnosable error.
pub fn parse_structured(completion: &Completion) -> Result<Value> {
    let trimmed = completion.text.trim();
    // Some OpenAI-compatible servers (notably older Ollama builds) still wrap
    // JSON in a markdown fence despite the schema request.
    let cleaned = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|rest| {
            rest.trim_start_matches('\n')
                .trim_end_matches('`')
                .trim_end()
        })
        .unwrap_or(trimmed);

    serde_json::from_str(cleaned).map_err(|e| {
        crate::error::AgentError::ModelProtocol(format!(
            "{} returned output that is not valid JSON ({e}); first 200 chars: {}",
            completion.provider.as_str(),
            cleaned.chars().take(200).collect::<String>()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completion(text: &str) -> Completion {
        Completion {
            text: text.into(),
            thinking: String::new(),
            model: "m".into(),
            provider: Provider::Ollama,
            stop_reason: "stop".into(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    #[test]
    fn strips_a_markdown_fence_from_json() {
        let value = parse_structured(&completion("```json\n{\"a\":1}\n```")).expect("parses");
        assert_eq!(value["a"], 1);
    }

    #[test]
    fn parses_bare_json() {
        let value = parse_structured(&completion("  {\"a\":2}  ")).expect("parses");
        assert_eq!(value["a"], 2);
    }

    #[test]
    fn reports_the_provider_on_garbage() {
        let error = parse_structured(&completion("not json")).expect_err("should fail");
        assert!(error.to_string().contains("ollama"), "{error}");
    }

    #[test]
    fn system_halves_are_joined_only_when_both_present() {
        let schema = serde_json::json!({});
        let both = CurationRequest {
            system_stable: "A",
            system_volatile: "B",
            user: "u",
            schema: schema.clone(),
        };
        assert_eq!(both.system_combined(), "A\n\nB");
        let one = CurationRequest {
            system_stable: "A",
            system_volatile: "  ",
            user: "u",
            schema,
        };
        assert_eq!(one.system_combined(), "A");
    }
}
