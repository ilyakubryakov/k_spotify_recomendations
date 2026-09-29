//! Downloading a release archive and putting the new binary in place.
//!
//! The order of operations is the whole design, because the failure modes are
//! asymmetric: a failed download costs a retry, a botched replacement costs
//! the user their working install.
//!
//!   1. verify the archive against the release's `SHA256SUMS` — the same
//!      boundary `install.sh` enforces, for the same reason: without it,
//!      whatever can intercept the download chooses what you execute;
//!   2. extract in memory, never onto the path the archive names;
//!   3. stage the new binary *next to* the current one, so the final step is a
//!      rename within one filesystem rather than a copy that can half-finish;
//!   4. run the staged binary with `--version` and require the expected
//!      answer — this catches a wrong-architecture or truncated download while
//!      the working binary is still in place;
//!   5. swap, and only then discard the old one.
//!
//! Everything here is synchronous. The caller runs it on a blocking thread:
//! it is all filesystem work plus one short-lived subprocess, and pretending
//! otherwise would only add colour to the signatures.

use crate::error::{AgentError, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Targets the release workflow actually publishes. Anything else has to be
/// built from source, and saying so is better than 404-ing at download time.
pub const PUBLISHED_TARGETS: [&str; 5] = [
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu",
    "universal-apple-darwin",
    "x86_64-pc-windows-msvc",
];

/// The binary's own name inside the release archive.
pub const BINARY_NAME: &str = if cfg!(windows) {
    "spotify-agent.exe"
} else {
    "spotify-agent"
};

/// A sanity ceiling on what we will decompress into memory. The real archives
/// are ~5 MB; this only exists so a hostile or corrupt archive cannot be
/// inflated into an out-of-memory kill.
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;

/// The release asset this build can install, or `None` when no asset is
/// published for this platform.
///
/// macOS is deliberately not architecture-dependent: the release is a single
/// `lipo`'d universal binary, so an Intel Mac and an Apple Silicon Mac fetch
/// the same file.
pub fn target_triple() -> Option<&'static str> {
    let triple = if cfg!(target_os = "macos") {
        "universal-apple-darwin"
    } else if cfg!(target_os = "windows") {
        match std::env::consts::ARCH {
            "x86_64" => "x86_64-pc-windows-msvc",
            _ => return None,
        }
    } else if cfg!(target_os = "linux") {
        match (std::env::consts::ARCH, cfg!(target_env = "musl")) {
            ("x86_64", true) => "x86_64-unknown-linux-musl",
            ("x86_64", false) => "x86_64-unknown-linux-gnu",
            ("aarch64", false) => "aarch64-unknown-linux-gnu",
            _ => return None,
        }
    } else {
        return None;
    };
    Some(triple)
}

pub fn archive_extension() -> &'static str {
    if cfg!(windows) { ".zip" } else { ".tar.gz" }
}

/// Release asset filename. No version in it — `releases/latest/download/`
/// only resolves for stable names, and both installers depend on that.
pub fn asset_name() -> Option<String> {
    Some(format!(
        "spotify-agent-{}{}",
        target_triple()?,
        archive_extension()
    ))
}

// ===========================================================================
// Where we are installed
// ===========================================================================

/// Absolute path of the running binary, with Windows' `\\?\` prefix removed.
pub fn current_exe() -> Result<PathBuf> {
    crate::schedule::current_binary()
}

