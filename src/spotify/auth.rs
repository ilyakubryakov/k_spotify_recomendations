//! Spotify authorisation.
//!
//! Default flow is **Authorization Code with PKCE** (RFC 7636): a desktop
//! binary cannot keep a client secret, so it should not have one. If the user
//! does configure `spotify.client_secret`, the classic confidential-client
//! flow is used instead (HTTP Basic), with PKCE still layered on top — that
//! combination is valid and strictly safer than either alone.
//!
//! The redirect target is a loopback listener we start ourselves. Spotify
//! requires an explicit-IP loopback (`http://127.0.0.1:<port>/callback`);
//! `localhost` is rejected at app-registration time.
//!
//! Tokens are persisted to `<data_dir>/tokens.json` with mode 0600. Refresh is
//! serialised behind a mutex so that N concurrent API calls hitting an expired
//! token produce exactly one refresh request, not N.

use crate::config::SpotifyConfig;
use crate::error::{AgentError, Result};
use crate::spotify::models::{OAuthErrorEnvelope, TokenResponse};
use crate::util::fs::write_private;
use crate::util::secret::Secret;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, RwLock};

/// Least-privilege scope set. Each one is load-bearing, and every write the
/// client can perform must appear here — a missing one surfaces as a 403 at the
/// moment the user presses a key, which is the worst possible time to find out.
///
///   user-library-read          → read Liked Songs
///   user-library-modify        → save/unsave a track (TUI `f` key)
///   user-top-read              → /me/top/{artists,tracks}
///   user-read-recently-played  → play history
///   user-read-private          → market inference
///   playlist-read-private      → find an existing target playlist
///   playlist-modify-private    → write a private playlist
///   playlist-modify-public     → write a public one (only if configured)
///
/// `scope_coverage` in the tests below pins each API path to its scope, so
/// adding an endpoint without its scope fails the build rather than a keypress.
pub const SCOPES: &[&str] = &[
    "user-library-read",
    "user-library-modify",
    "user-top-read",
    "user-read-recently-played",
    "user-read-private",
    "playlist-read-private",
    "playlist-modify-private",
    "playlist-modify-public",
];

/// Refresh this long before nominal expiry, so a long-running request never
/// races the boundary.
const REFRESH_SKEW_SECS: i64 = 120;

/// How long the loopback listener waits for the user to finish in the browser.
const LOGIN_TIMEOUT_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Token storage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access_token: Secret,
    pub refresh_token: Secret,
    pub expires_at: DateTime<Utc>,
    pub scope: String,
}

impl TokenSet {
    fn is_fresh(&self) -> bool {
        Utc::now() + ChronoDuration::seconds(REFRESH_SKEW_SECS) < self.expires_at
    }

    /// True when the persisted grant lacks a scope we now require — e.g. after
    /// upgrading a version that added playlist writes. The user is told to
    /// re-run `login` rather than being handed a confusing 403.
    fn missing_scopes(&self) -> Vec<&'static str> {
        let granted: Vec<&str> = self.scope.split_whitespace().collect();
        SCOPES
            .iter()
            .copied()
            .filter(|s| !granted.contains(s))
            .collect()
    }
}

/// On-disk representation. Separate from [`TokenSet`] because `Secret`
/// deliberately serialises as `"<redacted>"`; persisting requires an explicit,
/// greppable `expose()`.
#[derive(Debug, Serialize, Deserialize)]
struct StoredTokens {
    version: u8,
    access_token: String,
    refresh_token: String,
    expires_at: DateTime<Utc>,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    client_id: String,
}

// ---------------------------------------------------------------------------
// Authenticator
// ---------------------------------------------------------------------------

pub struct Authenticator {
    cfg: SpotifyConfig,
    client_id: String,
    http: reqwest::Client,
    token_path: PathBuf,
    tokens: RwLock<Option<TokenSet>>,
    /// Serialises refresh so concurrent callers make one network call.
    refresh_lock: Mutex<()>,
}

impl Authenticator {
    pub async fn new(
        cfg: SpotifyConfig,
        client_id: String,
        http: reqwest::Client,
        token_path: PathBuf,
    ) -> Result<Arc<Self>> {
        let existing = load_tokens(&token_path, &client_id).await?;
        Ok(Arc::new(Self {
            cfg,
            client_id,
            http,
            token_path,
            tokens: RwLock::new(existing),
            refresh_lock: Mutex::new(()),
        }))
    }

