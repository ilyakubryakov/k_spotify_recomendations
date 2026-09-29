//! Platform paths.
//!
//! | Platform | Config                                        | Data                                      |
//! |----------|-----------------------------------------------|-------------------------------------------|
//! | Linux    | `~/.config/spotify-agent/config.toml`         | `~/.local/share/spotify-agent/`           |
//! | macOS    | `~/Library/Application Support/spotify-agent/` | same                                      |
//! | Windows  | `%APPDATA%\spotify-agent\config\`             | `%APPDATA%\spotify-agent\data\`           |
//!
//! `XDG_CONFIG_HOME` / `XDG_DATA_HOME` are honoured on Linux by `directories`.
//! `SPOTIFY_AGENT_CONFIG` and `SPOTIFY_AGENT_DATA_DIR` override everything.

use crate::error::{AgentError, Result};
use directories::ProjectDirs;
use std::path::PathBuf;

const APP: &str = "spotify-agent";

fn project_dirs() -> Result<ProjectDirs> {
    // Empty qualifier/organisation keeps the Linux path at the plain
    // `~/.config/spotify-agent` the CLI documents, instead of a reverse-DNS
    // directory that nobody would guess.
    ProjectDirs::from("", "", APP).ok_or_else(|| {
        AgentError::config("cannot determine the user's home directory; set SPOTIFY_AGENT_CONFIG and SPOTIFY_AGENT_DATA_DIR explicitly")
    })
}

pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = env_path("SPOTIFY_AGENT_CONFIG_DIR") {
        return Ok(dir);
    }
    Ok(project_dirs()?.config_dir().to_path_buf())
}

pub fn config_file() -> Result<PathBuf> {
    if let Some(file) = env_path("SPOTIFY_AGENT_CONFIG") {
        return Ok(file);
    }
    Ok(config_dir()?.join("config.toml"))
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = env_path("SPOTIFY_AGENT_DATA_DIR") {
        return Ok(dir);
    }
    Ok(project_dirs()?.data_dir().to_path_buf())
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Create a directory (mode 0700 on Unix) and return it.
pub fn ensure_dir(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|e| AgentError::io(path.display().to_string(), e))?;
    crate::util::fs::harden_dir(path)
}
