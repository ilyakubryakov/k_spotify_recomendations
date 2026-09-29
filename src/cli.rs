//! Command-line interface.
//!
//! Two shapes are supported deliberately:
//!   * **interactive** — `spotify-agent` with no subcommand drops into the TUI
//!     when stdout is a terminal;
//!   * **automation** — `--cron` (or `generate --headless`) forces structured
//!     logs, no colour, no prompts, and a meaningful exit code.
//!
//! Every flag that overrides configuration is optional, so the precedence
//! chain (defaults → file → env → flags) stays honest: `None` means "do not
//! override", never "use the default".

use crate::config::{ColorMode, FillStrategy, LanguagePolicy, LogFormat};
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "spotify-agent",
    version,
    about = "Autonomous AI music curation for Spotify, powered by Claude",
    long_about = "Builds and maintains Spotify playlists by analysing your listening history \
locally and asking Claude to curate against it.\n\n\
Run without a subcommand to open the interactive TUI. Use --cron for unattended runs.",
    propagate_version = true
)]
pub struct Cli {
    /// Path to config.toml (default: the platform config dir).
    #[arg(short, long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Increase log verbosity (-v debug, -vv trace).
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Errors only.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Log output format.
    #[arg(long, global = true, value_enum)]
    pub log_format: Option<LogFormatArg>,

    /// When to colourise output.
    #[arg(long, global = true, value_enum)]
    pub color: Option<ColorArg>,

    /// Interface language for this run: en | ru | pl | lt.
    #[arg(long, global = true, value_name = "LANG")]
    pub lang: Option<String>,

    /// Unattended mode: JSON logs, no colour, never enters the TUI.
    #[arg(long, global = true)]
    pub cron: bool,

    /// Do not check for a new release on this run.
    #[arg(long, global = true)]
    pub no_update_check: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Authorise against Spotify (PKCE; opens a browser).
    Login {
        /// Print the URL instead of launching a browser — for headless hosts
        /// and SSH sessions.
        #[arg(long)]
        no_browser: bool,
    },

    /// Delete the stored Spotify tokens.
    Logout,

    /// Show configuration, authorisation and cache status.
    Status {
        #[arg(long)]
        json: bool,
    },

    /// Pull library, top items and recent plays into the local cache.
    Sync {
        /// Ignore the minimum sync interval.
        #[arg(long, short)]
        force: bool,
    },

    /// Show the computed taste profile.
    Profile {
        #[arg(long)]
        json: bool,
        /// How many rows per section.
        #[arg(long, default_value_t = 15)]
        top: usize,
    },

    /// List the available presets.
    Presets {
        #[arg(long)]
        json: bool,
    },

    /// Generate a playlist.
    Generate(Box<GenerateArgs>),

    /// Open the interactive TUI.
    Tui,

    /// Show what the agent has already recommended (its anti-repeat memory).
    Recommended {
        #[arg(long, default_value_t = 40)]
        limit: usize,
        /// Only this preset.
        #[arg(long, short)]
        preset: Option<String>,
        /// Include suggestions that were dropped before reaching a playlist.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },

    /// Forget recommendations so they become eligible again.
    Forget {
        /// Forget everything by this artist (repeatable).
        #[arg(long = "artist", value_name = "ARTIST", action = ArgAction::Append)]
        artists: Vec<String>,
        /// Forget the entire recommendation memory.
        #[arg(long, conflicts_with = "artists")]
        all: bool,
        /// Required with --all.
        #[arg(long)]
        yes: bool,
    },

    /// Manage the background schedule (systemd --user / launchd / Task Scheduler).
    Schedule {
        #[command(subcommand)]
        command: ScheduleCommand,
    },

    /// Export a tracklist to M3U8, CSV or JSON.
    Export(Box<ExportArgs>),

    /// List, inspect and restore playlist snapshots.
    Snapshots {
        #[command(subcommand)]
        command: SnapshotCommand,
    },

    /// Show what the feedback loop has learned.
    Feedback {
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },

    /// Ban an artist permanently.
    Ban {
        #[arg(value_name = "ARTIST", required = true)]
        artists: Vec<String>,
        /// Why, for your own records.
        #[arg(long)]
        reason: Option<String>,
    },

    /// Lift a ban.
    Unban {
        #[arg(value_name = "ARTIST", required = true)]
        artists: Vec<String>,
    },

    /// List banned artists.
    Bans {
        #[arg(long)]
        json: bool,
    },

    /// Show recent runs.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },

    /// Inspect or maintain the local cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },

    /// Inspect or scaffold the configuration file.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Check for a new release and install it in place.
    ///
    /// Downloads the release archive for this platform, verifies it against
    /// the release's published SHA256SUMS, and replaces this binary. Never
    /// needs root: it only writes where the binary already lives.
    Update(UpdateArgs),

    /// Print a shell completion script.
    Completions {
        #[arg(value_enum)]
        shell: clap_complete_shell::Shell,
    },
}

#[derive(Debug, Parser)]
pub struct GenerateArgs {
    /// Preset name (default: `defaults.preset` from the config).
    #[arg(value_name = "PRESET")]
    pub preset: Option<String>,

