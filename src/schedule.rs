//! Built-in background scheduling, per user, without root.
//!
//! Every platform already has a per-user scheduler; this module writes to the
//! right one and never touches a system-wide facility:
//!
//! | platform | mechanism                      | location                              |
//! |----------|--------------------------------|---------------------------------------|
//! | Linux    | `systemd --user` timer         | `~/.config/systemd/user/`             |
//! | macOS    | `launchd` LaunchAgent          | `~/Library/LaunchAgents/`             |
//! | Windows  | Task Scheduler, current user   | registered via `schtasks` XML         |
//!
//! No `cron` editing, no `sudo`, no machine-wide daemons. The unit is
//! generated from the running binary's own path, so an install that moved
//! keeps working after `schedule install` is re-run.

use crate::error::{AgentError, Result};
use std::fmt;
use std::path::PathBuf;
use std::process::Command;

/// How often the agent should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    Hourly,
    Daily,
    Weekly,
}

impl Cadence {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "hourly" => Some(Self::Hourly),
            "daily" => Some(Self::Daily),
            "weekly" => Some(Self::Weekly),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
        }
    }

    /// systemd `OnCalendar=` expression. Linux only.
    #[cfg(any(target_os = "linux", test))]
    fn systemd_calendar(self, hour: u32, minute: u32) -> String {
        match self {
            Self::Hourly => "hourly".to_string(),
            Self::Daily => format!("*-*-* {hour:02}:{minute:02}:00"),
            Self::Weekly => format!("Mon *-*-* {hour:02}:{minute:02}:00"),
        }
    }

    /// Interval in seconds — launchd schedules by interval, not by calendar.
    #[cfg(target_os = "macos")]
    fn seconds(self) -> u64 {
        match self {
            Self::Hourly => 3_600,
            Self::Daily => 86_400,
            Self::Weekly => 604_800,
        }
    }
}

impl fmt::Display for Cadence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What to schedule.
#[derive(Debug, Clone)]
pub struct ScheduleSpec {
    pub cadence: Cadence,
    /// Local hour/minute for daily and weekly runs.
    pub hour: u32,
    pub minute: u32,
    /// Preset passed to `generate`.
    pub preset: Option<String>,
    /// Absolute path to the binary to run.
    pub binary: PathBuf,
    /// Extra arguments appended after `--cron generate`.
    pub extra_args: Vec<String>,
}

impl ScheduleSpec {
    /// The full argument list, minus the binary itself.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["--cron".to_string(), "generate".to_string()];
        if let Some(preset) = &self.preset {
            args.push(preset.clone());
        }
        args.extend(self.extra_args.iter().cloned());
        args
    }

    pub fn command_line(&self) -> String {
        let mut parts = vec![quote_if_needed(&self.binary.display().to_string())];
        parts.extend(self.args().iter().map(|a| quote_if_needed(a)));
        parts.join(" ")
    }
}

