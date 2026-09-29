//! Self-update: notice a new release, ask, then replace this binary.
//!
//! Three entry points, one mechanism:
//!   * `spotify-agent update` — explicit, interactive or `--yes`;
//!   * the TUI — a modal offering install / skip / later;
//!   * any other command on a terminal — one line on stderr, at most once per
//!     `check_interval_hours`.
//!
//! Design notes that are not obvious from the code:
//!
//! **The check never fails a command.** GitHub rate-limits unauthenticated
//! requests per IP, CI runners share addresses, and captive portals return
//! HTML for everything. A failed check is a debug log and nothing else — the
//! user asked to generate a playlist, not to talk to GitHub.
//!
//! **The prompt is suppressible and remembers.** `state::UpdateState` records
//! both the last check and the versions that were declined, so "no" means no
//! until something newer exists.
//!
//! **Nothing is downloaded until the user says so**, and nothing is installed
//! that does not match the release's published SHA-256 — see [`install`],
//! which also explains the ordering of the replacement itself.
//!
//! Unattended runs never prompt, and never install unless
//! `update.auto_install` is set: silently swapping the binary under a cron job
//! is the kind of helpfulness that produces 3 a.m. pages.

pub mod install;
pub mod state;
pub mod version;

use crate::config::{Config, UpdateConfig};
use crate::error::{AgentError, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use state::UpdateState;
use std::path::PathBuf;
use std::time::Duration;
use version::Version;

/// GitHub's API root. Overridable so the wire tests can point at a local
/// server without a network, and so a GHES mirror works.
const DEFAULT_API: &str = "https://api.github.com";

/// A published release, reduced to the fields that decide anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    pub url: String,
    pub notes: String,
    pub prerelease: bool,
    pub published_at: Option<DateTime<Utc>>,
}

impl Release {
    /// First few lines of the release notes, for a UI that has no room for
    /// the whole changelog.
    pub fn summary(&self, lines: usize) -> String {
        self.notes
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("```"))
            .take(lines)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// Turned off in config or by `SPOTIFY_AGENT_NO_UPDATE_CHECK`.
    Disabled,
    UpToDate {
        current: Version,
    },
    Available {
        release: Box<Release>,
        current: Version,
    },
}

impl Check {
    pub fn release(&self) -> Option<&Release> {
        match self {
            Check::Available { release, .. } => Some(release),
            _ => None,
        }
    }
}

// ===========================================================================
// The release feed
// ===========================================================================

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<DateTime<Utc>>,
}

impl GhRelease {
    fn into_release(self) -> Option<Release> {
        // A draft is not published: its assets are not downloadable by
        // anyone but the author, so offering it would be an update to a 404.
        if self.draft {
            return None;
        }
        let version = Version::parse(&self.tag_name)?;
        Some(Release {
            tag: self.tag_name,
            version,
            url: self.html_url,
            notes: self.body.unwrap_or_default(),
            prerelease: self.prerelease,
            published_at: self.published_at,
        })
    }
}

/// Pick the newest usable release from a `/releases` listing.
///
/// Separate from the HTTP call so the selection rules — drafts out,
/// pre-releases only when asked for, unparseable tags ignored — are testable
/// without a server.
pub fn newest(releases: Vec<GhReleaseJson>, include_prereleases: bool) -> Option<Release> {
    releases
        .into_iter()
        .filter_map(|r| r.0.into_release())
        .filter(|r| include_prereleases || !r.prerelease)
        .max_by(|a, b| a.version.cmp(&b.version))
}

/// Newtype so the deserialisation shape stays private while [`newest`] can
/// still be exercised directly by tests.
#[derive(Debug, Deserialize)]
pub struct GhReleaseJson(GhRelease);

// ===========================================================================
// Updater
// ===========================================================================

pub struct Updater {
    http: reqwest::Client,
    cfg: UpdateConfig,
    repo: String,
    api_base: String,
    asset_base: Option<String>,
    data_dir: PathBuf,
}

