//! Filesystem helpers with owner-only permissions for anything secret-bearing.

use crate::error::{AgentError, Result};
use std::path::Path;

/// Atomically write `bytes` to `path` with mode 0600 on Unix.
///
/// Atomic = write to `path.tmp` then rename, so a crash mid-write can never
/// leave a truncated token file that would force a re-login.
pub async fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AgentError::io(parent.display().to_string(), e))?;
        harden_dir(parent)?;
    }

    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(|e| AgentError::io(tmp.display().to_string(), e))?;
    harden_file(&tmp)?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| AgentError::io(path.display().to_string(), e))?;
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms).map_err(|e| AgentError::io(path.display().to_string(), e))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    // Windows ACLs: the per-user AppData roaming dir is already owner-scoped.
    Ok(())
}

pub fn harden_file(path: &Path) -> Result<()> {
    set_mode(path, 0o600)
}

pub fn harden_dir(path: &Path) -> Result<()> {
    set_mode(path, 0o700)
}

/// A warning if the config file is group/world readable, else `None`.
///
/// Not fatal: the file may legitimately hold no secret. But it may hold an API
/// key, and a silent 0644 is a real leak path — so it is always reported.
/// Returns the message rather than logging it, because config loading runs
/// before the log subscriber exists.
#[cfg(unix)]
pub fn world_readable_warning(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).ok()?;
    let mode = meta.permissions().mode() & 0o777;
    (mode & 0o077 != 0).then(|| {
        format!(
            "{} is readable beyond its owner (mode {mode:o}); run `chmod 600` on it",
            path.display()
        )
    })
}

/// Windows permissions are ACL-based; the per-user config dir is already
/// owner-scoped, so there is nothing equivalent to check.
#[cfg(not(unix))]
pub fn world_readable_warning(_path: &Path) -> Option<String> {
    None
}
