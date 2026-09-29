//! Wire-level tests for the self-updater.
//!
//! The interesting behaviour is not "does it parse JSON" — it is what happens
//! when the network lies: a rate-limited API, a checksum that does not match,
//! an archive that 404s. Each of those has a required outcome, and each of
//! them is a way to end up with a broken install if it is handled loosely.

// Integration tests are their own crate, so the lib's `cfg_attr(test)` lint
// relaxations do not apply here.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

mod support;

use spotify_agent::config::Config;
use spotify_agent::update::{Check, Updater};
use support::{MockServer, Reply, TempDir};

/// A config whose data directory is disposable, so the update-state file a
/// check writes does not leak into the developer's real one.
fn config_in(dir: &TempDir) -> Config {
    let mut config = Config::default();
    config.general.data_dir = Some(dir.0.clone());
    config.update.repo = "someone/spotify-agent".into();
    config
}

fn updater(config: &Config, server: &MockServer) -> Updater {
    Updater::new(config)
        .expect("updater")
        .with_endpoints(server.base_url(), server.base_url())
}

fn release_json(tag: &str) -> String {
    format!(
        r#"{{"tag_name":"{tag}","html_url":"https://example.invalid/{tag}",
            "body":"- something changed","draft":false,"prerelease":false}}"#
    )
}

// ===========================================================================
// Checking
// ===========================================================================

