//! `spotify-agent` — autonomous AI music curation.
//!
//! `main` does four things and nothing else: parse, configure, dispatch, and
//! turn an error into an exit code. All work lives behind [`engine`].
//!
//! Exit codes follow `sysexits.h` so a cron wrapper can distinguish "retry
//! later" (75) from "fix your config" (78):
//!
//! | code | meaning                             |
//! |------|-------------------------------------|
//! | 0    | success                             |
//! | 1    | unclassified failure                |
//! | 69   | upstream unavailable / model refusal|
//! | 75   | temporary failure — retry later     |
//! | 77   | not authorised — run `login`        |
//! | 78   | configuration error                 |
//! | 130  | interrupted (Ctrl-C)                |

use clap::Parser;
use spotify_agent::cli::{Cli, Command};
use spotify_agent::config::{ColorMode, Config, LogFormat};
use spotify_agent::error::{AgentError, Result};
use spotify_agent::{commands, telemetry, tui};
use std::sync::Arc;

fn main() -> std::process::ExitCode {
    restore_default_sigpipe();

    // The runtime is built by hand rather than via `#[tokio::main]` so that a
    // failure to build it is reported like any other error instead of a panic.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    let code = runtime.block_on(run());
    // Dropping the runtime here (rather than at process exit) makes sure
    // in-flight SQLite blocking tasks finish before the process goes away.
    drop(runtime);
    std::process::ExitCode::from(code as u8)
}

/// Rust's runtime sets `SIGPIPE` to `SIG_IGN`, which turns a closed downstream
/// pipe into an `EPIPE` write error — and `println!` *panics* on a write error.
/// The visible symptom is `spotify-agent presets | head` aborting with
/// "failed printing to stdout: Broken pipe".
///
/// Restoring the default disposition makes the process terminate silently on
/// `SIGPIPE`, which is what every other Unix CLI does and what `head` expects.
#[cfg(unix)]
fn restore_default_sigpipe() {
    // SAFETY: called once, at the very top of `main`, before the runtime is
    // built and before any thread exists — so there is no concurrent signal
    // handler installation to race with. `SIG_DFL` is a constant disposition,
    // not a handler function, so no async-signal-safety obligations follow.
    #[allow(unsafe_code)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// Windows has no `SIGPIPE`; a closed pipe surfaces as an ordinary I/O error.
#[cfg(not(unix))]
fn restore_default_sigpipe() {}

async fn run() -> i32 {
    // Sweep up a binary an earlier update had to leave behind. Only Windows
    // can produce one (a running image cannot be deleted, only renamed), and
    // it is a no-op everywhere else.
    //
    // Before `Cli::parse`, deliberately: clap exits inside `parse` for
    // `--version` and `--help`, and those are exactly what someone runs
    // straight after an update to check it worked.
    spotify_agent::update::install::cleanup_leftovers();

    let cli = Cli::parse();

    // `completions` must work before any config exists.
    if let Some(Command::Completions { shell }) = &cli.command {
        commands::print_completions(*shell);
        return 0;
    }

    let config = match Config::load(cli.config.as_deref()).map(|mut c| {
        // A --lang flag overrides both the config file and the saved
        // preference, for this process only.
        if let Some(raw) = cli.lang.as_deref() {
            match spotify_agent::i18n::Lang::parse(raw) {
                Some(lang) => c.general.language = Some(lang),
                None => c
                    .warnings
                    .push(format!("ignoring --lang {raw}: expected en, ru, pl or lt")),
            }
        }
        c
    }) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("error: {e}");
            return e.exit_code();
        }
    };

    let headless = cli.is_headless();
    let log_format = resolve_log_format(&cli, &config);
    let color = cli.color.map(Into::into).unwrap_or(config.general.color);
    let directive = cli
        .log_directive()
        .map(str::to_string)
        .unwrap_or_else(|| config.general.log_level.clone());

    let log_buffer = match telemetry::init(telemetry::TelemetryOptions {
        directive: &directive,
        format: log_format,
        color,
        tui: !headless && matches!(cli.command, None | Some(Command::Tui)),
    }) {
        Ok(buffer) => buffer,
        Err(e) => {
            eprintln!("error: {e}");
            return e.exit_code();
        }
    };

    // Replayed now that a subscriber exists — see `Config::warnings`.
    for warning in &config.warnings {
        tracing::warn!("{warning}");
    }

    // Decided before `dispatch` consumes the parsed arguments.
    let update_notice = UpdateNotice::for_run(&cli);

    // Ctrl-C must leave the terminal usable and the exit code honest. The
    // select below cancels the in-flight command; because every network call
    // is cancellation-safe at an await point, the worst case is an
    // unfinished sync, which the next run repeats.
    let outcome = tokio::select! {
        result = dispatch(cli, Arc::clone(&config), color, log_buffer) => result,
        _ = tokio::signal::ctrl_c() => Err(AgentError::Cancelled),
    };

    // Only after the work is done, and never in its way.
    if outcome.is_ok() {
        update_notice.run(&config).await;
    }

    match outcome {
        Ok(()) => 0,
        Err(AgentError::Cancelled) => {
            eprintln!("\ninterrupted");
            130
        }
        Err(e) => {
            // Logged as structured data for cron, and printed plainly for a
            // human who may have logging turned down.
            tracing::error!(error = %e, "command failed");
            eprintln!("error: {e}");
            if let Some(hint) = hint_for(&e) {
                eprintln!("hint: {hint}");
            }
            e.exit_code()
        }
    }
}

