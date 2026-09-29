//! The ordered backend chain.
//!
//! One curation turn is attempted against each configured backend in order
//! until one answers. A backend is skipped permanently (at construction) when
//! it has no usable credentials, and skipped for this turn when it fails in a
//! way another provider might survive:
//!
//! | failure                         | move to the next backend? |
//! |---------------------------------|---------------------------|
//! | network / 5xx / retries spent   | yes                       |
//! | rate limited past the budget    | yes                       |
//! | model refusal                   | yes — a different model may not refuse |
//! | output that fails the schema    | yes                       |
//! | truncated at the token cap      | yes                       |
//! | user cancelled                  | **no** — abort immediately |
//!
//! The chain never silently degrades: every fallback is logged at `warn`, and
//! the backend that actually served the turn is reported in the result so the
//! CLI and TUI can show it.

use crate::config::BackendConfig;
use crate::config::Config;
use crate::error::{AgentError, Result};
use crate::llm::Provider;
use crate::llm::{
    Backend, Completion, CurationRequest, Listener, anthropic::AnthropicBackend,
    gemini::GeminiBackend, openai::OpenAiBackend,
};
use serde_json::Value;

pub struct LlmChain {
    backends: Vec<Backend>,
}

/// A successful turn plus which backend produced it.
pub struct ChainOutcome {
    pub value: Value,
    pub completion: Completion,
    /// `anthropic/claude-opus-5` — the backend that answered.
    pub backend: String,
    /// True when the primary did not answer and a fallback did.
    pub used_fallback: bool,
}

impl LlmChain {
    /// Build every backend that has usable credentials.
    ///
    /// A backend with no key is dropped with a warning rather than failing the
    /// whole chain — that is the entire point of configuring several. The
    /// chain is only an error when *nothing* is usable.
    pub fn build(config: &Config, http: reqwest::Client) -> Result<Self> {
        let mut backends = Vec::new();
        let mut problems = Vec::new();

        let mut add =
            |cfg: &BackendConfig, label: &str, backends: &mut Vec<Backend>| match construct(
                cfg,
                http.clone(),
            ) {
                Ok(backend) => backends.push(backend),
                Err(e) => {
                    tracing::warn!(backend = label, error = %e, "skipping unusable LLM backend");
                    problems.push(format!("{label}: {e}"));
                }
            };

        let primary_label = config.claude.label();
        add(&config.claude, &primary_label, &mut backends);
        for fallback in &config.llm.fallbacks {
            let label = fallback.label();
            add(fallback, &label, &mut backends);
        }

        if backends.is_empty() {
            return Err(AgentError::MissingCredential {
                name: "llm api key",
                hint: format!(
                    "no usable LLM backend. Tried: {}. Export a key (e.g. ANTHROPIC_API_KEY) \
                     or configure a local one under [[llm.fallbacks]] with provider = \"ollama\"",
                    problems.join("; ")
                ),
            });
        }

        tracing::debug!(
            backends = ?backends.iter().map(Backend::label).collect::<Vec<_>>(),
            "llm chain ready"
        );
        Ok(Self { backends })
    }

    /// Label of the backend that will be tried first.
    pub fn primary_label(&self) -> String {
        self.backends
            .first()
            .map(Backend::label)
            .unwrap_or_default()
    }

    pub fn labels(&self) -> Vec<String> {
        self.backends.iter().map(Backend::label).collect()
    }

    /// Run the turn, falling forward through the chain.
    pub async fn curate(
        &self,
        request: &CurationRequest<'_>,
        listener: Option<Listener>,
        mut on_fallback: impl FnMut(&str, &str),
    ) -> Result<ChainOutcome> {
        let mut last_error: Option<AgentError> = None;

        for (index, backend) in self.backends.iter().enumerate() {
            let label = backend.label();
            if index > 0 {
                let reason = last_error
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "unknown".into());
                tracing::warn!(backend = %label, %reason, "falling back to the next LLM backend");
                on_fallback(&label, &reason);
            }

            match backend.complete(request, listener.clone()).await {
                Ok(completion) => match crate::llm::parse_structured(&completion) {
                    Ok(value) => {
                        return Ok(ChainOutcome {
                            value,
                            completion,
                            backend: label,
                            used_fallback: index > 0,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(backend = %label, error = %e, "backend returned unusable output");
                        last_error = Some(e);
                    }
                },
                // Cancellation is the user's decision, not a provider failure:
                // trying the next model would ignore a Ctrl-C.
                Err(AgentError::Cancelled) => return Err(AgentError::Cancelled),
                Err(e) => {
                    tracing::warn!(backend = %label, error = %e, "backend failed");
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            AgentError::other("the LLM chain produced neither a result nor an error")
        }))
    }
}

fn construct(cfg: &BackendConfig, http: reqwest::Client) -> Result<Backend> {
    Ok(match cfg.provider {
        Provider::Anthropic => Backend::Anthropic(AnthropicBackend::new(cfg.clone(), http)?),
        Provider::Openai | Provider::Ollama => {
            Backend::OpenAi(OpenAiBackend::new(cfg.clone(), http)?)
        }
        Provider::Gemini => Backend::Gemini(GeminiBackend::new(cfg.clone(), http)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> Config {
        Config {
            presets: crate::config::presets::builtin(),
            ..Default::default()
        }
    }

    #[test]
    fn a_keyless_local_backend_keeps_the_chain_usable() {
        // No ANTHROPIC_API_KEY in this test process, so the primary is dropped;
        // Ollama needs no key, so the chain must still build.
        let mut config = base_config();
        config.claude.api_key_env = Some("SPOTIFY_AGENT_DEFINITELY_UNSET_KEY".into());
        config.llm.fallbacks = vec![BackendConfig {
            provider: Provider::Ollama,
            model: "llama3.1".into(),
            ..Default::default()
        }];

        let chain = LlmChain::build(&config, reqwest::Client::new()).expect("chain builds");
        assert_eq!(chain.labels(), vec!["ollama/llama3.1".to_string()]);
    }

    #[test]
    fn no_usable_backend_is_a_credential_error() {
        let mut config = base_config();
        config.claude.api_key_env = Some("SPOTIFY_AGENT_DEFINITELY_UNSET_KEY".into());
        let Err(error) = LlmChain::build(&config, reqwest::Client::new()) else {
            panic!("a chain with no usable backend must not build");
        };
        assert!(matches!(error, AgentError::MissingCredential { .. }));
        assert_eq!(error.exit_code(), 78);
    }

    #[test]
    fn chain_order_is_primary_then_fallbacks() {
        let mut config = base_config();
        config.claude.api_key = Some(crate::util::secret::Secret::new("k"));
        config.llm.fallbacks = vec![
            BackendConfig {
                provider: Provider::Ollama,
                model: "local".into(),
                ..Default::default()
            },
            BackendConfig {
                provider: Provider::Gemini,
                model: "g".into(),
                api_key: Some(crate::util::secret::Secret::new("k")),
                ..Default::default()
            },
        ];
        let chain = LlmChain::build(&config, reqwest::Client::new()).expect("chain");
        assert_eq!(
            chain.labels(),
            vec![
                "anthropic/claude-opus-5".to_string(),
                "ollama/local".to_string(),
                "gemini/g".to_string()
            ]
        );
        assert_eq!(chain.primary_label(), "anthropic/claude-opus-5");
    }
}
