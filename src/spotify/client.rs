//! Async Spotify Web API client.
//!
//! Hand-rolled over `reqwest` rather than delegating to `rspotify`, for three
//! reasons that matter in production:
//!   * rate limiting: Spotify's 429 carries a `Retry-After` that can be
//!     minutes long, and it must be obeyed exactly — this client funnels every
//!     request through the one shared [`RetryPolicy`];
//!   * token lifecycle: refresh is serialised across concurrent requests, and
//!     a 401 invalidates the cached token and retries once;
//!   * failure surface: every endpoint returns the crate's `Result`, so the
//!     pipeline has a single error vocabulary.

use crate::config::SpotifyConfig;
use crate::domain::{Artist, PlayEvent, Playlist, TimeRange, Track};
use crate::error::{AgentError, Result};
use crate::spotify::auth::Authenticator;
use crate::spotify::models::{self, *};
use crate::util::retry::{RetryPolicy, parse_retry_after, with_retry};
use futures_util::{StreamExt, stream};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

const SERVICE: &str = "spotify";

/// Endpoint page sizes. These are Spotify's documented maxima; using anything
/// smaller just multiplies requests against the same rate limit.
const PAGE_LIMIT: usize = 50;
const PLAYLIST_PAGE_LIMIT: usize = 50;
/// `PUT/POST /playlists/{id}/tracks` accepts at most 100 URIs per call.
const PLAYLIST_WRITE_CHUNK: usize = 100;
/// `GET /artists` accepts at most 50 ids per call.
const ARTIST_CHUNK: usize = 50;

pub struct SpotifyClient {
    http: reqwest::Client,
    auth: Arc<Authenticator>,
    api_base: String,
    market: Option<String>,
    policy: RetryPolicy,
    concurrency: usize,
}

impl SpotifyClient {
    pub fn new(cfg: &SpotifyConfig, http: reqwest::Client, auth: Arc<Authenticator>) -> Self {
        Self {
            http,
            auth,
            api_base: cfg.api_base.trim_end_matches('/').to_string(),
            market: cfg.market.clone(),
            policy: RetryPolicy {
                max_attempts: cfg.max_retries.max(1),
                ..Default::default()
            },
            concurrency: cfg.concurrency.clamp(1, 16),
        }
    }

    // -----------------------------------------------------------------
    // Transport
    // -----------------------------------------------------------------