fn quote_if_needed(value: &str) -> String {
    if value.contains(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

/// Current state of the scheduled job.
#[derive(Debug, Clone)]
pub struct ScheduleStatus {
    pub installed: bool,
    pub mechanism: &'static str,
    pub detail: String,
    /// Where the definition lives, when it is a file.
    pub path: Option<PathBuf>,
}

pub const TASK_NAME: &str = "spotify-agent";
pub const LAUNCHD_LABEL: &str = "io.github.spotify-agent";

/// Resolve the binary to schedule: the running executable, by absolute path.
pub fn current_binary() -> Result<PathBuf> {
    let path = std::env::current_exe()
        .map_err(|e| AgentError::other(format!("cannot determine this binary's path: {e}")))?;
    // A relative or symlinked path would break once the scheduler runs from a
    // different working directory.
    let resolved = path.canonicalize().unwrap_or(path);
    Ok(strip_extended_prefix(resolved))
}

/// Remove Windows' `\\?\` extended-length prefix.
///
/// `canonicalize` returns it on Windows, and while Task Scheduler stores such a
/// path without complaint, it is not universally understood — and it is what
/// the user sees when they inspect the task. Stripping it is safe for ordinary
/// drive paths; a UNC extended path (`\\?\UNC\...`) is left alone, since
/// shortening that one changes its meaning.
fn strip_extended_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

// ===========================================================================
// Linux — systemd user units
// ===========================================================================

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    fn unit_dir() -> Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs_home().map(|h| h.join(".config")))
            .ok_or_else(|| AgentError::config("cannot locate the user config directory"))?;
        Ok(base.join("systemd/user"))
    }

    fn dirs_home() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    pub fn service_path() -> Result<PathBuf> {
        Ok(unit_dir()?.join(format!("{TASK_NAME}.service")))
    }

    pub fn timer_path() -> Result<PathBuf> {
        Ok(unit_dir()?.join(format!("{TASK_NAME}.timer")))
    }

    pub fn render_service(spec: &ScheduleSpec) -> String {
        format!(
            "[Unit]\n\
             Description=spotify-agent — generate an AI-curated playlist\n\
             Documentation=https://github.com/ilyakubryakov/k_spotify_recomendations\n\
             After=network-online.target\n\
             Wants=network-online.target\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart={}\n\
             # 75 is EX_TEMPFAIL (rate limited / upstream down): retry rather than alert.\n\
             Restart=on-failure\n\
             RestartSec=15min\n\
             NoNewPrivileges=true\n\
             PrivateTmp=true\n",
            spec.command_line()
        )
    }

    pub fn render_timer(spec: &ScheduleSpec) -> String {
        format!(
            "[Unit]\n\
             Description=Run spotify-agent on a schedule\n\
             \n\
             [Timer]\n\
             OnCalendar={}\n\
             # Catch up after the machine was asleep at the scheduled time.\n\
             Persistent=true\n\
             # Spread load so installs do not all hit the APIs on the same second.\n\
             RandomizedDelaySec=15min\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n",
            spec.cadence.systemd_calendar(spec.hour, spec.minute)
        )
    }

    pub fn install(spec: &ScheduleSpec) -> Result<ScheduleStatus> {
        let dir = unit_dir()?;
        std::fs::create_dir_all(&dir).map_err(|e| AgentError::io(dir.display().to_string(), e))?;

        let service = service_path()?;
        let timer = timer_path()?;
        std::fs::write(&service, render_service(spec))
            .map_err(|e| AgentError::io(service.display().to_string(), e))?;
        std::fs::write(&timer, render_timer(spec))
            .map_err(|e| AgentError::io(timer.display().to_string(), e))?;

        // The unit files are already on disk at this point. If systemd cannot
        // be reached — no session bus, a container, or XDG_CONFIG_HOME pointing
        // somewhere systemd does not read — report that honestly instead of
        // failing and leaving orphaned files behind with no explanation.
        let enabled = run_systemctl(&["daemon-reload"])
            .and_then(|()| run_systemctl(&["enable", "--now", &format!("{TASK_NAME}.timer")]));

        let mut detail = match enabled {
            Ok(()) => format!("{} timer enabled", spec.cadence),
            Err(e) => {
                return Ok(ScheduleStatus {
                    installed: false,
                    mechanism: "systemd --user",
                    detail: format!(
                        "unit files written to {}, but systemd would not enable them: {e}\n                           enable them yourself with:\n                             systemctl --user daemon-reload && systemctl --user enable --now {TASK_NAME}.timer",
                        dir.display()
                    ),
                    path: Some(timer),
                });
            }
        };
        if !lingering_enabled() {
            // Without lingering, user timers stop when the last session ends —
            // a silent non-run that is very hard to notice.
            detail.push_str(
                "\n  note: user timers only run while you are logged in.\n  \
                 for headless operation run once: sudo loginctl enable-linger $USER",
            );
        }

        Ok(ScheduleStatus {
            installed: true,
            mechanism: "systemd --user",
            detail,
            path: Some(timer),
        })
    }

    pub fn remove() -> Result<bool> {
        let _ = run_systemctl(&["disable", "--now", &format!("{TASK_NAME}.timer")]);
        let mut removed = false;
        for path in [service_path()?, timer_path()?] {
            if path.exists() {
                std::fs::remove_file(&path)
                    .map_err(|e| AgentError::io(path.display().to_string(), e))?;
                removed = true;
            }
        }
        let _ = run_systemctl(&["daemon-reload"]);
        Ok(removed)
    }

    pub fn status() -> Result<ScheduleStatus> {
        let timer = timer_path()?;
        if !timer.exists() {
            return Ok(ScheduleStatus {
                installed: false,
                mechanism: "systemd --user",
                detail: "no timer installed".into(),
                path: None,
            });
        }
        let detail = Command::new("systemctl")
            .args([
                "--user",
                "list-timers",
                "--all",
                &format!("{TASK_NAME}.timer"),
            ])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "installed (systemctl unavailable)".into());

        Ok(ScheduleStatus {
            installed: true,
            mechanism: "systemd --user",
            detail,
            path: Some(timer),
        })
    }

    fn run_systemctl(args: &[&str]) -> Result<()> {
        let output = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .map_err(|e| {
                AgentError::other(format!(
                    "systemctl is not available ({e}); the unit files were written but not enabled"
                ))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(AgentError::other(format!(
                "systemctl {args:?} failed: {stderr}"
            )));
        }
        Ok(())
    }

    fn lingering_enabled() -> bool {
        let user = std::env::var("USER").unwrap_or_default();
        Command::new("loginctl")
            .args(["show-user", &user, "-p", "Linger"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.contains("Linger=yes"))
            .unwrap_or(false)
    }
}

// ===========================================================================
// macOS — launchd LaunchAgent
// ===========================================================================

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn home() -> Result<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| AgentError::config("HOME is not set"))
    }

    pub fn plist_path() -> Result<PathBuf> {
        Ok(home()?
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")))
    }

    fn log_dir() -> Result<PathBuf> {
        Ok(home()?.join("Library/Logs/spotify-agent"))
    }

    pub fn render_plist(spec: &ScheduleSpec) -> String {
        let args: String = std::iter::once(spec.binary.display().to_string())
            .chain(spec.args())
            .map(|a| format!("        <string>{}</string>\n", xml_escape(&a)))
            .collect();
        let logs = log_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{args}    </array>
    <key>StartInterval</key><integer>{interval}</integer>
    <!-- Do not fire the moment the agent is loaded; wait for the interval. -->
    <key>RunAtLoad</key><false/>
    <key>StandardOutPath</key><string>{logs}/agent.log</string>
    <key>StandardErrorPath</key><string>{logs}/agent.err.log</string>
    <key>ProcessType</key><string>Background</string>
</dict>
</plist>
"#,
            label = LAUNCHD_LABEL,
            args = args,
            interval = spec.cadence.seconds(),
            logs = xml_escape(&logs),
        )
    }

    pub fn install(spec: &ScheduleSpec) -> Result<ScheduleStatus> {
        let path = plist_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| AgentError::io(parent.display().to_string(), e))?;
        }
        let logs = log_dir()?;
        std::fs::create_dir_all(&logs).ok();

        std::fs::write(&path, render_plist(spec))
            .map_err(|e| AgentError::io(path.display().to_string(), e))?;

        // Unload first so a re-install replaces rather than duplicates.
        let _ = Command::new("launchctl").arg("unload").arg(&path).output();
        let output = Command::new("launchctl")
            .arg("load")
            .arg(&path)
            .output()
            .map_err(|e| AgentError::other(format!("launchctl is unavailable: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(AgentError::other(format!(
                "launchctl load failed: {stderr}"
            )));
        }

        Ok(ScheduleStatus {
            installed: true,
            mechanism: "launchd (LaunchAgent)",
            detail: format!(
                "{} agent loaded\n  logs: {}/agent.log\n  \
                 note: launchd does not inherit your shell environment — make the API key \
                 visible with `launchctl setenv ANTHROPIC_API_KEY \"$ANTHROPIC_API_KEY\"` \
                 or put it in the config file",
                spec.cadence,
                logs.display()
            ),
            path: Some(path),
        })
    }

    pub fn remove() -> Result<bool> {
        let path = plist_path()?;
        if !path.exists() {
            return Ok(false);
        }
        let _ = Command::new("launchctl").arg("unload").arg(&path).output();
        std::fs::remove_file(&path).map_err(|e| AgentError::io(path.display().to_string(), e))?;
        Ok(true)
    }

    pub fn status() -> Result<ScheduleStatus> {
        let path = plist_path()?;
        if !path.exists() {
            return Ok(ScheduleStatus {
                installed: false,
                mechanism: "launchd (LaunchAgent)",
                detail: "no launch agent installed".into(),
                path: None,
            });
        }
        let listed = Command::new("launchctl")
            .args(["list", LAUNCHD_LABEL])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        Ok(ScheduleStatus {
            installed: true,
            mechanism: "launchd (LaunchAgent)",
            detail: if listed {
                "loaded".into()
            } else {
                "installed but not loaded".into()
            },
            path: Some(path),
        })
    }
}

