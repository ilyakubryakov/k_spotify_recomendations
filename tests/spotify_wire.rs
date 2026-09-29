//! Wire-level tests for the Spotify client: rate limiting, pagination,
//! auth-header plumbing, 401 recovery and playlist write chunking.

// Integration tests are their own crate, so the lib's `cfg_attr(test)` lint
// relaxations do not apply here. Assertions legitimately index and unwrap.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod support;

use spotify_agent::config::SpotifyConfig;
use spotify_agent::domain::TimeRange;
use spotify_agent::spotify::{Authenticator, SpotifyClient};
use support::{MockServer, Reply, TempDir, write_valid_tokens};

const CLIENT_ID: &str = "test-client-id";

async fn client_for(server: &MockServer, dir: &TempDir) -> SpotifyClient {
    let mut cfg = SpotifyConfig {
        api_base: format!("{}/v1", server.base_url()),
        accounts_base: server.base_url(),
        client_id: Some(CLIENT_ID.into()),
        concurrency: 2,
        ..Default::default()
    };
    // Keep the retry budget tight so a failing test fails fast.
    cfg.max_retries = 3;

    let token_path = dir.join("tokens.json");
    write_valid_tokens(&token_path, CLIENT_ID);

    let http = reqwest::Client::new();
    let auth = Authenticator::new(cfg.clone(), CLIENT_ID.into(), http.clone(), token_path)
        .await
        .expect("authenticator");
    SpotifyClient::new(&cfg, http, auth)
}

fn track_json(id: &str, name: &str, artist: &str) -> String {
    format!(
        r#"{{"id":"{id}","name":"{name}","duration_ms":210000,"popularity":55,"explicit":false,
            "artists":[{{"id":"ar-{id}","name":"{artist}"}}],
            "album":{{"name":"Album","release_date":"2019-04-05","release_date_precision":"day"}},
            "external_ids":{{"isrc":"XX{id}"}}}}"#
    )
}

#[tokio::test]
async fn sends_bearer_token_and_parses_tracks() {
    let body = format!(
        r#"{{"items":[{{"track":{}}}],"next":null,"total":1}}"#,
        track_json("t1", "Song", "Artist")
    );
    let server = MockServer::start(vec![Reply::json(200, body)]).await;
    let dir = TempDir::new("auth-header");
    let client = client_for(&server, &dir).await;

    let tracks = client.saved_tracks(None).await.expect("saved tracks");

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].id, "t1");
    assert_eq!(tracks[0].release_year, Some(2019));
    assert_eq!(tracks[0].isrc.as_deref(), Some("XXt1"));

    let request = &server.requests()[0];
    assert_eq!(
        request.header("authorization"),
        Some("Bearer test-access-token")
    );
    assert!(request.path.starts_with("/v1/me/tracks"));
}