    fn url(&self, path: &str) -> String {
        if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{}", self.api_base, path)
        }
    }

    /// One request, with auth, retry, and structured error mapping.
    ///
    /// `body` is serialised as JSON when present. `T = ()` is not usable for
    /// empty responses — use [`Self::send_no_content`] for those.
    async fn send<T, B>(&self, method: Method, path: &str, body: Option<&B>) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let raw = self.send_raw(method, path, body).await?;
        if raw.trim().is_empty() {
            return Err(AgentError::Api {
                service: SERVICE,
                status: 200,
                message: "expected a JSON body but the response was empty".into(),
            });
        }
        serde_json::from_str(&raw).map_err(|e| AgentError::Api {
            service: SERVICE,
            status: 200,
            message: format!("malformed response from {path}: {e}"),
        })
    }

    async fn send_no_content<B>(&self, method: Method, path: &str, body: Option<&B>) -> Result<()>
    where
        B: Serialize + ?Sized,
    {
        self.send_raw(method, path, body).await.map(|_| ())
    }

    async fn send_raw<B>(&self, method: Method, path: &str, body: Option<&B>) -> Result<String>
    where
        B: Serialize + ?Sized,
    {
        let url = self.url(path);

        with_retry(SERVICE, self.policy, |attempt| {
            let http = self.http.clone();
            let auth = Arc::clone(&self.auth);
            let url = url.clone();
            let method = method.clone();
            let payload = body.map(serde_json::to_vec);

            async move {
                let payload = payload.transpose()?;
                let token = auth.access_token().await?;

                let mut req = http
                    .request(method, &url)
                    .bearer_auth(token.expose())
                    .header(reqwest::header::ACCEPT, "application/json");
                if let Some(bytes) = payload {
                    req = req
                        .header(reqwest::header::CONTENT_TYPE, "application/json")
                        .body(bytes);
                }

                let response = req.send().await?;
                let status = response.status();
                let retry_after =
                    parse_retry_after(header_str(response.headers().get("retry-after")).as_deref());
                let text = response.text().await.unwrap_or_default();

                if status.is_success() {
                    return Ok(text);
                }

                let message = extract_error_message(&text, status);

                match status {
                    StatusCode::TOO_MANY_REQUESTS => Err(AgentError::RateLimited {
                        service: SERVICE,
                        retry_after,
                    }),
                    // A 401 after our proactive refresh means the cached token
                    // was revoked out from under us. Drop it and let the retry
                    // loop mint a new one — exactly once.
                    StatusCode::UNAUTHORIZED if attempt == 1 => {
                        auth.invalidate().await;
                        Err(AgentError::Transient {
                            service: SERVICE,
                            status: 401,
                            message: "access token rejected; refreshing".into(),
                        })
                    }
                    StatusCode::UNAUTHORIZED => Err(AgentError::NotAuthorized),
                    StatusCode::FORBIDDEN => Err(AgentError::Api {
                        service: SERVICE,
                        status: 403,
                        message: format!(
                            "{message} (this usually means a missing OAuth scope — run `spotify-agent login` again)"
                        ),
                    }),
                    s if s.is_server_error() => Err(AgentError::Transient {
                        service: SERVICE,
                        status: s.as_u16(),
                        message,
                    }),
                    s => Err(AgentError::Api {
                        service: SERVICE,
                        status: s.as_u16(),
                        message,
                    }),
                }
            }
        })
        .await
    }

    /// Walk an offset-paged collection to exhaustion (or `max_items`).
    async fn paginate<T: DeserializeOwned>(
        &self,
        first_path: String,
        max_items: Option<usize>,
    ) -> Result<Vec<T>> {
        let mut out: Vec<T> = Vec::new();
        let mut next = Some(first_path);

        while let Some(path) = next {
            let page: Page<T> = self.send::<_, ()>(Method::GET, &path, None).await?;
            out.extend(page.items);
            if let Some(max) = max_items {
                if out.len() >= max {
                    out.truncate(max);
                    break;
                }
            }
            next = page.next;
        }
        Ok(out)
    }

    fn market_param(&self, sep: char) -> String {
        match &self.market {
            Some(m) => format!("{sep}market={m}"),
            None => String::new(),
        }
    }

    // -----------------------------------------------------------------
    // Profile & library
    // -----------------------------------------------------------------

    pub async fn current_user(&self) -> Result<UserObject> {
        self.send::<_, ()>(Method::GET, "/me", None).await
    }

    /// Liked Songs, newest first.
    pub async fn saved_tracks(&self, max_items: Option<usize>) -> Result<Vec<Track>> {
        let path = format!("/me/tracks?limit={PAGE_LIMIT}{}", self.market_param('&'));
        let items: Vec<SavedTrackItem> = self.paginate(path, max_items).await?;
        Ok(items
            .into_iter()
            .filter_map(|i| i.track.and_then(TrackObject::into_domain))
            .collect())
    }

    /// `/me/top/tracks`. Spotify caps this at 50 per window regardless of
    /// pagination, so `max_items` above 50 has no effect.
    pub async fn top_tracks(&self, range: TimeRange) -> Result<Vec<Track>> {
        let path = format!(
            "/me/top/tracks?time_range={}&limit={PAGE_LIMIT}",
            range.as_api()
        );
        let items: Vec<TrackObject> = self.paginate(path, Some(PAGE_LIMIT)).await?;
        Ok(items
            .into_iter()
            .filter_map(TrackObject::into_domain)
            .collect())
    }

    pub async fn top_artists(&self, range: TimeRange) -> Result<Vec<Artist>> {
        let path = format!(
            "/me/top/artists?time_range={}&limit={PAGE_LIMIT}",
            range.as_api()
        );
        let items: Vec<ArtistObject> = self.paginate(path, Some(PAGE_LIMIT)).await?;
        Ok(items.into_iter().map(Artist::from).collect())
    }

    /// `/me/player/recently-played`. Spotify retains only the last 50 events,
    /// which is precisely why this agent keeps its own SQLite history.
    ///
    /// `after_ms` is a Unix-millis cursor: pass the newest timestamp already
    /// stored to fetch only what is new.
    pub async fn recently_played(
        &self,
        after_ms: Option<i64>,
    ) -> Result<(Vec<Track>, Vec<PlayEvent>)> {
        let mut path = format!("/me/player/recently-played?limit={PAGE_LIMIT}");
        if let Some(after) = after_ms {
            path.push_str(&format!("&after={after}"));
        }

        let mut items: Vec<PlayHistoryItem> = Vec::new();
        let mut next = Some(path);
        // Bounded: the endpoint is cursor-paged and can, in pathological
        // cases, keep handing back a `next`. Five pages is far more than the
        // 50-item retention window can hold.
        for _ in 0..5 {
            let Some(path) = next.take() else { break };
            let page: CursorPage<PlayHistoryItem> =
                self.send::<_, ()>(Method::GET, &path, None).await?;
            let empty = page.items.is_empty();
            items.extend(page.items);
            if empty {
                break;
            }
            next = page.next;
        }

        Ok(models::play_events(items))
    }

    /// Hydrate artists (for genres). Chunked at 50 ids per request.
    pub async fn artists(&self, ids: &[String]) -> Result<Vec<Artist>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(ARTIST_CHUNK) {
            let path = format!("/artists?ids={}", chunk.join(","));
            let response: ArtistsResponse = self.send::<_, ()>(Method::GET, &path, None).await?;
            out.extend(response.artists.into_iter().flatten().map(Artist::from));
        }
        Ok(out)
    }

    // -----------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------

    pub async fn search_tracks(&self, query: &str, limit: usize) -> Result<Vec<Track>> {
        let path = format!(
            "/search?type=track&limit={}&q={}{}",
            limit.clamp(1, PAGE_LIMIT),
            urlencode(query),
            self.market_param('&')
        );
        let response: SearchResponse = self.send::<_, ()>(Method::GET, &path, None).await?;
        Ok(response
            .tracks
            .map(|p| p.items)
            .unwrap_or_default()
            .into_iter()
            .filter_map(TrackObject::into_domain)
            .collect())
    }

    /// Run many searches with bounded concurrency, preserving input order.
    ///
    /// Order is preserved because the caller pairs results back with the
    /// model's suggestions positionally, and `buffer_unordered` alone would
    /// scramble that.
    pub async fn search_many(&self, queries: Vec<String>, limit: usize) -> Vec<Result<Vec<Track>>> {
        let mut results: Vec<Option<Result<Vec<Track>>>> =
            (0..queries.len()).map(|_| None).collect();

        let mut stream = stream::iter(queries.into_iter().enumerate())
            .map(|(idx, q)| async move { (idx, self.search_tracks(&q, limit).await) })
            .buffer_unordered(self.concurrency);

        while let Some((idx, result)) = stream.next().await {
            if let Some(slot) = results.get_mut(idx) {
                *slot = Some(result);
            }
        }

        results
            .into_iter()
            .map(|r| r.unwrap_or_else(|| Err(AgentError::other("search slot never filled"))))
            .collect()
    }

    // -----------------------------------------------------------------
    // Playlists
    // -----------------------------------------------------------------

    pub async fn user_playlists(&self) -> Result<Vec<Playlist>> {
        let path = format!("/me/playlists?limit={PAGE_LIMIT}");
        let items: Vec<PlaylistObject> = self.paginate(path, None).await?;
        Ok(items.into_iter().map(Playlist::from).collect())
    }

    /// Find a playlist owned by `owner_id` whose name matches exactly
    /// (case-insensitively). Ownership matters: the user may follow someone
    /// else's playlist with the same name, and we must never try to write to it.
    pub async fn find_playlist(&self, owner_id: &str, name: &str) -> Result<Option<Playlist>> {
        let playlists = self.user_playlists().await?;
        Ok(playlists
            .into_iter()
            .find(|p| p.owner_id == owner_id && p.name.eq_ignore_ascii_case(name)))
    }

    pub async fn create_playlist(
        &self,
        user_id: &str,
        name: &str,
        description: &str,
        public: bool,
    ) -> Result<Playlist> {
        #[derive(Serialize)]
        struct Body<'a> {
            name: &'a str,
            description: &'a str,
            public: bool,
        }
        let body = Body {
            name,
            // Spotify silently truncates past 300 chars; do it deliberately on
            // a char boundary instead of sending something it will mangle.
            description: &truncate_chars(description, 300),
            public,
        };
        let obj: PlaylistObject = self
            .send(
                Method::POST,
                &format!("/users/{}/playlists", urlencode(user_id)),
                Some(&body),
            )
            .await?;
        Ok(Playlist::from(obj))
    }

    pub async fn update_playlist_details(
        &self,
        playlist_id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            name: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            description: Option<String>,
        }
        if name.is_none() && description.is_none() {
            return Ok(());
        }
        let body = Body {
            name,
            description: description.map(|d| truncate_chars(d, 300)),
        };
        self.send_no_content(
            Method::PUT,
            &format!("/playlists/{playlist_id}"),
            Some(&body),
        )
        .await
    }

    pub async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Track>> {
        // `fields` trims the payload substantially — a 500-track playlist is
        // several MB of JSON without it.
        let path = format!(
            "/playlists/{playlist_id}/tracks?limit={PLAYLIST_PAGE_LIMIT}\
             &fields=next,items(track(id,name,duration_ms,popularity,explicit,is_local,artists(id,name),album(name,release_date,release_date_precision),external_ids(isrc))){}",
            self.market_param('&')
        );
        let items: Vec<PlaylistTrackItem> = self.paginate(path, None).await?;
        Ok(items
            .into_iter()
            .filter_map(|i| i.track.and_then(TrackObject::into_domain))
            .collect())
    }

    /// Replace the playlist contents. The first chunk uses `PUT` (which
    /// clears), subsequent chunks use `POST` (which appends) — that is the
    /// documented way to write more than 100 tracks atomically enough.
    pub async fn replace_playlist_tracks(&self, playlist_id: &str, uris: &[String]) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            uris: &'a [String],
        }

        let mut chunks = uris.chunks(PLAYLIST_WRITE_CHUNK);
        let first = chunks.next().unwrap_or(&[]);
        self.send_no_content(
            Method::PUT,
            &format!("/playlists/{playlist_id}/tracks"),
            Some(&Body { uris: first }),
        )
        .await?;

        for chunk in chunks {
            self.send_no_content(
                Method::POST,
                &format!("/playlists/{playlist_id}/tracks"),
                Some(&Body { uris: chunk }),
            )
            .await?;
        }
        Ok(())
    }

    /// Remove specific tracks from a playlist (all occurrences).
    pub async fn remove_playlist_tracks(&self, playlist_id: &str, uris: &[String]) -> Result<()> {
        #[derive(Serialize)]
        struct Entry<'a> {
            uri: &'a str,
        }
        #[derive(Serialize)]
        struct Body<'a> {
            tracks: Vec<Entry<'a>>,
        }
        for chunk in uris.chunks(PLAYLIST_WRITE_CHUNK) {
            let body = Body {
                tracks: chunk.iter().map(|uri| Entry { uri }).collect(),
            };
            let _: SnapshotResponse = self
                .send(
                    Method::DELETE,
                    &format!("/playlists/{playlist_id}/tracks"),
                    Some(&body),
                )
                .await?;
        }
        Ok(())
    }

    /// Add tracks to Liked Songs. Max 50 ids per request.
    pub async fn save_tracks(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(PAGE_LIMIT) {
            self.send_no_content::<()>(
                Method::PUT,
                &format!("/me/tracks?ids={}", chunk.join(",")),
                None,
            )
            .await?;
        }
        Ok(())
    }

    /// Remove tracks from Liked Songs.
    pub async fn unsave_tracks(&self, ids: &[String]) -> Result<()> {
        for chunk in ids.chunks(PAGE_LIMIT) {
            self.send_no_content::<()>(
                Method::DELETE,
                &format!("/me/tracks?ids={}", chunk.join(",")),
                None,
            )
            .await?;
        }
        Ok(())
    }

    pub async fn add_playlist_tracks(&self, playlist_id: &str, uris: &[String]) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            uris: &'a [String],
        }
        for chunk in uris.chunks(PLAYLIST_WRITE_CHUNK) {
            let _: SnapshotResponse = self
                .send(
                    Method::POST,
                    &format!("/playlists/{playlist_id}/tracks"),
                    Some(&Body { uris: chunk }),
                )
                .await?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn header_str(value: Option<&reqwest::header::HeaderValue>) -> Option<String> {
    value.and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// Pull the human-readable message out of Spotify's error envelope without
/// ever echoing an unparsed body (which may contain request context).
fn extract_error_message(body: &str, status: StatusCode) -> String {
    serde_json::from_str::<ApiErrorEnvelope>(body)
        .ok()
        .and_then(|e| e.error.message)
        .or_else(|| {
            serde_json::from_str::<OAuthErrorEnvelope>(body)
                .ok()
                .map(|e| e.error_description.unwrap_or(e.error))
        })
        .unwrap_or_else(|| {
            status
                .canonical_reason()
                .unwrap_or("request failed")
                .to_string()
        })
}

fn truncate_chars(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    input
        .chars()
        .take(max.saturating_sub(1))
        .collect::<String>()
        + "…"
}

/// Percent-encode a query-string value. Spotify's search syntax uses `:` and
/// `"`, both of which must survive as encoded bytes.
fn urlencode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 2);
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapes_search_syntax() {
        assert_eq!(urlencode("track:\"a b\""), "track%3A%22a%20b%22");
    }

    #[test]
    fn truncate_is_char_safe() {
        let s = "ю".repeat(400);
        let t = truncate_chars(&s, 300);
        assert_eq!(t.chars().count(), 300);
    }

    #[test]
    fn error_message_falls_back_to_status_text() {
        let msg = extract_error_message("<html>oops</html>", StatusCode::BAD_GATEWAY);
        assert_eq!(msg, "Bad Gateway");
    }
}