// ===========================================================================
// Windows — Task Scheduler (current user, no elevation)
// ===========================================================================

#[cfg(target_os = "windows")]
mod platform {
    use super::*;

    pub fn install(spec: &ScheduleSpec) -> Result<ScheduleStatus> {
        // `schtasks` is present on every Windows SKU, unlike the PowerShell
        // ScheduledTasks module, and creating a task under the current user
        // requires no elevation.
        let (sc, extra): (&str, Vec<String>) = match spec.cadence {
            Cadence::Hourly => ("HOURLY", vec![]),
            Cadence::Daily => ("DAILY", vec![]),
            Cadence::Weekly => ("WEEKLY", vec!["/D".into(), "MON".into()]),
        };

        let start_time = format!("{:02}:{:02}", spec.hour, spec.minute);
        let mut args: Vec<String> = vec![
            "/Create".into(),
            "/F".into(), // replace an existing definition
            "/TN".into(),
            TASK_NAME.into(),
            "/TR".into(),
            spec.command_line(),
            "/SC".into(),
            sc.into(),
            "/RL".into(),
            // LIMITED = the user's normal token: no administrator rights.
            "LIMITED".into(),
        ];
        if spec.cadence != Cadence::Hourly {
            args.push("/ST".into());
            args.push(start_time);
        }
        args.extend(extra);

        let output = Command::new("schtasks")
            .args(&args)
            .output()
            .map_err(|e| AgentError::other(format!("schtasks is unavailable: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            return Err(AgentError::other(format!(
                "schtasks /Create failed: {}",
                if stderr.is_empty() { stdout } else { stderr }
            )));
        }