/// Why this install cannot replace itself, if it cannot.
///
/// Two distinct cases, and conflating them produces a useless message:
/// a package-manager-owned path should never be overwritten even when it
/// happens to be writable, and a read-only directory cannot be.
pub fn blocked_reason(exe: &Path) -> Option<String> {
    let shown = exe.display().to_string();

    // A Nix store path is content-addressed and read-only by construction;
    // writing into it would corrupt the store's invariants.
    if shown.starts_with("/nix/store/") {
        return Some(format!(
            "{shown} is managed by Nix; update the package instead of this binary"
        ));
    }
    for prefix in ["/usr/", "/opt/homebrew/", "/usr/local/Cellar/", "/snap/"] {
        if shown.starts_with(prefix) {
            return Some(format!(
                "{shown} was installed by a package manager; update it the same way"
            ));
        }
    }

    let parent = exe.parent()?;
    if !is_writable(parent) {
        return Some(format!(
            "{} is not writable by this user; re-install with packaging/install.sh, or update as whoever owns it",
            parent.display()
        ));
    }
    None
}

/// Probe by creating a file, not by reading permission bits: the bits lie
/// under ACLs, read-only mounts, SELinux and containers, and the only answer
/// that matters is whether the rename in step 5 will work.
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".spotify-agent-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

// ===========================================================================
// Checksums
// ===========================================================================

/// Check `archive` against the `SHA256SUMS` line for `asset`.
///
/// A missing entry is a failure, not a pass: "not listed" and "listed and
/// matching" must never take the same branch.
pub fn verify_checksum(sums: &str, asset: &str, archive: &[u8]) -> Result<()> {
    let expected = sums
        .lines()
        .filter_map(|line| {
            let (digest, name) = line.split_once(char::is_whitespace)?;
            // GNU coreutils marks binary mode with a leading `*` on the name.
            let name = name.trim().trim_start_matches('*');
            (name == asset).then(|| digest.trim().to_ascii_lowercase())
        })
        .next()
        .ok_or_else(|| {
            AgentError::other(format!("{asset} is not listed in the release's SHA256SUMS"))
        })?;

    let actual = hex(&Sha256::digest(archive));
    if actual != expected {
        return Err(AgentError::other(format!(
            "checksum mismatch for {asset}: expected {expected}, got {actual}. \
             Refusing to install — this is either a corrupted download or tampering."
        )));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

// ===========================================================================
// Extraction
// ===========================================================================

/// Pull `BINARY_NAME` out of the release archive, in memory.
///
/// The archive's own paths are used only to *recognise* the entry, never to
/// decide where anything is written, so a crafted archive has no path to
/// traverse out of.
pub fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    #[cfg(windows)]
    {
        extract_from_zip(archive)
    }
    #[cfg(not(windows))]
    {
        extract_from_tar_gz(archive)
    }
}

#[cfg(not(windows))]
fn extract_from_tar_gz(archive: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;

    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    let entries = tar
        .entries()
        .map_err(|e| AgentError::other(format!("release archive is not readable: {e}")))?;

    for entry in entries {
        let mut entry =
            entry.map_err(|e| AgentError::other(format!("release archive is truncated: {e}")))?;
        let is_binary = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n == BINARY_NAME))
            .unwrap_or(false);
        if !is_binary {
            continue;
        }

        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(MAX_BINARY_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|e| {
                AgentError::other(format!("cannot read {BINARY_NAME} from the archive: {e}"))
            })?;
        return Ok(bytes);
    }

    Err(AgentError::other(format!(
        "the release archive does not contain {BINARY_NAME}"
    )))
}

#[cfg(windows)]
fn extract_from_zip(archive: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
        .map_err(|e| AgentError::other(format!("release archive is not readable: {e}")))?;

    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|e| AgentError::other(format!("release archive is truncated: {e}")))?;
        // Compress-Archive stores forward or backward slashes depending on the
        // PowerShell version, so compare the last component either way.
        let name = entry.name().replace('\\', "/");
        if name.rsplit('/').next() != Some(BINARY_NAME) {
            continue;
        }

        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(MAX_BINARY_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|e| {
                AgentError::other(format!("cannot read {BINARY_NAME} from the archive: {e}"))
            })?;
        return Ok(bytes);
    }

    Err(AgentError::other(format!(
        "the release archive does not contain {BINARY_NAME}"
    )))
}