impl Updater {
    pub fn new(config: &Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("spotify-agent/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(config.update.timeout_secs.max(5)))
            .build()?;

        Ok(Self {
            http,
            repo: env_override("SPOTIFY_AGENT_REPO").unwrap_or_else(|| config.update.repo.clone()),
            api_base: env_override("SPOTIFY_AGENT_UPDATE_API")
                .unwrap_or_else(|| DEFAULT_API.to_string()),
            asset_base: env_override("SPOTIFY_AGENT_ASSET_BASE"),
            cfg: config.update.clone(),
            data_dir: config.data_dir()?,
        })
    }

    /// Point this updater at a different release host.
    ///
    /// Same meaning as `SPOTIFY_AGENT_ASSET_BASE` in `packaging/install.sh`:
    /// assets are fetched from `<asset_base>/<name>` with no tag path, which
    /// is what a mirror — or a test's local server — wants.
    pub fn with_endpoints(
        mut self,
        api_base: impl Into<String>,
        asset_base: impl Into<String>,
    ) -> Self {
        self.api_base = api_base.into();
        self.asset_base = Some(asset_base.into());
        self
    }

    pub fn state(&self) -> UpdateState {
        UpdateState::load(&self.data_dir)
    }

    /// Whether the automatic check is allowed to run at all.
    ///
    /// The environment variable is checked here rather than at load time so a
    /// packager can disable it for a system-wide install without editing
    /// anyone's config.
    pub fn auto_enabled(&self) -> bool {
        self.cfg.enabled && env_override("SPOTIFY_AGENT_NO_UPDATE_CHECK").is_none()
    }

    /// Whether enough time has passed since the last automatic check.
    pub fn is_due(&self) -> bool {
        self.state()
            .is_due(self.cfg.check_interval_hours, Utc::now())
    }

    pub fn auto_install(&self) -> bool {
        self.cfg.auto_install
    }

    /// Ask GitHub what the newest release is.
    ///
    /// `force` bypasses the interval — it is what the explicit `update`
    /// command uses, because a user who typed the command is entitled to an
    /// answer regardless of when the background check last ran.
    pub async fn check(&self, force: bool) -> Result<Check> {
        if !force && !self.auto_enabled() {
            return Ok(Check::Disabled);
        }
        let current = Version::current();

        let release = self.latest().await?;

        // Recorded even on the "nothing new" path: the point of the timestamp
        // is to rate-limit our own requests, not to log upgrades.
        let mut state = self.state();
        state.mark_checked(Utc::now(), release.as_ref().map(|r| r.tag.as_str()));
        if let Err(e) = state.save(&self.data_dir) {
            tracing::debug!(error = %e, "could not record the update check");
        }

        match release {
            Some(release) if release.version > current => Ok(Check::Available {
                release: Box::new(release),
                current,
            }),
            _ => Ok(Check::UpToDate { current }),
        }
    }

    /// The newest release, or `None` when the project has published none that
    /// this build understands.
    pub async fn latest(&self) -> Result<Option<Release>> {
        if self.cfg.include_prereleases {
            // `/releases/latest` excludes pre-releases by definition, so the
            // listing is the only way to see them.
            let url = format!("{}/repos/{}/releases?per_page=20", self.api_base, self.repo);
            let listing: Vec<GhReleaseJson> = self.get_json(&url).await?;
            return Ok(newest(listing, true));
        }

        let url = format!("{}/repos/{}/releases/latest", self.api_base, self.repo);
        let release: GhReleaseJson = self.get_json(&url).await?;
        Ok(release.0.into_release())
    }

    /// One specific tag, for `update --to`.
    pub async fn release_by_tag(&self, tag: &str) -> Result<Release> {
        let url = format!("{}/repos/{}/releases/tags/{tag}", self.api_base, self.repo);
        let release: GhReleaseJson = self.get_json(&url).await?;
        release
            .0
            .into_release()
            .ok_or_else(|| AgentError::other(format!("{tag} is a draft, or is not a version tag")))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let response = self
            .http
            .get(url)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            // 403 with a rate-limit body is GitHub's way of saying "too many
            // anonymous requests from this address" — retryable, and not the
            // user's fault, so it must not be reported as a config error.
            let retryable = status.as_u16() == 429
                || (status.as_u16() == 403 && message.contains("rate limit"));
            return Err(if retryable {
                AgentError::RateLimited {
                    service: "github",
                    retry_after: None,
                }
            } else {
                AgentError::Api {
                    service: "github",
                    status: status.as_u16(),
                    message: truncate(&message, 200),
                }
            });
        }

        response.json::<T>().await.map_err(Into::into)
    }