#[tokio::test]
async fn follows_the_next_link_until_exhausted() {
    // Spotify returns `next` as an absolute URL; the client must follow it
    // verbatim rather than rebuilding the query from its own base.
    let server = MockServer::start_with(|base| {
        vec![
            Reply::json(
                200,
                format!(
                    r#"{{"items":[{{"track":{}}}],"next":"{base}/v1/me/tracks?offset=50"}}"#,
                    track_json("t1", "One", "A")
                ),
            ),
            Reply::json(
                200,
                format!(
                    r#"{{"items":[{{"track":{}}}],"next":null}}"#,
                    track_json("t2", "Two", "B")
                ),
            ),
        ]
    })
    .await;

    let dir = TempDir::new("paginate");
    let client = client_for(&server, &dir).await;
    let tracks = client.saved_tracks(None).await.expect("saved tracks");

    assert_eq!(tracks.len(), 2, "both pages should be collected");
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn honours_retry_after_on_429_then_succeeds() {
    let server = MockServer::start(vec![
        Reply::json(429, r#"{"error":{"status":429,"message":"rate limited"}}"#)
            .with_header("retry-after", "1"),
        Reply::json(200, r#"{"items":[],"next":null}"#),
    ])
    .await;
    let dir = TempDir::new("429");
    let client = client_for(&server, &dir).await;

    let started = std::time::Instant::now();
    let tracks = client
        .top_tracks(TimeRange::ShortTerm)
        .await
        .expect("retried");
    let elapsed = started.elapsed();

    assert!(tracks.is_empty());
    assert_eq!(server.request_count(), 2, "should retry exactly once");
    // The client sleeps Retry-After plus a small guard, so ~1.25s. Anything
    // under a second would mean the header was ignored.
    assert!(
        elapsed.as_millis() >= 1_000,
        "slept only {elapsed:?}; Retry-After was ignored"
    );
}

#[tokio::test]
async fn gives_up_and_reports_rate_limiting() {
    let server = MockServer::start(vec![
        Reply::json(429, r#"{"error":{"status":429,"message":"nope"}}"#)
            .with_header("retry-after", "0"),
    ])
    .await;
    let dir = TempDir::new("429-exhaust");
    let client = client_for(&server, &dir).await;

    let error = client
        .top_tracks(TimeRange::LongTerm)
        .await
        .expect_err("should fail");
    let rendered = error.to_string();
    assert!(
        rendered.contains("rate limited") || rendered.contains("exhausted"),
        "unexpected error: {rendered}"
    );
    // EX_TEMPFAIL: a cron wrapper must be able to tell "retry later" apart
    // from "your config is wrong".
    assert_eq!(error.exit_code(), 75);
}

#[tokio::test]
async fn a_401_invalidates_the_token_and_retries_once() {
    let server = MockServer::start(vec![
        Reply::json(401, r#"{"error":{"status":401,"message":"expired"}}"#),
        // The refresh round-trip lands on /api/token.
        Reply::json(
            200,
            r#"{"access_token":"fresh-token","token_type":"Bearer","expires_in":3600,"scope":"user-top-read"}"#,
        ),
        Reply::json(200, r#"{"items":[],"next":null}"#),
    ])
    .await;
    let dir = TempDir::new("401");
    let client = client_for(&server, &dir).await;

    let result = client.top_tracks(TimeRange::MediumTerm).await;
    assert!(result.is_ok(), "401 should be recovered: {result:?}");

    let requests = server.requests();
    assert!(
        requests.iter().any(|r| r.path.contains("/api/token")),
        "expected a token refresh, got: {:?}",
        requests.iter().map(|r| &r.path).collect::<Vec<_>>()
    );
    let last = requests.last().expect("a final request");
    assert_eq!(last.header("authorization"), Some("Bearer fresh-token"));
}

#[tokio::test]
async fn playlist_writes_are_chunked_at_100_uris() {
    let server = MockServer::start(vec![Reply::json(200, r#"{"snapshot_id":"snap"}"#)]).await;
    let dir = TempDir::new("chunk");
    let client = client_for(&server, &dir).await;

    let uris: Vec<String> = (0..250).map(|i| format!("spotify:track:{i:03}")).collect();
    client
        .replace_playlist_tracks("pl1", &uris)
        .await
        .expect("write");

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "250 URIs should become 3 requests");
    // First clears (PUT), the rest append (POST) — anything else would either
    // drop tracks or duplicate them.
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[2].method, "POST");

    let count = |r: &support::Recorded| r.json()["uris"].as_array().map(Vec::len).unwrap_or(0);
    assert_eq!(count(&requests[0]), 100);
    assert_eq!(count(&requests[1]), 100);
    assert_eq!(count(&requests[2]), 50);
}

#[tokio::test]
async fn an_empty_replace_still_clears_the_playlist() {
    let server = MockServer::start(vec![Reply::json(200, r#"{"snapshot_id":"snap"}"#)]).await;
    let dir = TempDir::new("empty-replace");
    let client = client_for(&server, &dir).await;

    client
        .replace_playlist_tracks("pl1", &[])
        .await
        .expect("write");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "PUT");
    assert_eq!(requests[0].json()["uris"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn a_403_explains_the_missing_scope() {
    let server = MockServer::start(vec![Reply::json(
        403,
        r#"{"error":{"status":403,"message":"Insufficient client scope"}}"#,
    )])
    .await;
    let dir = TempDir::new("403");
    let client = client_for(&server, &dir).await;

    let error = client.user_playlists().await.expect_err("should fail");
    let rendered = error.to_string();
    assert!(rendered.contains("Insufficient client scope"), "{rendered}");
    assert!(
        rendered.contains("login"),
        "a 403 should point at re-authorising: {rendered}"
    );
    // Not retried: a scope problem will never resolve itself.
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn concurrent_searches_preserve_input_order() {
    let server = MockServer::start(vec![Reply::json(
        200,
        format!(
            r#"{{"tracks":{{"items":[{}]}}}}"#,
            track_json("x", "Song", "Artist")
        ),
    )])
    .await;
    let dir = TempDir::new("search-order");
    let client = client_for(&server, &dir).await;

    let queries: Vec<String> = (0..12).map(|i| format!("track:\"t{i}\"")).collect();
    let results = client.search_many(queries.clone(), 5).await;

    assert_eq!(results.len(), queries.len());
    assert!(
        results.iter().all(|r| r.is_ok()),
        "all searches should resolve"
    );
}