        Ok(ScheduleStatus {
            installed: true,
            mechanism: "Task Scheduler (current user)",
            detail: format!(
                "{} task '{TASK_NAME}' registered\n  \
                 note: the task runs only while you are logged in (Logon Mode: Interactive only).\n  \
                 That is the trade for needing no administrator rights — a task that runs\n  \
                 logged-out has to store your password.\n  \
                 note: a scheduled task sees only *user* environment variables — set the API key with\n  \
                 [Environment]::SetEnvironmentVariable('ANTHROPIC_API_KEY','sk-ant-...','User')",
                spec.cadence
            ),
            path: None,
        })
    }

    pub fn remove() -> Result<bool> {
        let output = Command::new("schtasks")
            .args(["/Delete", "/TN", TASK_NAME, "/F"])
            .output()
            .map_err(|e| AgentError::other(format!("schtasks is unavailable: {e}")))?;
        Ok(output.status.success())
    }

    pub fn status() -> Result<ScheduleStatus> {
        let output = Command::new("schtasks")
            .args(["/Query", "/TN", TASK_NAME, "/FO", "LIST"])
            .output()
            .map_err(|e| AgentError::other(format!("schtasks is unavailable: {e}")))?;

        if !output.status.success() {
            return Ok(ScheduleStatus {
                installed: false,
                mechanism: "Task Scheduler (current user)",
                detail: "no scheduled task".into(),
                path: None,
            });
        }
        Ok(ScheduleStatus {
            installed: true,
            mechanism: "Task Scheduler (current user)",
            detail: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            path: None,
        })
    }
}