    // -------------------------------------------------------------------
    // Installing
    // -------------------------------------------------------------------

    /// Where this release's assets live.
    fn asset_base(&self, tag: &str) -> String {
        if let Some(base) = &self.asset_base {
            return base.clone();
        }
        // Addressed by tag rather than through `releases/latest/download`, so
        // that what gets installed is exactly the release that was inspected
        // even if a newer one appears mid-download.
        format!("https://github.com/{}/releases/download/{tag}", self.repo)
    }

    /// Download, verify and swap in the release binary.
    ///
    /// `progress` is called with (bytes so far, total if the server said).
    pub async fn install<F>(&self, release: &Release, progress: F) -> Result<PathBuf>
    where
        F: Fn(u64, Option<u64>) + Send + 'static,
    {
        let exe = install::current_exe()?;
        if let Some(reason) = install::blocked_reason(&exe) {
            return Err(AgentError::config(reason));
        }
        self.install_to(release, &exe, progress).await?;
        Ok(exe)
    }

    /// Install over a specific path.
    ///
    /// Split out from [`Self::install`] so the whole download → verify →
    /// extract → swap chain can be exercised against a real (if small) archive
    /// without a test replacing the test runner's own binary.
    pub async fn install_to<F>(
        &self,
        release: &Release,
        exe: &std::path::Path,
        progress: F,
    ) -> Result<()>
    where
        F: Fn(u64, Option<u64>) + Send + 'static,
    {
        let asset = install::asset_name().ok_or_else(|| {
            AgentError::other(format!(
                "no release binary is published for {}-{}; build from source instead \
                 (see the README)",
                std::env::consts::OS,
                std::env::consts::ARCH
            ))
        })?;

        let base = self.asset_base(&release.tag);
        let sums = if env_override("SPOTIFY_AGENT_SKIP_VERIFY").is_some() {
            tracing::warn!("SPOTIFY_AGENT_SKIP_VERIFY is set — the download will not be verified");
            None
        } else {
            Some(self.fetch_text(&format!("{base}/SHA256SUMS")).await?)
        };

        let archive = self.download(&format!("{base}/{asset}"), progress).await?;

        let expected = release.version.to_string();
        let target = exe.to_path_buf();
        // Everything from here is filesystem work plus one subprocess.
        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(sums) = sums {
                install::verify_checksum(&sums, &asset, &archive)?;
            }
            let binary = install::extract_binary(&archive)?;
            let staged = install::stage(&target, &binary)?;
            install::smoke_test(staged.path(), &expected)?;
            install::swap(&target, staged)
        })
        .await
        .map_err(|e| AgentError::other(format!("the install task did not finish: {e}")))?
    }

    async fn fetch_text(&self, url: &str) -> Result<String> {
        let response = self.http.get(url).send().await?;
        if !response.status().is_success() {
            return Err(AgentError::Api {
                service: "github",
                status: response.status().as_u16(),
                message: format!("could not fetch {url}"),
            });
        }
        Ok(response.text().await?)
    }

    async fn download<F>(&self, url: &str, progress: F) -> Result<Vec<u8>>
    where
        F: Fn(u64, Option<u64>),
    {
        use futures_util::StreamExt;

        let response = self.http.get(url).send().await?;
        if !response.status().is_success() {
            return Err(AgentError::Api {
                service: "github",
                status: response.status().as_u16(),
                message: format!("could not download {url}"),
            });
        }

        let total = response.content_length();
        let mut bytes = Vec::with_capacity(total.unwrap_or(8 * 1024 * 1024) as usize);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
            progress(bytes.len() as u64, total);
        }
        Ok(bytes)
    }

    // -------------------------------------------------------------------
    // Remembering
    // -------------------------------------------------------------------

    /// Record "not this version", so the prompt stops until a newer one.
    pub fn skip(&self, tag: &str) -> Result<()> {
        let mut state = self.state();
        state.skip(tag);
        state.save(&self.data_dir)
    }

    pub fn was_skipped(&self, release: &Release) -> bool {
        self.state().is_skipped(&release.version)
    }
}

fn env_override(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    match trimmed.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &trimmed[..idx]),
        None => trimmed.to_string(),
    }
}

// ===========================================================================
// The passive notice
// ===========================================================================