fn resolve_log_format(cli: &Cli, config: &Config) -> LogFormat {
    if let Some(format) = cli.log_format {
        return format.into();
    }
    if cli.cron {
        // Unattended runs go to a log aggregator far more often than to a
        // human, so structured output is the right default there.
        return LogFormat::Json;
    }
    config.general.log_format
}

async fn dispatch(
    cli: Cli,
    config: Arc<Config>,
    color: ColorMode,
    log_buffer: Option<telemetry::LogBuffer>,
) -> Result<()> {
    let use_color = telemetry::use_color(color);

    match cli.command {
        None => {
            if cli.cron {
                return Err(AgentError::config(
                    "--cron needs a subcommand, e.g. `spotify-agent --cron generate`",
                ));
            }
            tui::run(config, log_buffer).await
        }
        Some(Command::Tui) => tui::run(config, log_buffer).await,

        Some(Command::Login { no_browser }) => commands::login(config, no_browser).await,
        Some(Command::Logout) => commands::logout(config).await,
        Some(Command::Status { json }) => commands::status(config, json, use_color).await,
        Some(Command::Sync { force }) => commands::sync(config, force, use_color).await,
        Some(Command::Profile { json, top }) => {
            commands::profile(config, json, top, use_color).await
        }
        Some(Command::Presets { json }) => commands::presets(config, json, use_color),
        Some(Command::Generate(args)) => commands::generate(config, *args, use_color).await,
        Some(Command::Schedule { command }) => commands::schedule(config, command, use_color).await,
        Some(Command::Export(args)) => commands::export(config, *args).await,
        Some(Command::Snapshots { command }) => {
            commands::snapshots(config, command, use_color).await
        }
        Some(Command::Feedback { limit, json }) => {
            commands::feedback(config, limit, json, use_color).await
        }
        Some(Command::Ban { artists, reason }) => commands::ban(config, artists, reason).await,
        Some(Command::Unban { artists }) => commands::unban(config, artists).await,
        Some(Command::Bans { json }) => commands::bans(config, json).await,
        Some(Command::Recommended {
            limit,
            preset,
            all,
            json,
        }) => commands::recommended(config, limit, preset, all, json, use_color).await,
        Some(Command::Forget { artists, all, yes }) => {
            commands::forget(config, artists, all, yes).await
        }
        Some(Command::History { limit, json }) => commands::history(config, limit, json).await,
        Some(Command::Cache { command }) => commands::cache(config, command).await,
        Some(Command::Config { command }) => commands::config_cmd(config, command),
        Some(Command::Update(args)) => commands::update(config, args, use_color).await,
        Some(Command::Completions { .. }) => Ok(()), // handled earlier
    }
}

/// Whether — and how — to mention a newer release once the command is done.
///
/// Three rules keep this from ever being in the way:
///   * it runs *after* the command succeeded, so a slow or unreachable GitHub
///     cannot delay or fail the work the user actually asked for;
///   * it writes to stderr, so `--json` output stays machine-readable;
///   * it stays quiet unless stderr is a terminal, so pipes, logs and CI
///     transcripts are untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateNotice {
    Skip,
    /// Structured log line, and an install only if the config asked for one.
    Unattended,
    /// One line on stderr.
    Interactive,
}

impl UpdateNotice {
    fn for_run(cli: &Cli) -> Self {
        if cli.no_update_check || cli.quiet {
            return Self::Skip;
        }
        // The TUI has its own dialog and `update` is the check; `completions`
        // must stay pure so its output can be sourced.
        if matches!(
            cli.command,
            None | Some(Command::Tui)
                | Some(Command::Update(_))
                | Some(Command::Completions { .. })
        ) {
            return Self::Skip;
        }
        if cli.cron {
            return Self::Unattended;
        }
        Self::Interactive
    }

    async fn run(self, config: &std::sync::Arc<Config>) {
        use std::io::IsTerminal;

        match self {
            Self::Skip => {}
            Self::Unattended => spotify_agent::update::unattended(config).await,
            Self::Interactive => {
                if !std::io::stderr().is_terminal() {
                    return;
                }
                if let Some(notice) = spotify_agent::update::passive_notice(config).await {
                    eprintln!("\n{notice}");
                }
            }
        }
    }
}

/// Actionable next step for the common failure modes.
fn hint_for(error: &AgentError) -> Option<&'static str> {
    match error {
        AgentError::NotAuthorized | AgentError::Auth(_) => Some("run `spotify-agent login`"),
        AgentError::MissingCredential { name, .. } if name.contains("anthropic") => {
            Some("export ANTHROPIC_API_KEY=… or set claude.api_key in the config")
        }
        AgentError::MissingCredential { .. } => {
            Some("run `spotify-agent config init` and fill in spotify.client_id")
        }
        AgentError::RateLimited { .. } | AgentError::RetriesExhausted { .. } => {
            Some("the upstream API is rate limiting; retry in a few minutes")
        }
        AgentError::ModelRefusal { .. } => {
            Some("rephrase the preset brief, or configure claude.fallback_models")
        }
        _ => None,
    }
}
