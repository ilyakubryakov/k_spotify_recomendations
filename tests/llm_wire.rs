//! Wire-level tests for every LLM backend and for the fallback chain.
//!
//! Each provider gets its exact request shape locked in, because the failure
//! mode when one drifts is a 400 at 3am in a cron run, not a compile error.

// Integration tests are their own crate, so the lib's `cfg_attr(test)` lint
// relaxations do not apply here. Assertions legitimately index and unwrap.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod support;

use spotify_agent::config::{BackendConfig, Config, Effort, ThinkingMode};
use spotify_agent::llm::anthropic::AnthropicBackend;
use spotify_agent::llm::chain::LlmChain;
use spotify_agent::llm::gemini::GeminiBackend;
use spotify_agent::llm::openai::OpenAiBackend;
use spotify_agent::llm::schema::curation_schema;
use spotify_agent::llm::{CurationRequest, Provider, parse_structured};
use spotify_agent::util::secret::Secret;
use support::{MockServer, Reply};

const OK_JSON: &str = r#"{"playlist_title":"Night Drive","summary":"A set.","tracks":[]}"#;

fn request() -> CurationRequest<'static> {
    CurationRequest {
        system_stable: "STABLE SYSTEM",
        system_volatile: "VOLATILE",
        user: "user message",
        schema: curation_schema(),
    }
}

fn backend_cfg(server: &MockServer, provider: Provider, stream: bool) -> BackendConfig {
    BackendConfig {
        provider,
        api_key: Some(Secret::new("test-key")),
        base_url: Some(server.base_url()),
        model: match provider {
            Provider::Anthropic => "claude-opus-5".into(),
            Provider::Openai => "gpt-test".into(),
            Provider::Ollama => "llama-test".into(),
            Provider::Gemini => "gemini-test".into(),
        },
        stream,
        max_retries: 3,
        ..Default::default()
    }
}

// ===========================================================================
// Anthropic
// ===========================================================================

fn anthropic_ok() -> Reply {
    Reply::json(
        200,
        format!(
            r#"{{"id":"msg_1","model":"claude-opus-5","stop_reason":"end_turn",
                 "content":[{{"type":"thinking","thinking":"weighing options"}},
                            {{"type":"text","text":{}}}],
                 "usage":{{"input_tokens":1234,"output_tokens":567}}}}"#,
            serde_json::to_string(OK_JSON).expect("encode")
        ),
    )
}

#[tokio::test]
async fn anthropic_request_shape_matches_the_current_api() {
    let server = MockServer::start(vec![anthropic_ok()]).await;
    let backend = AnthropicBackend::new(
        backend_cfg(&server, Provider::Anthropic, false),
        reqwest::Client::new(),
    )
    .expect("backend");

    let completion = backend.complete(&request(), None).await.expect("call");
    let value = parse_structured(&completion).expect("json");

    assert_eq!(value["playlist_title"], "Night Drive");
    assert_eq!(completion.input_tokens, 1234);
    assert_eq!(completion.output_tokens, 567);
    assert_eq!(completion.thinking, "weighing options");
    assert_eq!(completion.provider, Provider::Anthropic);

    let req = &server.requests()[0];
    assert_eq!(req.path, "/v1/messages");
    assert_eq!(req.header("x-api-key"), Some("test-key"));
    assert_eq!(req.header("anthropic-version"), Some("2023-06-01"));

    let body = req.json();
    // Removed on this model generation — sending any of them is a 400.
    assert!(body.get("temperature").is_none());
    assert!(body.get("top_p").is_none());
    assert!(body.get("top_k").is_none());
    assert!(body["thinking"].get("budget_tokens").is_none());

    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    // Cache breakpoint on the stable prefix only.
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert!(body["system"][1].get("cache_control").is_none());
}

#[tokio::test]
async fn anthropic_streams_and_reports_usage() {
    let (a, b) = OK_JSON.split_at(OK_JSON.len() / 2);
    let server = MockServer::start(vec![Reply::sse(&[
        r#"{"type":"message_start","message":{"model":"claude-opus-5","usage":{"input_tokens":900}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reasoning…"}}"#,
        &format!(
            r#"{{"type":"content_block_delta","index":1,"delta":{{"type":"text_delta","text":{}}}}}"#,
            serde_json::to_string(a).expect("encode")
        ),
        &format!(
            r#"{{"type":"content_block_delta","index":1,"delta":{{"type":"text_delta","text":{}}}}}"#,
            serde_json::to_string(b).expect("encode")
        ),
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":321}}"#,
    ])])
    .await;

    let backend = AnthropicBackend::new(
        backend_cfg(&server, Provider::Anthropic, true),
        reqwest::Client::new(),
    )
    .expect("backend");
    let completion = backend.complete(&request(), None).await.expect("call");

    assert_eq!(
        parse_structured(&completion).expect("json")["playlist_title"],
        "Night Drive"
    );
    assert_eq!(completion.input_tokens, 900);
    assert_eq!(completion.output_tokens, 321);
}

