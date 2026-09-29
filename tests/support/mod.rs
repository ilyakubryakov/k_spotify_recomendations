//! A minimal scripted HTTP server for wire-level tests.
//!
//! Deliberately hand-rolled rather than pulling in a mocking framework: the
//! whole point of these tests is to assert on the exact bytes we send and to
//! control the exact bytes we receive — SSE framing, `Retry-After`, truncated
//! pages — and a framework would sit between the assertion and the wire.

// Integration tests are their own crate, so the lib's `cfg_attr(test)` lint
// relaxations do not apply here. Assertions legitimately index and unwrap.
#![allow(dead_code)]
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Recorded {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Bytes, not a String: release archives are gzip, and a test that has to
    /// serve one must be able to serve exactly the bytes it built.
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into().into_bytes(),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: body.into().into_bytes(),
        }
    }

    pub fn bytes(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/octet-stream".into())],
            body,
        }
    }

    /// Server-sent events body.
    pub fn sse(frames: &[&str]) -> Self {
        let body = frames
            .iter()
            .map(|f| format!("event: x\ndata: {f}\n\n"))
            .collect::<String>();
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: body.into_bytes(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

pub struct MockServer {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockServer {
    /// Serve `replies` in order. Once only one remains it repeats, so a test
    /// only has to script the interesting prefix.
    pub async fn start(replies: Vec<Reply>) -> Self {
        Self::start_with(|_| replies).await
    }

    /// Like [`Self::start`], but the replies are built *after* the port is
    /// known. Needed whenever a response body has to embed the server's own
    /// URL — Spotify's `next` pagination link, for instance.
    pub async fn start_with<F>(build: F) -> Self
    where
        F: FnOnce(&str) -> Vec<Reply>,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let replies = build(&format!("http://{addr}"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(replies)));

        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let queue = Arc::clone(&queue);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        let Ok(n) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(idx) = find(&buf, b"\r\n\r\n") {
                            break idx + 4;
                        }
                    };

                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let mut lines = head.lines();
                    let request_line = lines.next().unwrap_or_default().to_string();
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let path = parts.next().unwrap_or_default().to_string();

                    let mut headers = HashMap::new();
                    for line in lines {
                        if let Some((k, v)) = line.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }

                    let len: usize = headers
                        .get("content-length")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let mut body = buf[header_end..].to_vec();
                    while body.len() < len {
                        let Ok(n) = stream.read(&mut chunk).await else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
                        body.extend_from_slice(&chunk[..n]);
                    }

                    if let Ok(mut guard) = recorded.lock() {
                        guard.push(Recorded {
                            method,
                            path,
                            headers,
                            body: String::from_utf8_lossy(&body).to_string(),
                        });
                    }

                    let reply = {
                        let Ok(mut q) = queue.lock() else { return };
                        if q.len() > 1 {
                            q.pop_front()
                        } else {
                            q.front().cloned()
                        }
                    }
                    .unwrap_or_else(|| Reply::json(200, "{}"));

                    let mut response = format!("HTTP/1.1 {} X\r\n", reply.status);
                    for (name, value) in &reply.headers {
                        response.push_str(&format!("{name}: {value}\r\n"));
                    }
                    response.push_str(&format!(
                        "content-length: {}\r\nconnection: close\r\n\r\n",
                        reply.body.len()
                    ));
                    let mut response = response.into_bytes();
                    response.extend_from_slice(&reply.body);
                    let _ = stream.write_all(&response).await;
                    let _ = stream.flush().await;
                });
            }
        });

        Self { addr, requests }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().map(|g| g.len()).unwrap_or(0)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Scratch directory that removes itself on drop.
pub struct TempDir(pub std::path::PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let mut dir = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        dir.push(format!(
            "spotify-agent-{tag}-{unique}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }
    pub fn join(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Write a token store that is valid for an hour, so the client under test
/// never tries to refresh.
pub fn write_valid_tokens(path: &std::path::Path, client_id: &str) {
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(1);
    let json = serde_json::json!({
        "version": 1,
        "access_token": "test-access-token",
        "refresh_token": "test-refresh-token",
        "expires_at": expires_at.to_rfc3339(),
        "scope": spotify_agent::spotify::auth::SCOPES.join(" "),
        "client_id": client_id,
    });
    std::fs::write(path, serde_json::to_vec_pretty(&json).expect("serialize")).expect("write");
}
