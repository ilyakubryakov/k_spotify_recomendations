//! Invariants for the installer scripts.
//!
//! These are shell and PowerShell files, so nothing else type-checks them.
//! Each assertion here corresponds to a failure that has actually happened or
//! that would be silent and user-visible.
// Integration tests are their own crate, so the lib's `cfg_attr(test)` lint
// relaxations do not apply here.
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::fs;

const PS1: &str = "packaging/install.ps1";
const SH: &str = "packaging/install.sh";

#[test]
fn the_powershell_installer_is_strictly_ascii() {
    // Windows PowerShell 5.1 decodes a BOM-less .ps1 using the system ANSI
    // codepage. A single em-dash inside a string therefore becomes mojibake
    // and kills the *parser* — with an error pointing at an unrelated later
    // line about a missing brace. This is exactly how it failed on Win11.
    let source = fs::read(PS1).expect("install.ps1 is readable");
    let offenders: Vec<(usize, u8)> = source
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte > 0x7F)
        .map(|(index, byte)| (index, *byte))
        .take(5)
        .collect();

    assert!(
        offenders.is_empty(),
        "install.ps1 must be pure ASCII; first non-ASCII bytes at {offenders:?}"
    );
}

#[test]
fn the_powershell_installer_has_no_byte_order_mark() {
    // A BOM would make the ASCII rule moot, but it also breaks `#!`-style
    // tooling and shows up as a stray character in diffs. Pure ASCII with no
    // BOM is the one combination every PowerShell version reads correctly.
    let source = fs::read(PS1).expect("install.ps1 is readable");
    assert!(
        !source.starts_with(&[0xEF, 0xBB, 0xBF]),
        "install.ps1 must not carry a UTF-8 BOM"
    );
}

#[test]
fn neither_installer_escalates_privileges() {
    // The whole promise of these scripts is a user-local install. A `sudo`
    // that crept in would violate it silently on the happy path.
    let sh = fs::read_to_string(SH).expect("install.sh is readable");
    for line in sh.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        // Distinguish *invoking* sudo from *printing advice about* it. The
        // script legitimately tells the user to run `sudo loginctl
        // enable-linger` once, which is not the same as doing it for them.
        let invokes = trimmed.starts_with("sudo ")
            || ["| sudo ", "&& sudo ", "; sudo ", "$(sudo ", "`sudo "]
                .iter()
                .any(|pattern| trimmed.contains(pattern));
        assert!(!invokes, "install.sh must never invoke sudo itself: {line}");
    }

    let ps1 = fs::read_to_string(PS1).expect("install.ps1 is readable");
    assert!(
        !ps1.contains("Start-Process") || !ps1.contains("RunAs"),
        "install.ps1 must never request elevation"
    );
}

#[test]
fn both_installers_refuse_to_run_privileged() {
    let sh = fs::read_to_string(SH).expect("install.sh is readable");
    assert!(
        sh.contains("id -u") && sh.contains("SPOTIFY_AGENT_ALLOW_ROOT"),
        "install.sh must refuse to run as root, with a documented override"
    );

    let ps1 = fs::read_to_string(PS1).expect("install.ps1 is readable");
    assert!(
        ps1.contains("IsInRole") && ps1.contains("SPOTIFY_AGENT_ALLOW_ADMIN"),
        "install.ps1 must refuse to run elevated, with a documented override"
    );
}

#[test]
fn installers_target_user_writable_locations_only() {
    let sh = fs::read_to_string(SH).expect("install.sh is readable");
    assert!(
        sh.contains("$HOME/.local"),
        "install.sh should default to ~/.local"
    );
    for system_path in ["/usr/local/bin", "/usr/bin", "/opt/"] {
        assert!(
            !sh.contains(&format!("PREFIX=\"{system_path}")),
            "install.sh must not default to the system path {system_path}"
        );
    }

    let ps1 = fs::read_to_string(PS1).expect("install.ps1 is readable");
    assert!(
        ps1.contains("LOCALAPPDATA"),
        "install.ps1 should install under %LOCALAPPDATA%"
    );
    assert!(
        !ps1.contains("'Machine'"),
        "install.ps1 must only ever write the *user* PATH, never the machine PATH"
    );
}