#[tokio::test]
async fn anthropic_refusal_is_an_error() {
    let server = MockServer::start(vec![Reply::sse(&[
        r#"{"type":"message_start","message":{"model":"claude-opus-5"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"declined"}}}"#,
    ])])
    .await;
    let backend = AnthropicBackend::new(
        backend_cfg(&server, Provider::Anthropic, true),
        reqwest::Client::new(),
    )
    .expect("backend");

    let error = backend
        .complete(&request(), None)
        .await
        .expect_err("refusal");
    assert!(error.to_string().contains("declined"), "{error}");
    assert_eq!(server.request_count(), 1, "a refusal must not be retried");
}

#[tokio::test]
async fn anthropic_truncation_is_reported_clearly() {
    let server = MockServer::start(vec![Reply::json(
        200,
        r#"{"model":"claude-opus-5","stop_reason":"max_tokens",
            "content":[{"type":"text","text":"{\"tracks\": ["}],
            "usage":{"input_tokens":1,"output_tokens":2}}"#,
    )])
    .await;
    let backend = AnthropicBackend::new(
        backend_cfg(&server, Provider::Anthropic, false),
        reqwest::Client::new(),
    )
    .expect("backend");

    let error = backend
        .complete(&request(), None)
        .await
        .expect_err("truncated");
    assert!(error.to_string().contains("truncated"), "{error}");
}

#[tokio::test]
async fn anthropic_429_honours_retry_after() {
    let server = MockServer::start(vec![
        Reply::json(
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        )
        .with_header("retry-after", "1"),
        anthropic_ok(),
    ])
    .await;
    let backend = AnthropicBackend::new(
        backend_cfg(&server, Provider::Anthropic, false),
        reqwest::Client::new(),
    )
    .expect("backend");

    let started = std::time::Instant::now();
    backend.complete(&request(), None).await.expect("retried");
    assert!(
        started.elapsed().as_millis() >= 1_000,
        "Retry-After was ignored"
    );
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn anthropic_disabled_thinking_serialises_correctly() {
    let server = MockServer::start(vec![anthropic_ok()]).await;
    let mut cfg = backend_cfg(&server, Provider::Anthropic, false);
    cfg.thinking = ThinkingMode::Disabled;
    cfg.effort = Effort::Medium; // disabled thinking is only valid at <= high
    let backend = AnthropicBackend::new(cfg, reqwest::Client::new()).expect("backend");

    backend.complete(&request(), None).await.expect("call");
    let body = server.requests()[0].json();
    assert_eq!(body["thinking"]["type"], "disabled");
    assert_eq!(body["output_config"]["effort"], "medium");
}

// ===========================================================================
// OpenAI / Ollama
// ===========================================================================

fn openai_ok() -> Reply {
    Reply::json(
        200,
        format!(
            r#"{{"model":"gpt-test","choices":[{{"finish_reason":"stop","message":{{"content":{}}}}}],
                 "usage":{{"prompt_tokens":10,"completion_tokens":20}}}}"#,
            serde_json::to_string(OK_JSON).expect("encode")
        ),
    )
}

#[tokio::test]
async fn openai_sends_strict_json_schema_and_bearer_auth() {
    let server = MockServer::start(vec![openai_ok()]).await;
    let backend = OpenAiBackend::new(
        backend_cfg(&server, Provider::Openai, false),
        reqwest::Client::new(),
    )
    .expect("backend");

    let completion = backend.complete(&request(), None).await.expect("call");
    assert_eq!(
        parse_structured(&completion).expect("json")["playlist_title"],
        "Night Drive"
    );
    assert_eq!(completion.input_tokens, 10);
    assert_eq!(completion.output_tokens, 20);

    let req = &server.requests()[0];
    assert_eq!(req.path, "/v1/chat/completions");
    assert_eq!(req.header("authorization"), Some("Bearer test-key"));

    let body = req.json();
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    assert_eq!(
        body["response_format"]["json_schema"]["schema"]["additionalProperties"],
        false
    );
    // OpenAI's current parameter name; `max_tokens` is rejected.
    assert!(body.get("max_completion_tokens").is_some());
    assert!(body.get("max_tokens").is_none());
}