    /// Number of tracks in the final playlist.
    #[arg(short, long)]
    pub size: Option<usize>,

    /// Playlist name. Supports {preset}, {date}, {datetime}, {title}.
    #[arg(short, long, value_name = "NAME")]
    pub playlist: Option<String>,

    /// Overwrite the playlist or append to it.
    #[arg(long, value_enum)]
    pub strategy: Option<StrategyArg>,

    /// Language policy for the selection.
    #[arg(short, long, value_enum)]
    pub language: Option<LanguageArg>,

    /// Extra free-text steer for this run only.
    #[arg(short, long, value_name = "TEXT")]
    pub instructions: Option<String>,

    /// Restrict to these genres (repeatable).
    #[arg(long = "genre", value_name = "GENRE", action = ArgAction::Append)]
    pub genres: Vec<String>,

    /// Block an artist for this run (repeatable).
    #[arg(long = "block-artist", value_name = "ARTIST", action = ArgAction::Append)]
    pub block_artists: Vec<String>,

    /// Override the Claude model for this run.
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Resolve and filter, but write nothing to Spotify.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the library sync even if the cache is stale.
    #[arg(long)]
    pub no_sync: bool,

    /// Force non-interactive output even on a TTY.
    #[arg(long)]
    pub headless: bool,

    /// Print the result as JSON on stdout.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Parser)]
pub struct UpdateArgs {
    /// Report what is available and exit without downloading anything.
    #[arg(long)]
    pub check: bool,

    /// Install without asking.
    #[arg(long, short)]
    pub yes: bool,

    /// Install this exact tag instead of the newest release. Allows going
    /// backwards, which is the point: it is how you undo a bad upgrade.
    #[arg(long, value_name = "TAG")]
    pub to: Option<String>,

    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum ScheduleCommand {
    /// Register a recurring background run for the current user.
    ///
    /// Never requires root/administrator: it writes a systemd --user timer on
    /// Linux, a LaunchAgent on macOS, and a current-user task on Windows.
    Install {
        /// hourly | daily | weekly
        #[arg(long, default_value = "daily")]
        every: String,
        /// Local time for daily/weekly runs, HH:MM.
        #[arg(long, default_value = "07:30")]
        at: String,
        /// Preset to generate.
        #[arg(long, short)]
        preset: Option<String>,
    },
    /// Show whether a schedule is installed and when it next runs.
    Status,
    /// Remove the schedule.
    Remove,
}

#[derive(Debug, Parser)]
pub struct ExportArgs {
    /// Playlist name to export (fetched live from Spotify).
    #[arg(long, short, value_name = "NAME", conflicts_with_all = ["snapshot", "last"])]
    pub playlist: Option<String>,

    /// Export a stored snapshot by id.
    #[arg(long, value_name = "ID", conflicts_with_all = ["playlist", "last"])]
    pub snapshot: Option<i64>,

    /// Export the most recent generated run.
    #[arg(long, conflicts_with_all = ["playlist", "snapshot"])]
    pub last: bool,

    /// m3u8 | csv | json
    #[arg(long, short, default_value = "m3u8")]
    pub format: String,