#[test]
fn the_installers_and_the_self_updater_agree_on_where_releases_come_from() {
    // Three independent implementations download the same assets: install.sh,
    // install.ps1 and `spotify-agent update`. If they drift, one of them 404s
    // at exactly the moment a user is trying to install or upgrade.
    let sh = fs::read_to_string(SH).expect("install.sh is readable");
    let ps1 = fs::read_to_string(PS1).expect("install.ps1 is readable");
    let repo = spotify_agent::config::DEFAULT_REPO;

    assert!(sh.contains(repo), "install.sh does not default to {repo}");
    assert!(ps1.contains(repo), "install.ps1 does not default to {repo}");

    // Asset names carry no version, because `releases/latest/download/` only
    // resolves for stable names — see the release workflow.
    assert!(sh.contains(r#"archive="$APP-$TRIPLE.tar.gz""#));
    assert!(ps1.contains(r#"$archive = "$App-$Triple.zip""#));
    assert_eq!(
        spotify_agent::update::install::archive_extension(),
        if cfg!(windows) { ".zip" } else { ".tar.gz" }
    );
    if let Some(asset) = spotify_agent::update::install::asset_name() {
        assert!(asset.starts_with("spotify-agent-"), "{asset}");
    }

    // Both installers verify against the same file the updater does.
    assert!(sh.contains("SHA256SUMS"));
    assert!(ps1.contains("SHA256SUMS"));
}

#[test]
fn every_published_target_is_one_the_release_workflow_builds() {
    // The updater's list is what it derives a download URL from; the workflow
    // is what actually uploads. A target in one and not the other is a 404.
    let workflow =
        fs::read_to_string(".github/workflows/release.yml").expect("release.yml is readable");
    for target in spotify_agent::update::install::PUBLISHED_TARGETS {
        assert!(
            workflow.contains(target),
            "{target} is offered by the updater but not built by release.yml"
        );
    }
}

#[test]
fn the_shell_installer_is_posix_sh_not_bash() {
    let sh = fs::read_to_string(SH).expect("install.sh is readable");
    let first = sh.lines().next().unwrap_or_default();
    // macOS ships bash 3.2 and some minimal images have no bash at all.
    assert_eq!(first, "#!/bin/sh", "install.sh must be POSIX sh");
    for bashism in ["[[ ", "declare ", "local -", "${!"] {
        assert!(
            !sh.contains(bashism),
            "install.sh uses the bashism `{bashism}`"
        );
    }
}

/// Exercise `install.sh`'s checksum verification for real.
///
/// This is the security boundary for `curl | sh`: if it can be made to accept a
/// mismatched or unlisted archive, the install pipeline is compromised. The
/// function is extracted from the script and driven with a stubbed `fetch`, so
/// the code under test is the shipping code, not a copy.
#[cfg(unix)]
mod checksum {
    use std::fs;
    use std::process::Command;

    fn harness(dir: &std::path::Path, sums_body: &str, archive_body: &[u8]) -> (bool, String) {
        let script = fs::read_to_string(super::SH).expect("install.sh is readable");

        // Pull out just `verify_checksum`, from its definition to the closing
        // brace at column 0.
        let start = script
            .find("verify_checksum() {")
            .expect("install.sh defines verify_checksum");
        let rest = &script[start..];
        let end = rest.find("\n}\n").expect("verify_checksum is closed") + 3;
        let function = &rest[..end];

        fs::write(dir.join("archive.tar.gz"), archive_body).expect("write archive");
        fs::write(dir.join("SHA256SUMS.src"), sums_body).expect("write sums");

        let harness = format!(
            r#"set -eu
SKIP_VERIFY=0
say()  {{ :; }}
info() {{ :; }}
ok()   {{ echo "OK: $*"; }}
warn() {{ echo "WARN: $*"; }}
die()  {{ echo "DIE: $*"; exit 9; }}
# Stub the network: serve SHA256SUMS from disk.
fetch() {{ cp "{dir}/SHA256SUMS.src" "$2" 2>/dev/null; }}
release_base() {{ printf 'stub'; }}

{function}

verify_checksum "{dir}" archive.tar.gz
echo "ACCEPTED"
"#,
            dir = dir.display(),
            function = function
        );

        let path = dir.join("harness.sh");
        fs::write(&path, harness).expect("write harness");
        let output = Command::new("sh").arg(&path).output().expect("run harness");
        let combined = String::from_utf8_lossy(&output.stdout).to_string()
            + &String::from_utf8_lossy(&output.stderr);
        (output.status.success(), combined)
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spotify-agent-sum-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// sha256 of the literal bytes `payload`.
    const PAYLOAD: &[u8] = b"payload";
    const PAYLOAD_SHA256: &str = "239f59ed55e737c77147cf55ad0c1b030b6d7ee748a7426952f9b852d5a935e5";

    #[test]
    fn a_matching_checksum_is_accepted() {
        let dir = scratch("ok");
        let sums = format!("{PAYLOAD_SHA256}  archive.tar.gz\n");
        let (success, out) = harness(&dir, &sums, PAYLOAD);
        assert!(success, "should accept a matching checksum: {out}");
        assert!(out.contains("ACCEPTED"), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tampered_archive_is_refused() {
        let dir = scratch("bad");
        let sums = format!("{PAYLOAD_SHA256}  archive.tar.gz\n");
        let (success, out) = harness(&dir, &sums, b"payload-but-modified");
        assert!(!success, "a mismatched archive must not be accepted: {out}");
        assert!(out.contains("DIE:"), "should abort loudly: {out}");
        assert!(out.contains("checksum mismatch"), "{out}");
        assert!(!out.contains("ACCEPTED"), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_archive_missing_from_sha256sums_is_refused() {
        let dir = scratch("missing");
        let sums = format!("{PAYLOAD_SHA256}  some-other-file.tar.gz\n");
        let (success, out) = harness(&dir, &sums, PAYLOAD);
        // Not fatal, but it must return non-zero so the caller falls back
        // rather than installing an unverified download.
        assert!(!success || !out.contains("ACCEPTED"), "{out}");
        assert!(out.contains("not listed"), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bsd_style_star_prefix_is_accepted() {
        // `sha256sum -b` writes "<hash> *<file>"; both spellings appear in the
        // wild and the installer must read either.
        let dir = scratch("star");
        let sums = format!("{PAYLOAD_SHA256} *archive.tar.gz\n");
        let (success, out) = harness(&dir, &sums, PAYLOAD);
        assert!(success, "should accept the `*file` spelling: {out}");
        assert!(out.contains("ACCEPTED"), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }
}