#[tokio::test]
async fn ollama_uses_max_tokens_and_needs_no_key() {
    let server = MockServer::start(vec![openai_ok()]).await;
    let mut cfg = backend_cfg(&server, Provider::Ollama, false);
    cfg.api_key = None;
    // Point at a variable that is certainly unset, so the no-key path is real.
    cfg.api_key_env = Some("SPOTIFY_AGENT_DEFINITELY_UNSET_KEY".into());
    let backend = OpenAiBackend::new(cfg, reqwest::Client::new()).expect("backend without a key");

    backend.complete(&request(), None).await.expect("call");

    let req = &server.requests()[0];
    assert!(
        req.header("authorization").is_none(),
        "a local server gets no bearer token"
    );
    let body = req.json();
    assert!(body.get("max_tokens").is_some());
    assert!(body.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn openai_streaming_reassembles_content() {
    let (a, b) = OK_JSON.split_at(OK_JSON.len() / 2);
    let server = MockServer::start(vec![Reply::sse(&[
        &format!(
            r#"{{"model":"gpt-test","choices":[{{"delta":{{"content":{}}}}}]}}"#,
            serde_json::to_string(a).expect("encode")
        ),
        &format!(
            r#"{{"model":"gpt-test","choices":[{{"delta":{{"content":{}}},"finish_reason":"stop"}}]}}"#,
            serde_json::to_string(b).expect("encode")
        ),
        r#"{"model":"gpt-test","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":6}}"#,
        "[DONE]",
    ])])
    .await;
    let backend = OpenAiBackend::new(
        backend_cfg(&server, Provider::Openai, true),
        reqwest::Client::new(),
    )
    .expect("backend");

    let completion = backend.complete(&request(), None).await.expect("call");
    assert_eq!(
        parse_structured(&completion).expect("json")["playlist_title"],
        "Night Drive"
    );
    assert_eq!(completion.output_tokens, 6);
    // Streaming must ask for usage explicitly or the totals never arrive.
    assert_eq!(
        server.requests()[0].json()["stream_options"]["include_usage"],
        true
    );
}

// ===========================================================================
// Gemini
// ===========================================================================

#[tokio::test]
async fn gemini_translates_the_schema_and_uses_the_key_header() {
    let server = MockServer::start(vec![Reply::json(
        200,
        format!(
            r#"{{"modelVersion":"gemini-test",
                 "candidates":[{{"finishReason":"STOP","content":{{"parts":[
                    {{"text":"thinking out loud","thought":true}},
                    {{"text":{}}}]}}}}],
                 "usageMetadata":{{"promptTokenCount":3,"candidatesTokenCount":4}}}}"#,
            serde_json::to_string(OK_JSON).expect("encode")
        ),
    )])
    .await;
    let backend = GeminiBackend::new(
        backend_cfg(&server, Provider::Gemini, false),
        reqwest::Client::new(),
    )
    .expect("backend");

    let completion = backend.complete(&request(), None).await.expect("call");
    // A `thought` part must never reach the JSON parser.
    assert_eq!(
        parse_structured(&completion).expect("json")["playlist_title"],
        "Night Drive"
    );
    assert_eq!(completion.thinking, "thinking out loud");
    assert_eq!(completion.input_tokens, 3);

    let req = &server.requests()[0];
    assert!(
        req.path
            .starts_with("/v1beta/models/gemini-test:generateContent")
    );
    assert_eq!(req.header("x-goog-api-key"), Some("test-key"));
    // The key must never be in the URL — it would land in proxy logs.
    assert!(!req.path.contains("test-key"));

    let body = req.json();
    let schema = &body["generationConfig"]["responseSchema"];
    assert!(
        schema.get("additionalProperties").is_none(),
        "gemini rejects additionalProperties"
    );
    assert!(schema["propertyOrdering"].as_array().is_some());
    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert!(
        body["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("STABLE SYSTEM")
    );
}

#[tokio::test]
async fn gemini_safety_block_is_a_refusal() {
    let server = MockServer::start(vec![Reply::json(
        200,
        r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#,
    )])
    .await;
    let backend = GeminiBackend::new(
        backend_cfg(&server, Provider::Gemini, false),
        reqwest::Client::new(),
    )
    .expect("backend");

    let error = backend
        .complete(&request(), None)
        .await
        .expect_err("blocked");
    assert!(error.to_string().contains("SAFETY"), "{error}");
}

// ===========================================================================
// The chain
// ===========================================================================

async fn chain_config(primary: &MockServer, fallback: &MockServer) -> Config {
    let mut config = Config {
        presets: spotify_agent::config::presets::builtin(),
        ..Default::default()
    };
    config.claude = backend_cfg(primary, Provider::Anthropic, false);
    config.llm.fallbacks = vec![backend_cfg(fallback, Provider::Openai, false)];
    config
}

#[tokio::test]
async fn chain_falls_forward_when_the_primary_is_down() {
    let primary = MockServer::start(vec![Reply::json(
        500,
        r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#,
    )])
    .await;
    let fallback = MockServer::start(vec![openai_ok()]).await;
    let config = chain_config(&primary, &fallback).await;

    let chain = LlmChain::build(&config, reqwest::Client::new()).expect("chain");
    let mut fell_back_to = String::new();
    let outcome = chain
        .curate(&request(), None, |backend, _reason| {
            fell_back_to = backend.to_string()
        })
        .await
        .expect("fallback answered");

    assert!(outcome.used_fallback);
    assert_eq!(outcome.backend, "openai/gpt-test");
    assert_eq!(fell_back_to, "openai/gpt-test");
    assert_eq!(outcome.value["playlist_title"], "Night Drive");
    // The primary was retried to exhaustion before giving up.
    assert!(
        primary.request_count() >= 2,
        "primary should retry before falling forward"
    );
}

#[tokio::test]
async fn chain_falls_forward_on_a_refusal() {
    let primary = MockServer::start(vec![Reply::json(
        200,
        r#"{"model":"claude-opus-5","stop_reason":"refusal",
            "stop_details":{"type":"refusal","category":"cyber","explanation":"no"},
            "content":[]}"#,
    )])
    .await;
    let fallback = MockServer::start(vec![openai_ok()]).await;
    let config = chain_config(&primary, &fallback).await;

    let chain = LlmChain::build(&config, reqwest::Client::new()).expect("chain");
    let outcome = chain
        .curate(&request(), None, |_, _| {})
        .await
        .expect("fallback answered");

    assert!(
        outcome.used_fallback,
        "a refusal should try the next provider"
    );
    assert_eq!(
        primary.request_count(),
        1,
        "a refusal is not retried on the same backend"
    );
}