#[tokio::test]
async fn the_check_asks_github_for_the_latest_release() {
    let server = MockServer::start(vec![Reply::json(200, release_json("v99.0.0"))]).await;
    let dir = TempDir::new("update-check");
    let config = config_in(&dir);

    let check = updater(&config, &server).check(true).await.expect("checks");

    let Check::Available { release, current } = check else {
        panic!("expected an update to be available, got {check:?}");
    };
    assert_eq!(release.tag, "v99.0.0");
    assert!(release.version > current);

    let request = &server.requests()[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/repos/someone/spotify-agent/releases/latest");
    // GitHub asks for both of these, and answers unversioned requests with
    // whatever the API happens to be doing that week.
    assert_eq!(
        request.header("accept"),
        Some("application/vnd.github+json")
    );
    assert_eq!(request.header("x-github-api-version"), Some("2022-11-28"));
    assert!(
        request
            .header("user-agent")
            .is_some_and(|ua| ua.starts_with("spotify-agent/")),
        "GitHub rejects requests with no User-Agent"
    );
}

#[tokio::test]
async fn an_older_release_is_not_offered_as_an_update() {
    // The failure this guards against is an update loop: offering 0.0.1 to a
    // 0.1.2 install, installing it, and offering it again forever.
    let server = MockServer::start(vec![Reply::json(200, release_json("v0.0.1"))]).await;
    let dir = TempDir::new("update-older");
    let config = config_in(&dir);

    let check = updater(&config, &server).check(true).await.expect("checks");
    assert!(matches!(check, Check::UpToDate { .. }), "got {check:?}");
}

#[tokio::test]
async fn a_checked_release_is_not_checked_again_until_the_interval_elapses() {
    let server = MockServer::start(vec![Reply::json(200, release_json("v99.0.0"))]).await;
    let dir = TempDir::new("update-interval");
    let config = config_in(&dir);
    let updater = updater(&config, &server);

    assert!(updater.is_due(), "a fresh install has never checked");
    updater.check(true).await.expect("checks");
    assert!(
        !updater.is_due(),
        "the timestamp must be recorded, or every command would hit the API"
    );
}

#[tokio::test]
async fn being_rate_limited_is_retryable_rather_than_an_error_in_the_config() {
    // GitHub answers anonymous abuse with 403, not 429. Classifying that as a
    // permanent API error would tell the user to fix something that is fine.
    let server = MockServer::start(vec![Reply::json(
        403,
        r#"{"message":"API rate limit exceeded for 1.2.3.4."}"#,
    )])
    .await;
    let dir = TempDir::new("update-429");
    let config = config_in(&dir);

    let error = updater(&config, &server)
        .check(true)
        .await
        .expect_err("rate limited");
    assert!(error.is_retryable(), "got {error}");
}

#[tokio::test]
async fn a_declined_version_stops_being_offered() {
    let server = MockServer::start(vec![Reply::json(200, release_json("v99.0.0"))]).await;
    let dir = TempDir::new("update-skip");
    let config = config_in(&dir);
    let updater = updater(&config, &server);

    let check = updater.check(true).await.expect("checks");
    let release = check.release().expect("a release").clone();
    assert!(!updater.was_skipped(&release));

    updater.skip(&release.tag).expect("records the decision");
    assert!(updater.was_skipped(&release));
}

// ===========================================================================
// Installing
// ===========================================================================

/// The whole chain — SHA256SUMS, archive, extract, smoke test, swap — against
/// a real archive. Unix only: the smoke test runs the downloaded file, and a
/// shell script is the cheapest thing that can plausibly answer `--version`.
#[cfg(unix)]
mod installing {
    use super::*;
    use sha2::{Digest, Sha256};
    use spotify_agent::update::install;
    use std::os::unix::fs::PermissionsExt;

    const NEW_VERSION: &str = "99.0.0";

    /// A tar.gz shaped exactly like the release workflow's, containing a
    /// "binary" that answers `--version` the way the real one does.
    fn release_archive() -> Vec<u8> {
        let script = format!("#!/bin/sh\necho 'spotify-agent {NEW_VERSION}'\n");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(script.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("spotify-agent-fake/{}", install::BINARY_NAME),
                script.as_bytes(),
            )
            .expect("append");
        builder
            .into_inner()
            .and_then(|encoder| encoder.finish())
            .expect("finish")
    }

    fn sums_for(archive: &[u8], asset: &str) -> String {
        let digest = Sha256::digest(archive);
        let hex = digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        format!("{hex}  {asset}\n")
    }

    fn existing_binary(dir: &TempDir) -> std::path::PathBuf {
        let exe = dir.join("spotify-agent");
        std::fs::write(&exe, b"the old binary").expect("write");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        exe
    }

    fn release(tag: &str) -> spotify_agent::update::Release {
        spotify_agent::update::Release {
            tag: tag.into(),
            version: spotify_agent::update::version::Version::parse(tag).expect("parses"),
            url: String::new(),
            notes: String::new(),
            prerelease: false,
            published_at: None,
        }
    }

    #[tokio::test]
    async fn a_verified_release_replaces_the_binary_in_place() {
        let Some(asset) = install::asset_name() else {
            return; // no published asset for this platform; nothing to install
        };
        let archive = release_archive();
        let server = MockServer::start(vec![
            Reply::text(200, sums_for(&archive, &asset)),
            Reply::bytes(200, archive),
        ])
        .await;

        let dir = TempDir::new("update-install");
        let config = config_in(&dir);
        let exe = existing_binary(&dir);

        let seen = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = std::sync::Arc::clone(&seen);
        updater(&config, &server)
            .install_to(
                &release(&format!("v{NEW_VERSION}")),
                &exe,
                move |done, _| {
                    counter.store(done, std::sync::atomic::Ordering::Relaxed);
                },
            )
            .await
            .expect("installs");

        let installed = std::fs::read_to_string(&exe).expect("read");
        assert!(
            installed.contains(NEW_VERSION),
            "the binary was not replaced: {installed}"
        );
        assert_eq!(
            std::fs::metadata(&exe).expect("meta").permissions().mode() & 0o777,
            0o755,
            "the executable bit must survive the swap"
        );
        assert!(
            seen.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "progress must be reported while downloading"
        );

        // The staged file is gone, not left next to the binary.
        let leftovers: Vec<_> = std::fs::read_dir(&dir.0)
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name != "spotify-agent" && name != "update-state.json")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }

    #[tokio::test]
    async fn an_archive_that_does_not_match_its_checksum_is_refused() {
        // The security boundary. If this passes anything through, whatever can
        // intercept the download chooses what the user executes.
        let Some(asset) = install::asset_name() else {
            return;
        };
        let server = MockServer::start(vec![
            Reply::text(200, sums_for(b"a completely different file", &asset)),
            Reply::bytes(200, release_archive()),
        ])
        .await;

        let dir = TempDir::new("update-tamper");
        let config = config_in(&dir);
        let exe = existing_binary(&dir);

        let error = updater(&config, &server)
            .install_to(&release(&format!("v{NEW_VERSION}")), &exe, |_, _| {})
            .await
            .expect_err("refuses");
        assert!(error.to_string().contains("checksum mismatch"), "{error}");
        assert_eq!(
            std::fs::read(&exe).expect("read"),
            b"the old binary",
            "a refused update must leave the working binary alone"
        );
    }

    #[tokio::test]
    async fn an_asset_missing_from_the_release_is_reported_not_installed() {
        let Some(asset) = install::asset_name() else {
            return;
        };
        let archive = release_archive();
        let server = MockServer::start(vec![
            Reply::text(200, sums_for(&archive, &asset)),
            Reply::text(404, "Not Found"),
        ])
        .await;

        let dir = TempDir::new("update-404");
        let config = config_in(&dir);
        let exe = existing_binary(&dir);

        let error = updater(&config, &server)
            .install_to(&release(&format!("v{NEW_VERSION}")), &exe, |_, _| {})
            .await
            .expect_err("fails");
        assert!(error.to_string().contains("404"), "{error}");
        assert_eq!(std::fs::read(&exe).expect("read"), b"the old binary");
    }

    #[tokio::test]
    async fn a_binary_claiming_another_version_is_rejected_before_the_swap() {
        // Catches a wrong-architecture or stale-asset download while the
        // working binary is still in place.
        let Some(asset) = install::asset_name() else {
            return;
        };
        let archive = release_archive();
        let server = MockServer::start(vec![
            Reply::text(200, sums_for(&archive, &asset)),
            Reply::bytes(200, archive),
        ])
        .await;

        let dir = TempDir::new("update-mismatch");
        let config = config_in(&dir);
        let exe = existing_binary(&dir);

        // The archive answers 99.0.0; the release claims 98.0.0.
        let error = updater(&config, &server)
            .install_to(&release("v98.0.0"), &exe, |_, _| {})
            .await
            .expect_err("rejects");
        assert!(error.to_string().contains("98.0.0"), "{error}");
        assert_eq!(std::fs::read(&exe).expect("read"), b"the old binary");
    }
}