    /// Output file. Defaults to a name derived from the playlist; `-` is stdout.
    #[arg(long, short, value_name = "PATH")]
    pub out: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum SnapshotCommand {
    /// List stored snapshots.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Print the contents of one snapshot.
    Show {
        id: i64,
        #[arg(long)]
        json: bool,
    },
    /// Write a snapshot's contents back to its playlist.
    Restore {
        id: i64,
        /// Required: this overwrites the playlist's current contents.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Row counts and database size.
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Apply retention and VACUUM.
    Prune,
    /// Delete every cached row. Stored Spotify tokens are untouched.
    Clear {
        /// Required: this is destructive and irreversible.
        #[arg(long)]
        yes: bool,
    },
    /// Print the database path.
    Path,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the config file path.
    Path,
    /// Show or set the saved interface language (en | ru | pl | lt).
    Language {
        /// Omit to show the current setting.
        value: Option<String>,
    },
    /// Print the effective configuration (secrets redacted).
    Show,
    /// Write a fully commented starter config.
    Init {
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
}

// ---------------------------------------------------------------------------
// Value enums — kept separate from the config enums so clap's derive does not
// dictate the config file's serde representation.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum StrategyArg {
    Replace,
    Append,
}

impl From<StrategyArg> for FillStrategy {
    fn from(v: StrategyArg) -> Self {
        match v {
            StrategyArg::Replace => FillStrategy::Replace,
            StrategyArg::Append => FillStrategy::Append,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LanguageArg {
    English,
    Russian,
    Mixed,
    Any,
}

impl From<LanguageArg> for LanguagePolicy {
    fn from(v: LanguageArg) -> Self {
        match v {
            LanguageArg::English => LanguagePolicy::English,
            LanguageArg::Russian => LanguagePolicy::Russian,
            LanguageArg::Mixed => LanguagePolicy::Mixed,
            LanguageArg::Any => LanguagePolicy::Any,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LogFormatArg {
    Text,
    Json,
}

impl From<LogFormatArg> for LogFormat {
    fn from(v: LogFormatArg) -> Self {
        match v {
            LogFormatArg::Text => LogFormat::Text,
            LogFormatArg::Json => LogFormat::Json,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ColorArg {
    Auto,
    Always,
    Never,
}

impl From<ColorArg> for ColorMode {
    fn from(v: ColorArg) -> Self {
        match v {
            ColorArg::Auto => ColorMode::Auto,
            ColorArg::Always => ColorMode::Always,
            ColorArg::Never => ColorMode::Never,
        }
    }
}

impl Cli {
    /// Tracing directive implied by the verbosity flags, if any.
    pub fn log_directive(&self) -> Option<&'static str> {
        if self.quiet {
            return Some("error");
        }
        match self.verbose {
            0 => None,
            1 => Some("spotify_agent=debug,info"),
            _ => Some("spotify_agent=trace,debug"),
        }
    }

    /// True when the process must not take over the terminal.
    pub fn is_headless(&self) -> bool {
        if self.cron {
            return true;
        }
        match &self.command {
            Some(Command::Generate(args)) => args.headless || args.json,
            Some(Command::Tui) | None => false,
            // Every other subcommand is a one-shot report.
            Some(_) => true,
        }
    }
}

/// Re-exported so `main` does not need a direct `clap_complete` import.
pub mod clap_complete_shell {
    pub use clap_complete::Shell;
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cron_forces_headless() {
        let cli = Cli::try_parse_from(["spotify-agent", "--cron", "tui"]).expect("parses");
        assert!(cli.is_headless());
    }

    #[test]
    fn bare_invocation_is_interactive() {
        let cli = Cli::try_parse_from(["spotify-agent"]).expect("parses");
        assert!(!cli.is_headless());
        assert!(cli.command.is_none());
    }

    #[test]
    fn generate_flags_parse() {
        let cli = Cli::try_parse_from([
            "spotify-agent",
            "generate",
            "road_trip",
            "--size",
            "50",
            "--language",
            "mixed",
            "--genre",
            "post-punk",
            "--genre",
            "shoegaze",
            "--block-artist",
            "Nickelback",
            "--dry-run",
        ])
        .expect("parses");
        let Some(Command::Generate(args)) = cli.command else {
            panic!("expected generate");
        };
        assert_eq!(args.preset.as_deref(), Some("road_trip"));
        assert_eq!(args.size, Some(50));
        assert_eq!(args.genres.len(), 2);
        assert!(args.dry_run);
    }
}