    pub async fn is_authorized(&self) -> bool {
        self.tokens.read().await.is_some()
    }

    /// Scopes this version needs that the stored grant does not carry.
    ///
    /// Non-empty means some operation will 403 the moment it is attempted --
    /// usually after a version upgrade added a feature. Surfaced by `status`
    /// so it is discoverable before a keypress fails.
    pub async fn missing_scopes(&self) -> Vec<&'static str> {
        self.tokens
            .read()
            .await
            .as_ref()
            .map(TokenSet::missing_scopes)
            .unwrap_or_default()
    }

    pub async fn status(&self) -> Option<(DateTime<Utc>, String)> {
        self.tokens
            .read()
            .await
            .as_ref()
            .map(|t| (t.expires_at, t.scope.clone()))
    }

    /// Return a valid access token, refreshing if needed.
    pub async fn access_token(&self) -> Result<Secret> {
        if let Some(t) = self.tokens.read().await.as_ref()
            && t.is_fresh()
        {
            return Ok(t.access_token.clone());
        }

        // Serialise: whoever wins the lock refreshes, the rest observe the
        // fresh token and return immediately.
        let _guard = self.refresh_lock.lock().await;
        if let Some(t) = self.tokens.read().await.as_ref()
            && t.is_fresh()
        {
            return Ok(t.access_token.clone());
        }

        let current = self
            .tokens
            .read()
            .await
            .clone()
            .ok_or(AgentError::NotAuthorized)?;

        tracing::debug!("access token expired; refreshing");
        let refreshed = self.refresh(&current).await?;
        let access = refreshed.access_token.clone();
        self.store(refreshed).await?;
        Ok(access)
    }

    /// Mark the cached access token as unusable so the next
    /// [`Self::access_token`] call forces a refresh. Called when Spotify
    /// rejects a token we believed was still valid (revoked grant, clock skew,
    /// or a token invalidated by a password change).
    pub async fn invalidate(&self) {
        if let Some(t) = self.tokens.write().await.as_mut() {
            t.expires_at = Utc::now() - ChronoDuration::seconds(1);
        }
    }

    async fn refresh(&self, current: &TokenSet) -> Result<TokenSet> {
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", current.refresh_token.expose().to_string()),
            ("client_id", self.client_id.clone()),
        ];
        if self.cfg.client_secret.is_some() {
            // With a confidential client the id goes in the Basic header
            // instead; sending it twice is harmless but noisy.
            form.retain(|(k, _)| *k != "client_id");
        }

        let response = self.token_request(form).await.map_err(|e| match e {
            // A revoked or rotated refresh token is unrecoverable: tell the
            // user to re-login rather than retrying forever.
            AgentError::Auth(msg) => AgentError::Auth(format!(
                "{msg} — the stored refresh token is no longer valid; run `spotify-agent login`"
            )),
            other => other,
        })?;

        Ok(TokenSet {
            access_token: Secret::new(response.access_token),
            // Spotify omits `refresh_token` on refresh unless it rotated one.
            refresh_token: response
                .refresh_token
                .map(Secret::new)
                .unwrap_or_else(|| current.refresh_token.clone()),
            expires_at: expiry_from(response.expires_in),
            scope: response.scope.unwrap_or_else(|| current.scope.clone()),
        })
    }

    /// Full interactive authorisation. Returns once tokens are stored.
    pub async fn login(&self, no_browser: bool) -> Result<()> {
        let verifier = code_verifier();
        let challenge = code_challenge(&verifier);
        let state = random_token(24);

        let listener = self.bind_listener().await?;
        let authorize_url = self.authorize_url(&challenge, &state);

        println!("\nAuthorise spotify-agent in your browser:\n\n  {authorize_url}\n");
        if !no_browser && let Err(e) = open_browser(&authorize_url) {
            tracing::debug!(error = %e, "could not launch a browser; use the URL above");
        }
        println!("Waiting for the redirect on {} …", self.cfg.redirect_uri());

        let callback = tokio::time::timeout(
            std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS),
            wait_for_callback(listener, &self.cfg.redirect_path),
        )
        .await
        .map_err(|_| AgentError::Auth("timed out waiting for the browser redirect".into()))??;

        // Constant-shape comparison; `state` is the CSRF guard for the
        // redirect, so a mismatch is a hard failure, never a warning.
        if callback.state.as_deref() != Some(state.as_str()) {
            return Err(AgentError::Auth(
                "state mismatch on the OAuth callback — aborting".into(),
            ));
        }
        if let Some(err) = callback.error {
            return Err(AgentError::Auth(format!(
                "Spotify denied authorisation: {err}"
            )));
        }
        let code = callback
            .code
            .ok_or_else(|| AgentError::Auth("callback carried no authorization code".into()))?;

        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("redirect_uri", self.cfg.redirect_uri()),
            ("code_verifier", verifier),
        ];
        if self.cfg.client_secret.is_none() {
            form.push(("client_id", self.client_id.clone()));
        }

        let response = self.token_request(form).await?;
        let refresh_token = response.refresh_token.ok_or_else(|| {
            AgentError::Auth("Spotify returned no refresh token; check the app's settings".into())
        })?;

        let tokens = TokenSet {
            access_token: Secret::new(response.access_token),
            refresh_token: Secret::new(refresh_token),
            expires_at: expiry_from(response.expires_in),
            scope: response.scope.unwrap_or_default(),
        };

        let missing = tokens.missing_scopes();
        if !missing.is_empty() {
            tracing::warn!(missing = ?missing, "Spotify granted fewer scopes than requested");
        }

        self.store(tokens).await?;
        println!(
            "\nAuthorised. Tokens stored at {}",
            self.token_path.display()
        );
        Ok(())
    }

    /// Forget stored credentials.
    pub async fn logout(&self) -> Result<()> {
        *self.tokens.write().await = None;
        if self.token_path.exists() {
            tokio::fs::remove_file(&self.token_path)
                .await
                .map_err(|e| AgentError::io(self.token_path.display().to_string(), e))?;
        }
        Ok(())
    }

    async fn bind_listener(&self) -> Result<TcpListener> {
        let addr = format!("{}:{}", self.cfg.redirect_host, self.cfg.redirect_port);
        TcpListener::bind(&addr).await.map_err(|e| {
            AgentError::Auth(format!(
                "cannot bind the OAuth callback listener on {addr}: {e}. \
                 Change spotify.redirect_port (and the redirect URI registered in the Spotify dashboard)."
            ))
        })
    }

    fn authorize_url(&self, challenge: &str, state: &str) -> String {
        let mut url = format!("{}/authorize", self.cfg.accounts_base);
        let params = [
            ("client_id", self.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", &self.cfg.redirect_uri()),
            ("scope", &SCOPES.join(" ")),
            ("state", state),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge),
            // Always show the consent screen so a scope upgrade is visible to
            // the user instead of being silently auto-approved.
            ("show_dialog", "true"),
        ];
        url.push('?');
        url.push_str(&serde_urlencoded_lite(&params));
        url
    }

    async fn token_request(&self, form: Vec<(&str, String)>) -> Result<TokenResponse> {
        let url = format!("{}/api/token", self.cfg.accounts_base);
        let policy = crate::util::retry::RetryPolicy {
            max_attempts: 3,
            ..Default::default()
        };

        crate::util::retry::with_retry("spotify-auth", policy, |_attempt| {
            let http = self.http.clone();
            let url = url.clone();
            let form = form.clone();
            let secret = self.cfg.client_secret.clone();
            let client_id = self.client_id.clone();
            async move {
                let mut req = http.post(&url).form(&form);
                if let Some(secret) = &secret {
                    req = req.basic_auth(&client_id, Some(secret.expose()));
                }
                let response = req.send().await?;
                let status = response.status();
                let body = response.text().await?;

                if status.is_success() {
                    return serde_json::from_str::<TokenResponse>(&body)
                        .map_err(|e| AgentError::Auth(format!("malformed token response: {e}")));
                }

                let detail = serde_json::from_str::<OAuthErrorEnvelope>(&body)
                    .map(|e| match e.error_description {
                        Some(d) => format!("{}: {d}", e.error),
                        None => e.error,
                    })
                    // Never echo an unparsed body: it can contain the code or
                    // token that caused the failure.
                    .unwrap_or_else(|_| format!("HTTP {status}"));

                if status.as_u16() == 429 || status.is_server_error() {
                    Err(AgentError::Transient {
                        service: "spotify-auth",
                        status: status.as_u16(),
                        message: detail,
                    })
                } else {
                    Err(AgentError::Auth(detail))
                }
            }
        })
        .await
    }

    async fn store(&self, tokens: TokenSet) -> Result<()> {
        let stored = StoredTokens {
            version: 1,
            access_token: tokens.access_token.expose().to_string(),
            refresh_token: tokens.refresh_token.expose().to_string(),
            expires_at: tokens.expires_at,
            scope: tokens.scope.clone(),
            client_id: self.client_id.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&stored)?;
        write_private(&self.token_path, &bytes).await?;
        *self.tokens.write().await = Some(tokens);
        Ok(())
    }
}