/// One line for stderr after an ordinary command, or `None`.
///
/// Returns `None` for every uninteresting case — disabled, not due, up to
/// date, declined, or the check failed — so the caller has no policy to
/// implement and cannot accidentally turn a network hiccup into output.
pub async fn passive_notice(config: &Config) -> Option<String> {
    let updater = Updater::new(config).ok()?;
    if !updater.auto_enabled() || !updater.is_due() {
        return None;
    }

    let check = match updater.check(false).await {
        Ok(check) => check,
        Err(e) => {
            tracing::debug!(error = %e, "update check failed");
            return None;
        }
    };

    let Check::Available { release, current } = check else {
        return None;
    };
    if updater.was_skipped(&release) {
        return None;
    }

    Some(format!(
        "update available: {current} → {} · run `spotify-agent update`",
        release.version
    ))
}

/// The unattended path: log what is available, and install it only if the
/// config explicitly asked for that.
///
/// Deliberately silent about failures at anything above debug level. A cron
/// run that could not reach GitHub still generated the playlist it was asked
/// for, and a warning on every run trains people to ignore the log.
pub async fn unattended(config: &Config) {
    let Ok(updater) = Updater::new(config) else {
        return;
    };
    if !updater.auto_enabled() || !updater.is_due() {
        return;
    }

    match updater.check(false).await {
        Ok(Check::Available { release, current }) => {
            tracing::info!(
                current = %current,
                latest = %release.version,
                url = %release.url,
                "a newer release is available"
            );
            if !updater.auto_install() {
                return;
            }
            match updater.install(&release, |_, _| {}).await {
                Ok(path) => tracing::info!(
                    version = %release.version,
                    path = %path.display(),
                    "installed the new release"
                ),
                Err(e) => tracing::warn!(error = %e, "could not install the new release"),
            }
        }
        Ok(_) => {}
        Err(e) => tracing::debug!(error = %e, "update check failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(raw: &str) -> Vec<GhReleaseJson> {
        serde_json::from_str(raw).expect("parses")
    }

    #[test]
    fn drafts_are_never_offered() {
        // Their assets are not downloadable, so an "update" to one is a 404.
        let releases =
            listing(r#"[{"tag_name":"v9.9.9","draft":true},{"tag_name":"v1.0.0","draft":false}]"#);
        let picked = newest(releases, true).expect("a release");
        assert_eq!(picked.tag, "v1.0.0");
    }

    #[test]
    fn prereleases_are_opt_in() {
        let raw = r#"[{"tag_name":"v2.0.0-rc.1","prerelease":true},{"tag_name":"v1.9.0"}]"#;
        assert_eq!(
            newest(listing(raw), false).expect("a release").tag,
            "v1.9.0"
        );
        assert_eq!(
            newest(listing(raw), true).expect("a release").tag,
            "v2.0.0-rc.1"
        );
    }

    #[test]
    fn a_tag_that_is_not_a_version_is_ignored_not_fatal() {
        // Repositories collect tags like `nightly` and `legacy`; one of those
        // must not stop the newest real release being found.
        let raw = r#"[{"tag_name":"nightly"},{"tag_name":"v1.2.3"},{"tag_name":"legacy"}]"#;
        assert_eq!(
            newest(listing(raw), false).expect("a release").tag,
            "v1.2.3"
        );
    }

    #[test]
    fn the_newest_is_chosen_by_version_not_by_listing_order() {
        // GitHub orders by creation date; a patch backport to an old branch
        // can be created after a newer minor and would win a naive `first()`.
        let raw = r#"[{"tag_name":"v0.9.1"},{"tag_name":"v0.10.0"},{"tag_name":"v0.9.2"}]"#;
        assert_eq!(
            newest(listing(raw), false).expect("a release").tag,
            "v0.10.0"
        );
    }

    #[test]
    fn an_empty_release_list_is_not_an_error() {
        assert!(newest(listing("[]"), true).is_none());
    }

    #[test]
    fn release_notes_are_summarised_without_fences_or_blank_lines() {
        let release = Release {
            tag: "v1.0.0".into(),
            version: Version::parse("1.0.0").expect("parses"),
            url: String::new(),
            notes: "## What's new\n\n- faster sync\n\n```sh\ncode\n```\n- fewer bugs".into(),
            prerelease: false,
            published_at: None,
        };
        assert_eq!(release.summary(3), "## What's new\n- faster sync\ncode");
    }
}