// ===========================================================================
// Replacement
// ===========================================================================

/// A staged binary that deletes itself unless it is committed.
///
/// Without this, every early return between "wrote the file" and "renamed it
/// into place" would leak a multi-megabyte turd next to the user's binary.
pub struct Staged {
    path: PathBuf,
    committed: bool,
}

impl Staged {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Write the new binary beside the current one, executable and ready to swap.
///
/// Beside, specifically: `rename` is only atomic within a filesystem, and
/// `$TMPDIR` is very often a different one (tmpfs on Linux, a separate volume
/// in a container). Staging elsewhere would degrade the swap into a copy that
/// can be interrupted halfway through.
pub fn stage(exe: &Path, bytes: &[u8]) -> Result<Staged> {
    let dir = exe
        .parent()
        .ok_or_else(|| AgentError::other(format!("{} has no parent directory", exe.display())))?;
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "spotify-agent".to_string());

    let path = dir.join(format!(".{name}.new-{}", std::process::id()));
    std::fs::write(&path, bytes).map_err(|e| AgentError::io(path.display().to_string(), e))?;
    let staged = Staged {
        path,
        committed: false,
    };

    copy_mode(exe, staged.path())?;
    Ok(staged)
}

/// Give the replacement the permissions the original had, defaulting to 0755.
///
/// Inheriting rather than hardcoding matters for shared installs: a binary at
/// mode 0750 owned by a group should not silently become world-executable
/// because it was updated.
#[cfg(unix)]
fn copy_mode(exe: &Path, staged: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(exe)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o755);
    // The archive is the source of truth for "is a program", so the execute
    // bit is forced on even if the previous file somehow lacked it.
    let mode = mode | 0o700;
    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(mode))
        .map_err(|e| AgentError::io(staged.display().to_string(), e))
}

#[cfg(not(unix))]
fn copy_mode(_exe: &Path, _staged: &Path) -> Result<()> {
    // Windows infers executability from the extension; the staged file already
    // ends in `.exe`, and it inherits the directory's ACL.
    Ok(())
}

/// Confirm the staged binary runs on this machine and is the version claimed.
///
/// This is the step that turns "downloaded the wrong architecture" from a
/// broken install into a refused update: it runs *before* anything is
/// replaced, so a failure here leaves the user exactly where they started.
pub fn smoke_test(staged: &Path, expected: &str) -> Result<()> {
    let output = std::process::Command::new(staged)
        .arg("--version")
        .output()
        .map_err(|e| {
            AgentError::other(format!(
                "the downloaded binary will not run on this machine: {e}"
            ))
        })?;

    if !output.status.success() {
        return Err(AgentError::other(format!(
            "the downloaded binary exited with {} when asked for its version",
            output.status
        )));
    }

    let reported = String::from_utf8_lossy(&output.stdout);
    if !reported.contains(expected) {
        return Err(AgentError::other(format!(
            "the downloaded binary reports `{}` but the release claims {expected}",
            reported.trim()
        )));
    }
    Ok(())
}

/// Put the staged binary in place of the running one.
///
/// Unix: a plain rename. The running process keeps its open inode, so the
/// swap is invisible to it and atomic for everyone else.
///
/// Windows: an executable with a mapped image cannot be deleted or
/// overwritten, but it *can* be renamed. So the live binary is moved aside
/// first and only removed once nothing holds it — which is never within this
/// process, hence [`cleanup_leftovers`].
pub fn swap(exe: &Path, staged: Staged) -> Result<()> {
    #[cfg(windows)]
    {
        let backup = backup_path(exe)?;
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(exe, &backup).map_err(|e| {
            AgentError::io(
                format!("{} (moving the running binary aside)", exe.display()),
                e,
            )
        })?;

        if let Err(e) = std::fs::rename(staged.path(), exe) {
            // Put the user's working binary back before reporting: a failed
            // update must not also be a failed uninstall.
            let _ = std::fs::rename(&backup, exe);
            return Err(AgentError::io(exe.display().to_string(), e));
        }
        commit(staged);
        // Expected to fail while this process is running; the next start
        // sweeps it up.
        let _ = std::fs::remove_file(&backup);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(staged.path(), exe)
            .map_err(|e| AgentError::io(exe.display().to_string(), e))?;
        commit(staged);
        Ok(())
    }
}