// ===========================================================================
// Unsupported platforms
// ===========================================================================

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod platform {
    use super::*;

    fn unsupported() -> AgentError {
        AgentError::config(
            "built-in scheduling is implemented for Linux, macOS and Windows only; \
             use your platform's own per-user scheduler to run `spotify-agent --cron generate`",
        )
    }

    pub fn install(_spec: &ScheduleSpec) -> Result<ScheduleStatus> {
        Err(unsupported())
    }
    pub fn remove() -> Result<bool> {
        Err(unsupported())
    }
    pub fn status() -> Result<ScheduleStatus> {
        Err(unsupported())
    }
}

pub use platform::{install, remove, status};

/// Escape text destined for a plist. Only macOS emits XML.
#[cfg(target_os = "macos")]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(cadence: Cadence) -> ScheduleSpec {
        ScheduleSpec {
            cadence,
            hour: 7,
            minute: 30,
            preset: Some("focus".into()),
            binary: PathBuf::from("/home/u/.local/bin/spotify-agent"),
            extra_args: vec![],
        }
    }

    #[test]
    fn the_command_line_always_runs_headless() {
        let line = spec(Cadence::Daily).command_line();
        assert!(
            line.contains("--cron"),
            "a scheduled run must never try to open a TUI"
        );
        assert!(line.ends_with("generate focus"));
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        let mut s = spec(Cadence::Daily);
        s.binary = PathBuf::from("/Users/a b/bin/spotify-agent");
        assert!(
            s.command_line()
                .starts_with("\"/Users/a b/bin/spotify-agent\"")
        );
    }

    #[test]
    fn the_windows_extended_length_prefix_is_stripped() {
        // `canonicalize` hands back `\\?\C:\...` on Windows. Baking that into a
        // scheduled task's action is legal but not universally understood, and
        // it is what the user sees when they inspect the task.
        assert_eq!(
            strip_extended_prefix(PathBuf::from(r"\\?\C:\bin\spotify-agent.exe")),
            PathBuf::from(r"C:\bin\spotify-agent.exe")
        );
        // A UNC extended path must be left alone: shortening it changes what
        // it refers to.
        let unc = PathBuf::from(r"\\?\UNC\server\share\app.exe");
        assert_eq!(strip_extended_prefix(unc.clone()), unc);
        // Ordinary paths pass through untouched.
        let plain = PathBuf::from("/home/u/.local/bin/spotify-agent");
        assert_eq!(strip_extended_prefix(plain.clone()), plain);
    }

    #[test]
    fn cadence_parsing_is_case_insensitive() {
        assert_eq!(Cadence::parse("DAILY"), Some(Cadence::Daily));
        assert_eq!(Cadence::parse("never"), None);
    }

    #[test]
    fn systemd_calendar_expressions_are_well_formed() {
        assert_eq!(Cadence::Hourly.systemd_calendar(7, 30), "hourly");
        assert_eq!(Cadence::Daily.systemd_calendar(7, 30), "*-*-* 07:30:00");
        assert_eq!(Cadence::Weekly.systemd_calendar(7, 5), "Mon *-*-* 07:05:00");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_systemd_unit_has_the_sections_systemd_requires() {
        let service = platform::render_service(&spec(Cadence::Daily));
        assert!(service.contains("[Unit]") && service.contains("[Service]"));
        assert!(service.contains("Type=oneshot"));
        assert!(service.contains("--cron"));

        let timer = platform::render_timer(&spec(Cadence::Daily));
        assert!(timer.contains("[Timer]") && timer.contains("[Install]"));
        assert!(timer.contains("OnCalendar=*-*-* 07:30:00"));
        assert!(timer.contains("Persistent=true"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_plist_is_well_formed_and_escapes_xml() {
        let mut s = spec(Cadence::Daily);
        s.extra_args = vec!["--instructions".into(), "a & b < c".into()];
        let plist = platform::render_plist(&s);
        assert!(plist.starts_with("<?xml"));
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains("&amp;") && plist.contains("&lt;"));
        assert!(
            !plist.contains("a & b"),
            "raw ampersands would break the plist"
        );
    }
}