#[tokio::test]
async fn chain_falls_forward_when_output_is_not_valid_json() {
    let primary = MockServer::start(vec![Reply::json(
        200,
        r#"{"model":"claude-opus-5","stop_reason":"end_turn",
            "content":[{"type":"text","text":"I'm afraid I can't do that"}],
            "usage":{"input_tokens":1,"output_tokens":1}}"#,
    )])
    .await;
    let fallback = MockServer::start(vec![openai_ok()]).await;
    let config = chain_config(&primary, &fallback).await;

    let chain = LlmChain::build(&config, reqwest::Client::new()).expect("chain");
    let outcome = chain
        .curate(&request(), None, |_, _| {})
        .await
        .expect("fallback answered");
    assert!(outcome.used_fallback);
}

#[tokio::test]
async fn chain_uses_the_primary_when_it_works_and_never_touches_the_fallback() {
    let primary = MockServer::start(vec![anthropic_ok()]).await;
    let fallback = MockServer::start(vec![openai_ok()]).await;
    let config = chain_config(&primary, &fallback).await;

    let chain = LlmChain::build(&config, reqwest::Client::new()).expect("chain");
    let outcome = chain
        .curate(&request(), None, |_, _| {})
        .await
        .expect("primary answered");

    assert!(!outcome.used_fallback);
    assert_eq!(outcome.backend, "anthropic/claude-opus-5");
    assert_eq!(
        fallback.request_count(),
        0,
        "the fallback must stay untouched"
    );
}

#[tokio::test]
async fn chain_reports_the_last_error_when_every_backend_fails() {
    let primary = MockServer::start(vec![Reply::json(
        500,
        r#"{"error":{"message":"primary down"}}"#,
    )])
    .await;
    let fallback = MockServer::start(vec![Reply::json(
        500,
        r#"{"error":{"message":"fallback down"}}"#,
    )])
    .await;
    let config = chain_config(&primary, &fallback).await;

    let chain = LlmChain::build(&config, reqwest::Client::new()).expect("chain");
    let error = chain
        .curate(&request(), None, |_, _| {})
        .await
        .err()
        .expect("everything failed");

    assert_eq!(
        error.exit_code(),
        75,
        "an all-down chain is a temporary failure"
    );
}