fn commit(mut staged: Staged) {
    staged.committed = true;
}

#[cfg(windows)]
fn backup_path(exe: &Path) -> Result<PathBuf> {
    let dir = exe
        .parent()
        .ok_or_else(|| AgentError::other(format!("{} has no parent directory", exe.display())))?;
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "spotify-agent.exe".to_string());
    let fixed = dir.join(format!("{name}.old"));
    if !fixed.exists() {
        return Ok(fixed);
    }
    // A previous `.old` is still held open by another running instance.
    Ok(dir.join(format!("{name}.old-{}", std::process::id())))
}

/// Remove binaries left behind by an earlier update.
///
/// Only Windows can leave any: elsewhere the swap consumes the staged file and
/// the old inode is reclaimed when the last process exits. Best-effort by
/// design — a leftover is wasted disk, not a fault worth reporting.
pub fn cleanup_leftovers() {
    #[cfg(windows)]
    {
        let Ok(exe) = current_exe() else { return };
        let Some(dir) = exe.parent().map(Path::to_path_buf) else {
            return;
        };
        let Some(name) = exe.file_name().map(|n| n.to_string_lossy().to_string()) else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let stale = format!("{name}.old");
        for entry in entries.flatten() {
            let found = entry.file_name().to_string_lossy().to_string();
            if found.starts_with(&stale) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_name_matches_what_the_release_workflow_publishes() {
        let Some(triple) = target_triple() else {
            // A platform with no published asset is a supported state; the
            // updater reports it instead of guessing a URL.
            return;
        };
        assert!(
            PUBLISHED_TARGETS.contains(&triple),
            "{triple} is not published by .github/workflows/release.yml"
        );
        let asset = asset_name().expect("an asset name when a triple exists");
        assert_eq!(
            asset,
            format!("spotify-agent-{triple}{}", archive_extension())
        );
        // No version in the filename: `releases/latest/download/` only
        // resolves for stable names, and both installers rely on that.
        assert!(!asset.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn macos_installs_the_universal_binary_on_either_architecture() {
        if cfg!(target_os = "macos") {
            assert_eq!(target_triple(), Some("universal-apple-darwin"));
        }
    }

    #[test]
    fn a_matching_checksum_passes() {
        let archive = b"pretend this is a tarball";
        let digest = hex(&Sha256::digest(archive));
        let sums = format!("{digest}  spotify-agent-x86_64-unknown-linux-gnu.tar.gz\n");
        verify_checksum(
            &sums,
            "spotify-agent-x86_64-unknown-linux-gnu.tar.gz",
            archive,
        )
        .expect("verifies");
    }

    #[test]
    fn the_binary_mode_marker_coreutils_writes_is_tolerated() {
        let archive = b"bytes";
        let digest = hex(&Sha256::digest(archive));
        let sums = format!("{digest} *spotify-agent-x86_64-pc-windows-msvc.zip\n");
        verify_checksum(&sums, "spotify-agent-x86_64-pc-windows-msvc.zip", archive)
            .expect("verifies");
    }

    #[test]
    fn a_mismatched_checksum_is_refused() {
        let sums = format!("{}  asset.tar.gz\n", "0".repeat(64));
        let error = verify_checksum(&sums, "asset.tar.gz", b"bytes").expect_err("refuses");
        assert!(error.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn an_unlisted_asset_is_refused_rather_than_waved_through() {
        // The dangerous bug: treating "no line for this file" as success.
        let sums = format!("{}  something-else.tar.gz\n", "0".repeat(64));
        let error = verify_checksum(&sums, "asset.tar.gz", b"bytes").expect_err("refuses");
        assert!(error.to_string().contains("not listed"));
    }

    #[test]
    fn a_prefix_of_another_assets_name_does_not_match_it() {
        let archive = b"bytes";
        let digest = hex(&Sha256::digest(archive));
        let sums = format!("{digest}  spotify-agent-x86_64-unknown-linux-gnu.tar.gz\n");
        // "…-linux-gnu" is a prefix of "…-linux-gnu.tar.gz"; a sloppy
        // `starts_with` would verify the wrong file against it.
        verify_checksum(&sums, "spotify-agent-x86_64-unknown-linux", archive).expect_err("refuses");
    }

    #[cfg(not(windows))]
    #[test]
    fn the_binary_is_found_inside_the_archives_directory() {
        let archive = tar_gz_fixture();
        let bytes = extract_binary(&archive).expect("extracts");
        assert_eq!(bytes, b"#!/bin/sh\necho hello\n");
    }

    #[cfg(not(windows))]
    #[test]
    fn an_archive_without_the_binary_is_an_error_not_an_empty_install() {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let body = b"not the binary";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "spotify-agent-x/README.md", &body[..])
            .expect("append");
        let archive = builder
            .into_inner()
            .and_then(|e| e.finish())
            .expect("finish");

        assert!(extract_binary(&archive).is_err());
    }

    #[cfg(not(windows))]
    fn tar_gz_fixture() -> Vec<u8> {
        let body = b"#!/bin/sh\necho hello\n";
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("spotify-agent-{}/{BINARY_NAME}", PUBLISHED_TARGETS[0]),
                &body[..],
            )
            .expect("append");
        builder
            .into_inner()
            .and_then(|e| e.finish())
            .expect("finish")
    }

    #[test]
    fn staging_then_swapping_replaces_the_file_in_place() {
        let dir = std::env::temp_dir().join(format!("sa-swap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let exe = dir.join("spotify-agent-fake");
        std::fs::write(&exe, b"old").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        let staged = stage(&exe, b"new").expect("stage");
        assert!(staged.path().exists());
        swap(&exe, staged).expect("swap");

        assert_eq!(std::fs::read(&exe).expect("read"), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&exe).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "the executable bit must survive");
        }
        // Nothing left behind next to the binary.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "spotify-agent-fake")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_uncommitted_staged_file_removes_itself() {
        let dir = std::env::temp_dir().join(format!("sa-stage-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let exe = dir.join("spotify-agent-fake");
        std::fs::write(&exe, b"old").expect("write");

        let path = {
            let staged = stage(&exe, b"new").expect("stage");
            staged.path().to_path_buf()
        };
        assert!(!path.exists(), "the guard must clean up on an early return");
        assert_eq!(std::fs::read(&exe).expect("read"), b"old");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn the_smoke_test_rejects_a_binary_that_reports_another_version() {
        let dir = std::env::temp_dir().join(format!("sa-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let script = dir.join("fake-agent");
        std::fs::write(&script, "#!/bin/sh\necho 'spotify-agent 9.9.9'\n").expect("write");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        smoke_test(&script, "9.9.9").expect("accepts the version it claims");
        let error = smoke_test(&script, "1.0.0").expect_err("rejects any other");
        assert!(error.to_string().contains("9.9.9"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_nix_store_path_is_reported_as_managed() {
        let reason = blocked_reason(Path::new("/nix/store/abc-spotify-agent/bin/spotify-agent"));
        assert!(reason.is_some_and(|r| r.contains("Nix")));
    }

    #[test]
    fn a_user_local_path_is_not_blocked() {
        let dir = std::env::temp_dir().join(format!("sa-writable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        assert!(blocked_reason(&dir.join("spotify-agent")).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