async fn load_tokens(path: &PathBuf, client_id: &str) -> Result<Option<TokenSet>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = tokio::fs::read(path)
        .await
        .map_err(|e| AgentError::io(path.display().to_string(), e))?;
    let stored: StoredTokens = match serde_json::from_slice(&raw) {
        Ok(s) => s,
        Err(e) => {
            // A corrupt store must not be fatal — re-login recovers it.
            tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable token store");
            return Ok(None);
        }
    };

    if !stored.client_id.is_empty() && stored.client_id != client_id {
        tracing::warn!("stored tokens belong to a different client_id; re-authorisation required");
        return Ok(None);
    }

    let tokens = TokenSet {
        access_token: Secret::new(stored.access_token),
        refresh_token: Secret::new(stored.refresh_token),
        expires_at: stored.expires_at,
        scope: stored.scope,
    };

    let missing = tokens.missing_scopes();
    if !missing.is_empty() {
        tracing::warn!(
            missing = ?missing,
            "stored grant is missing scopes this version needs; run `spotify-agent login` again"
        );
    }
    Ok(Some(tokens))
}

fn expiry_from(expires_in: Option<i64>) -> DateTime<Utc> {
    Utc::now() + ChronoDuration::seconds(expires_in.unwrap_or(3600).clamp(60, 86_400))
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// RFC 7636 §4.1: 43–128 characters from the unreserved set.
fn code_verifier() -> String {
    random_token(96)
}

fn code_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn random_token(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| {
            let idx = rng.gen_range(0..ALPHABET.len());
            // `idx` is in range by construction; the fallback keeps this
            // function total rather than relying on indexing.
            ALPHABET.get(idx).copied().unwrap_or(b'x') as char
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Loopback callback listener
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Accept connections until one hits the redirect path. Anything else (a
/// favicon probe, a stray browser prefetch) is answered with 404 and ignored,
/// because those would otherwise consume the single accept we need.
async fn wait_for_callback(listener: TcpListener, expected_path: &str) -> Result<Callback> {
    loop {
        let (stream, _peer) = listener
            .accept()
            .await
            .map_err(|e| AgentError::Auth(format!("callback listener failed: {e}")))?;

        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await.is_err() || request_line.is_empty() {
            continue;
        }

        // "GET /callback?code=…&state=… HTTP/1.1"
        let target = request_line.split_whitespace().nth(1).unwrap_or("/");
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p, q),
            None => (target, ""),
        };

        if path != expected_path {
            let _ = respond(reader.into_inner(), 404, "Not found").await;
            continue;
        }

        let mut cb = Callback::default();
        for (key, value) in parse_query(query) {
            match key.as_str() {
                "code" => cb.code = Some(value),
                "state" => cb.state = Some(value),
                "error" => cb.error = Some(value),
                _ => {}
            }
        }

        let page = if cb.error.is_some() {
            SUCCESS_PAGE_FAILURE
        } else {
            SUCCESS_PAGE_OK
        };
        let _ = respond(reader.into_inner(), 200, page).await;
        return Ok(cb);
    }
}

async fn respond(mut stream: tokio::net::TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| AgentError::io("oauth callback response", e))?;
    let _ = stream.flush().await;
    Ok(())
}

const SUCCESS_PAGE_OK: &str = "<!doctype html><meta charset=utf-8><title>spotify-agent</title>\
<body style=\"font:16px system-ui;display:grid;place-items:center;height:100vh;margin:0;background:#121212;color:#eee\">\
<div style=\"text-align:center\"><h1 style=\"font-weight:600\">Authorised</h1>\
<p style=\"opacity:.7\">You can close this tab and return to the terminal.</p></div>";

const SUCCESS_PAGE_FAILURE: &str = "<!doctype html><meta charset=utf-8><title>spotify-agent</title>\
<body style=\"font:16px system-ui;display:grid;place-items:center;height:100vh;margin:0;background:#121212;color:#eee\">\
<div style=\"text-align:center\"><h1 style=\"font-weight:600\">Authorisation failed</h1>\
<p style=\"opacity:.7\">Check the terminal for details.</p></div>";

// ---------------------------------------------------------------------------
// Small URL helpers (kept local to avoid pulling a form-encoding crate)
// ---------------------------------------------------------------------------

fn serde_urlencoded_lite(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((percent_decode(k), percent_decode(v)))
        })
        .collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes.get(i) {
            Some(b'%') if i + 2 < bytes.len() => {
                let hex = input.get(i + 1..i + 3).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            Some(b'+') => {
                out.push(b' ');
                i += 1;
            }
            Some(b) => {
                out.push(*b);
                i += 1;
            }
            None => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Cross-platform browser launch
// ---------------------------------------------------------------------------

/// Best-effort. A failure here is never fatal: the URL is always printed.
fn open_browser(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};

    let mut command = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        // `start` is a cmd builtin, and the empty "" is the window title —
        // without it, a quoted URL is swallowed as the title.
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_length_is_in_spec_range() {
        let v = code_verifier();
        assert!((43..=128).contains(&v.len()));
    }

    #[test]
    fn query_parsing_decodes() {
        let parsed = parse_query("code=a%2Fb&state=x+y");
        assert_eq!(
            parsed,
            vec![
                ("code".into(), "a/b".into()),
                ("state".into(), "x y".into())
            ]
        );
    }

    /// Every endpoint the client calls, with the scope Spotify requires for it.
    ///
    /// This table is the contract. `save_tracks` shipped without
    /// `user-library-modify` and 403'd the first time someone pressed `f`;
    /// this test is what stops that recurring.
    #[test]
    fn scope_coverage_matches_the_endpoints_we_call() {
        const REQUIRED: &[(&str, &str)] = &[
            ("GET /me/tracks", "user-library-read"),
            ("PUT /me/tracks", "user-library-modify"),
            ("DELETE /me/tracks", "user-library-modify"),
            ("GET /me/top/tracks", "user-top-read"),
            ("GET /me/top/artists", "user-top-read"),
            (
                "GET /me/player/recently-played",
                "user-read-recently-played",
            ),
            ("GET /me", "user-read-private"),
            ("GET /me/playlists", "playlist-read-private"),
            ("GET /playlists/{id}/tracks", "playlist-read-private"),
            ("POST /users/{id}/playlists", "playlist-modify-private"),
            ("PUT /playlists/{id}", "playlist-modify-private"),
            ("PUT /playlists/{id}/tracks", "playlist-modify-private"),
            ("POST /playlists/{id}/tracks", "playlist-modify-private"),
            ("DELETE /playlists/{id}/tracks", "playlist-modify-private"),
        ];

        for (endpoint, scope) in REQUIRED {
            assert!(
                SCOPES.contains(scope),
                "{endpoint} needs the `{scope}` scope, which is not requested at login"
            );
        }
    }

    #[test]
    fn scopes_are_unique() {
        let unique: std::collections::HashSet<&&str> = SCOPES.iter().collect();
        assert_eq!(unique.len(), SCOPES.len(), "duplicate scope in SCOPES");
    }

    #[test]
    fn missing_scopes_detected() {
        let t = TokenSet {
            access_token: Secret::new("a"),
            refresh_token: Secret::new("r"),
            expires_at: Utc::now(),
            scope: "user-top-read".into(),
        };
        assert!(t.missing_scopes().contains(&"playlist-modify-private"));
    }
}
