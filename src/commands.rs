//! Command implementations and terminal output.
//!
//! Everything here is presentation: the work happens in [`crate::engine`].
//! Two output modes are supported per command — a human table and `--json` —
//! and the JSON form always goes to stdout alone so it can be piped.

use crate::cli::{
    CacheCommand, ConfigCommand, ExportArgs, GenerateArgs, ScheduleCommand, SnapshotCommand,
};
use crate::config::{Config, paths};
use crate::domain::TasteProfile;
use crate::engine::{
    Engine, EngineEvent, EventSink, GenerateOptions, RunOutcome, Stage, SyncReport,
};
use crate::error::{AgentError, Result};
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Starter config, kept in one place so the repo file and `config init` agree.
const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");

// ===========================================================================
// Styling
// ===========================================================================

struct Style {
    enabled: bool,
}

impl Style {
    fn new(enabled: bool) -> Self {
        Self { enabled }
    }
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
    fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }
    fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }
    fn green(&self, text: &str) -> String {
        self.wrap("32", text)
    }
    fn yellow(&self, text: &str) -> String {
        self.wrap("33", text)
    }
    fn cyan(&self, text: &str) -> String {
        self.wrap("36", text)
    }
    fn red(&self, text: &str) -> String {
        self.wrap("31", text)
    }
    fn heading(&self, text: &str) -> String {
        self.bold(&self.cyan(text))
    }
}

/// Truncate to a display width, appending an ellipsis. Counts chars rather
/// than bytes so Cyrillic and CJK titles do not get cut mid-codepoint.
fn fit(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return format!("{text}{}", " ".repeat(width - count));
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn print_json(value: serde_json::Value) -> Result<()> {
    let rendered = serde_json::to_string_pretty(&value)?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{rendered}").map_err(|e| AgentError::io("stdout", e))
}

// ===========================================================================
// completions
// ===========================================================================

pub fn print_completions(shell: clap_complete::Shell) {
    use clap::CommandFactory;
    let mut command = crate::cli::Cli::command();
    let name = command.get_name().to_string();
    clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
}

// ===========================================================================
// auth
// ===========================================================================

pub async fn login(config: Arc<Config>, no_browser: bool) -> Result<()> {
    let handle = Engine::build_spotify_only(config).await?;
    handle.auth.login(no_browser).await?;

    // A first login with an empty cache is useless on its own — the next
    // command would just have to sync anyway. Doing it here makes `login`
    // leave the tool in a usable state.
    println!("\nRunning an initial sync…");
    let report = handle.sync(true, &None).await?;
    println!(
        "  {} saved tracks, {} top tracks, {} artists, {} plays recorded.",
        report.saved, report.top_tracks, report.artists_hydrated, report.new_plays
    );
    Ok(())
}

pub async fn logout(config: Arc<Config>) -> Result<()> {
    let handle = Engine::build_spotify_only(config).await?;
    handle.auth.logout().await?;
    println!("Stored Spotify tokens removed.");
    Ok(())
}

// ===========================================================================
// status
// ===========================================================================

pub async fn status(config: Arc<Config>, as_json: bool, color: bool) -> Result<()> {
    let s = Style::new(color);

    let config_path = config
        .source_path
        .clone()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| {
            format!(
                "{} (not created)",
                paths::config_file()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<unknown>".into())
            )
        });

    let has_client_id = config.spotify.client_id.is_some();
    let has_api_key = config.claude.resolve_api_key().is_ok();

    // Build the Spotify handle only when it can succeed; a missing client_id
    // must not turn `status` — the command you run to find that out — into an
    // error.
    let (authorized, token_expiry, cache, missing_scopes) = if has_client_id {
        match Engine::build_spotify_only(Arc::clone(&config)).await {
            Ok(handle) => {
                let authorized = handle.auth.is_authorized().await;
                let expiry = handle.auth.status().await.map(|(at, _)| at);
                let cache = handle.storage.stats().await.ok();
                let missing = handle.auth.missing_scopes().await;
                (authorized, expiry, cache, missing)
            }
            Err(_) => (false, None, None, Vec::new()),
        }
    } else {
        (false, None, None, Vec::new())
    };

    // Read from disk only: `status` reports what is known, and must not make
    // a network call the user did not ask for.
    let update_state = crate::update::state::UpdateState::load(&config.data_dir()?);

    if as_json {
        return print_json(json!({
            "config_path": config_path,
            "data_dir": config.data_dir()?.display().to_string(),
            "database": config.database_path()?.display().to_string(),
            "spotify": {
                "client_id_configured": has_client_id,
                "authorized": authorized,
                "missing_scopes": missing_scopes,
                "token_expires_at": token_expiry.map(|t| t.to_rfc3339()),
                "redirect_uri": config.spotify.redirect_uri(),
            },
            "claude": {
                "api_key_configured": has_api_key,
                "model": config.claude.model,
                "effort": config.claude.effort.as_str(),
                "streaming": config.claude.stream,
            },
            "cache": cache.as_ref().map(|c| json!({
                "tracks": c.tracks,
                "artists": c.artists,
                "saved": c.saved,
                "plays": c.plays,
                "runs": c.runs,
                "db_bytes": c.db_bytes,
                "last_sync": c.last_sync.map(|t| t.to_rfc3339()),
            })),
            "presets": config.preset_names(),
            "version": crate::VERSION,
            "update": {
                "enabled": config.update.enabled,
                "target": crate::update::install::target_triple(),
                "latest_seen": update_state.latest_seen,
                "last_check": update_state.last_check.map(|t| t.to_rfc3339()),
            },
        }));
    }

    let mark = |ok: bool| if ok { s.green("✓") } else { s.red("✗") };

    println!("{}", s.heading("spotify-agent"));
    println!("  config     {}", s.dim(&config_path));
    println!(
        "  data dir   {}",
        s.dim(&config.data_dir()?.display().to_string())
    );
    println!();
    println!("{}", s.heading("Spotify"));
    println!("  {} client id configured", mark(has_client_id));
    println!("  {} authorised", mark(authorized));
    if let Some(expiry) = token_expiry {
        println!(
            "      token valid until {}",
            s.dim(&expiry.format("%Y-%m-%d %H:%M UTC").to_string())
        );
    }
    println!("      redirect {}", s.dim(&config.spotify.redirect_uri()));
    if !missing_scopes.is_empty() {
        // A grant from an older version is missing permissions the current one
        // needs. Nothing looks wrong until an operation 403s, so say it here.
        println!(
            "  {} grant is missing {}",
            s.yellow("!"),
            s.yellow(&missing_scopes.join(", "))
        );
        println!(
            "      re-authorise to enable it: {}",
            s.bold("spotify-agent login")
        );
    }
    println!();
    println!("{}", s.heading("Claude"));
    println!("  {} API key available", mark(has_api_key));
    println!("      model   {}", s.bold(&config.claude.model));
    println!(
        "      effort  {}   thinking {}   streaming {}",
        config.claude.effort.as_str(),
        if matches!(
            config.claude.thinking,
            crate::config::ThinkingMode::Adaptive
        ) {
            "adaptive"
        } else {
            "disabled"
        },
        config.claude.stream
    );
    println!();
    println!("{}", s.heading("Cache"));
    match cache {
        Some(c) => {
            println!(
                "  {} tracks · {} artists · {} liked · {} plays · {} runs",
                c.tracks, c.artists, c.saved, c.plays, c.runs
            );
            println!("  {:.1} MB on disk", c.db_bytes as f64 / 1_048_576.0);
            match c.last_sync {
                Some(t) => println!(
                    "  last sync {}",
                    s.dim(&t.format("%Y-%m-%d %H:%M UTC").to_string())
                ),
                None => println!("  {}", s.yellow("never synced — run `spotify-agent sync`")),
            }
        }
        None => println!("  {}", s.dim("unavailable")),
    }
    println!();
    println!("{}", s.heading("Presets"));
    println!("  {}", config.preset_names().join(", "));
    println!();
    println!("{}", s.heading("Version"));
    println!(
        "  {}{}",
        s.bold(crate::VERSION),
        crate::update::install::target_triple()
            .map(|t| format!("  {}", s.dim(t)))
            .unwrap_or_else(|| format!("  {}", s.dim("built from source for this platform")))
    );
    match (&update_state.latest_seen, update_state.last_check) {
        (Some(latest), Some(checked)) => println!(
            "  latest release seen {} {}",
            s.bold(latest),
            s.dim(&format!(
                "(checked {})",
                checked.format("%Y-%m-%d %H:%M UTC")
            ))
        ),
        _ => println!("  {}", s.dim("no release check has run yet")),
    }
    if !config.update.enabled {
        println!("  {}", s.dim("automatic checks are turned off"));
    }
    Ok(())
}

// ===========================================================================
// sync
// ===========================================================================

pub async fn sync(config: Arc<Config>, force: bool, color: bool) -> Result<()> {
    let s = Style::new(color);
    let handle = Engine::build_spotify_only(config).await?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let printer = tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if let EngineEvent::Log(message) = event {
                println!("  {message}");
            }
        }
    });

    let report = handle.sync(force, &Some(tx)).await?;
    let _ = printer.await;

    if report.skipped {
        println!(
            "{}",
            s.dim("library is already fresh (use --force to sync anyway)")
        );
        return Ok(());
    }

    println!();
    println!("{}", s.heading("Sync complete"));
    print_sync_report(&report, &s);
    Ok(())
}

fn print_sync_report(report: &SyncReport, s: &Style) {
    println!("  saved tracks     {}", s.bold(&report.saved.to_string()));
    println!("  top tracks       {}", report.top_tracks);
    println!("  top artists      {}", report.top_artists);
    println!("  new plays        {}", report.new_plays);
    println!("  artists hydrated {}", report.artists_hydrated);
    println!("  took             {:.1}s", report.duration.as_secs_f32());
}

// ===========================================================================
// profile
// ===========================================================================

pub async fn profile(config: Arc<Config>, as_json: bool, top: usize, color: bool) -> Result<()> {
    let handle = Engine::build_spotify_only(config).await?;
    let profile = handle.profile().await?;

    if profile.is_empty() {
        return Err(AgentError::other(
            "the cache is empty — run `spotify-agent sync` first",
        ));
    }

    if as_json {
        return print_json(serde_json::to_value(&profile)?);
    }

    print_profile(&profile, top, &Style::new(color));
    Ok(())
}

fn print_profile(profile: &TasteProfile, top: usize, s: &Style) {
    println!("{}", s.heading("Taste profile"));
    println!(
        "  {} tracks · {} artists · {} liked · {} plays ({} distinct)",
        profile.known_tracks,
        profile.known_artists,
        profile.saved_tracks,
        profile.play_events,
        profile.distinct_played
    );
    println!("  era        {}", profile.era.describe());
    println!("  languages  {}", profile.script_mix.describe());
    println!(
        "  typical    popularity {}/100, {}:{:02} long",
        profile.median_popularity,
        profile.median_duration_secs / 60,
        profile.median_duration_secs % 60
    );

    if !profile.genres.is_empty() {
        println!("\n{}", s.heading("Top genres"));
        for genre in profile.genres.iter().take(top) {
            println!(
                "  {} {}",
                fit(&genre.genre, 32),
                s.dim(&format!("{} artists", genre.artist_count))
            );
        }
    }

    if !profile.top_artists.is_empty() {
        println!("\n{}", s.heading("Core artists"));
        for artist in profile.top_artists.iter().take(top) {
            println!(
                "  {} {} {}",
                fit(&artist.name, 32),
                s.dim(&format!("{:>5.1}", artist.score)),
                s.dim(
                    &artist
                        .genres
                        .iter()
                        .take(2)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            );
        }
    }

    if !profile.looped.is_empty() {
        println!("\n{}", s.heading("On repeat (30 days)"));
        for track in profile.looped.iter().take(top) {
            println!(
                "  {} {} {}",
                fit(&track.artist, 26),
                fit(&track.name, 34),
                s.dim(&format!("{}×", track.recent_plays))
            );
        }
    }
}

// ===========================================================================
// presets
// ===========================================================================

pub fn presets(config: Arc<Config>, as_json: bool, color: bool) -> Result<()> {
    if as_json {
        let items: Vec<serde_json::Value> = config
            .preset_names()
            .into_iter()
            .filter_map(|name| config.resolve_preset(&name).ok())
            .map(|p| {
                json!({
                    "name": p.name,
                    "label": p.label,
                    "brief": p.brief,
                    "size": p.run.size,
                    "language": p.run.language.as_str(),
                    "strategy": format!("{:?}", p.run.strategy).to_lowercase(),
                    "max_per_artist": p.run.max_per_artist,
                })
            })
            .collect();
        return print_json(json!(items));
    }

    let s = Style::new(color);
    println!("{}", s.heading("Presets"));
    for name in config.preset_names() {
        let Ok(preset) = config.resolve_preset(&name) else {
            continue;
        };
        let default_marker = if name == config.defaults.preset {
            s.green(" (default)")
        } else {
            String::new()
        };
        println!("\n  {}{}", s.bold(&name), default_marker);
        println!("  {}", s.dim(&preset.label));
        println!(
            "  {} tracks · {} · max {}/artist",
            preset.run.size,
            preset.run.language.as_str(),
            preset.run.max_per_artist
        );
    }
    Ok(())
}

// ===========================================================================
// generate
// ===========================================================================

pub async fn generate(config: Arc<Config>, args: GenerateArgs, color: bool) -> Result<()> {
    let s = Style::new(color);

    // A model override applies to this process only; it never touches the
    // config file.
    let config = match &args.model {
        Some(model) => {
            let mut c = (*config).clone();
            c.claude.model = model.clone();
            Arc::new(c)
        }
        None => config,
    };

    let engine = Engine::build(Arc::clone(&config)).await?;
    if !engine.auth.is_authorized().await {
        return Err(AgentError::NotAuthorized);
    }

    let opts = GenerateOptions {
        preset: args.preset.clone(),
        size: args.size,
        playlist_name: args.playlist.clone(),
        strategy: args.strategy.map(Into::into),
        language: args.language.map(Into::into),
        extra_instructions: args.instructions.clone(),
        dry_run: args.dry_run,
        skip_sync: args.no_sync,
        block_artists: args.block_artists.clone(),
        genres: args.genres.clone(),
    };

    // In JSON mode the progress narration would corrupt stdout, so the sink is
    // dropped and the pipeline reports through `tracing` to stderr instead.
    let sink: EventSink = if args.json {
        None
    } else {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let style = Style::new(color);
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    EngineEvent::Stage(stage) if stage != Stage::Done => {
                        println!(
                            "{} {}",
                            style.dim(&format!("[{}/{}]", stage.ordinal() + 1, Stage::COUNT)),
                            style.bold(stage.label())
                        );
                    }
                    EngineEvent::Log(message) => println!("      {}", style.dim(&message)),
                    EngineEvent::Rejected(what, why) => {
                        tracing::debug!(track = %what, reason = %why, "dropped");
                    }
                    _ => {}
                }
            }
        });
        Some(tx)
    };

    let outcome = match engine.generate(opts, sink).await {
        Ok(outcome) => outcome,
        Err(e) => {
            // Announce the failure before propagating: an unattended run that
            // fails silently is the whole reason notifications exist.
            notify_run(&config, None, Some(&e)).await;
            return Err(e);
        }
    };
    notify_run(&config, Some(&outcome), None).await;

    if args.json {
        return print_json(outcome_json(&outcome));
    }

    print_outcome(&outcome, &s);
    Ok(())
}

fn outcome_json(outcome: &RunOutcome) -> serde_json::Value {
    json!({
        "run_id": outcome.run_id,
        "preset": outcome.preset,
        "model": outcome.model,
        "dry_run": outcome.dry_run,
        "requested": outcome.requested,
        "suggested": outcome.suggested,
        "accepted": outcome.accepted.len(),
        "rejected": outcome.rejected.len(),
        "duration_secs": outcome.duration.as_secs_f32(),
        "usage": {"input_tokens": outcome.input_tokens, "output_tokens": outcome.output_tokens},
        "summary": outcome.summary,
        "playlist": outcome.playlist.as_ref().map(|p| json!({
            "id": p.id, "name": p.name, "url": p.web_url(), "uri": p.uri(),
        })),
        "tracks": outcome.accepted.iter().map(|item| json!({
            "id": item.track.id,
            "uri": item.track.uri(),
            "title": item.track.name,
            "artist": item.track.artist_line(),
            "album": item.track.album,
            "duration_ms": item.track.duration_ms,
            "popularity": item.track.popularity,
            "match_score": item.match_score,
            "reason": item.suggestion.reason,
            "mood": item.suggestion.mood,
        })).collect::<Vec<_>>(),
        "dropped": outcome.rejected.iter().map(|item| json!({
            "title": item.suggestion.title,
            "artist": item.suggestion.artist,
            "reason": item.reason.label(),
        })).collect::<Vec<_>>(),
    })
}

fn print_outcome(outcome: &RunOutcome, s: &Style) {
    println!();
    if outcome.accepted.is_empty() {
        println!(
            "{}",
            s.yellow("No tracks survived resolution and filtering.")
        );
    } else {
        println!("{}", s.heading(&outcome.playlist_title));
        if !outcome.summary.trim().is_empty() {
            println!("{}\n", s.dim(outcome.summary.trim()));
        }
        for (index, item) in outcome.accepted.iter().enumerate() {
            println!(
                "  {:>3}. {} {} {}",
                index + 1,
                fit(&item.track.artist_line(), 26),
                fit(&item.track.name, 36),
                s.dim(&item.track.duration_display())
            );
            if !item.suggestion.reason.trim().is_empty() {
                println!("       {}", s.dim(item.suggestion.reason.trim()));
            }
        }
    }

    println!();
    println!(
        "{} {} suggested → {} resolved → {} written{}",
        s.heading("Result"),
        outcome.suggested,
        outcome.accepted.len(),
        outcome.accepted.len(),
        if outcome.dry_run {
            s.yellow(" (dry run — nothing written)")
        } else {
            String::new()
        }
    );

    if !outcome.rejected.is_empty() {
        let mut reasons: std::collections::BTreeMap<String, usize> = Default::default();
        for item in &outcome.rejected {
            *reasons.entry(item.reason.label()).or_insert(0) += 1;
        }
        let breakdown = reasons
            .into_iter()
            .map(|(reason, count)| format!("{count} {reason}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!("{} {}", s.dim("dropped:"), s.dim(&breakdown));
    }

    println!(
        "{} {} · {} in / {} out tokens · {:.1}s",
        s.dim("model:"),
        s.dim(&outcome.model),
        outcome.input_tokens,
        outcome.output_tokens,
        outcome.duration.as_secs_f32()
    );

    if let Some(playlist) = &outcome.playlist {
        println!("{} {}", s.dim("playlist:"), s.cyan(&playlist.web_url()));
    }
}

// ===========================================================================
// recommended / forget — the anti-repeat memory
// ===========================================================================

pub async fn recommended(
    config: Arc<Config>,
    limit: usize,
    preset: Option<String>,
    include_dropped: bool,
    as_json: bool,
    color: bool,
) -> Result<()> {
    let storage = crate::storage::Storage::open(&config.database_path()?).await?;
    let rows = storage
        .recommendations(limit, preset, !include_dropped)
        .await?;

    if as_json {
        return print_json(json!(
            rows.iter()
                .map(|r| json!({
                    "artist": r.artist,
                    "title": r.title,
                    "preset": r.preset,
                    "mood": r.mood,
                    "reason": r.reason,
                    "accepted": r.accepted,
                    "reject_note": r.reject_note,
                    "created_at": r.created_at.to_rfc3339(),
                }))
                .collect::<Vec<_>>()
        ));
    }

    let s = Style::new(color);
    if rows.is_empty() {
        println!("Nothing recommended yet.");
        return Ok(());
    }

    let cooldown = config.defaults.artist_cooldown_days;
    let window = match config.defaults.exclude_recent_days {
        0 => "forever".to_string(),
        days => format!("{days} days"),
    };
    println!(
        "{} {}",
        s.heading("Recommendation memory"),
        s.dim(&format!(
            "(tracks excluded for {window}; artists on a {cooldown}-day cooldown)"
        ))
    );

    for row in &rows {
        let marker = if row.accepted {
            s.green("+")
        } else {
            s.dim("–")
        };
        println!(
            "  {} {} {} {} {}",
            marker,
            s.dim(&row.created_at.format("%Y-%m-%d").to_string()),
            fit(&row.artist, 24),
            fit(&row.title, 32),
            s.dim(
                &row.reject_note
                    .clone()
                    .unwrap_or_else(|| row.preset.clone())
            )
        );
    }
    Ok(())
}

pub async fn forget(config: Arc<Config>, artists: Vec<String>, all: bool, yes: bool) -> Result<()> {
    if artists.is_empty() && !all {
        return Err(AgentError::other(
            "nothing to forget: pass --artist <NAME> (repeatable) or --all",
        ));
    }
    let storage = crate::storage::Storage::open(&config.database_path()?).await?;

    if all {
        if !yes {
            return Err(AgentError::other(
                "refusing to wipe the recommendation memory without --yes (the agent will start \
                 repeating past picks)",
            ));
        }
        let removed = storage.forget_all_recommendations().await?;
        println!("Forgot {removed} remembered recommendations.");
        return Ok(());
    }

    let mut total = 0;
    for artist in &artists {
        let removed = storage.forget_artist(artist.clone()).await?;
        println!("{artist}: forgot {removed} recommendation(s)");
        total += removed;
    }
    if total == 0 {
        println!("Nothing matched — check the spelling with `spotify-agent recommended`.");
    }
    Ok(())
}

// ===========================================================================
// schedule
// ===========================================================================

pub async fn schedule(config: Arc<Config>, command: ScheduleCommand, color: bool) -> Result<()> {
    let s = Style::new(color);

    match command {
        ScheduleCommand::Install { every, at, preset } => {
            let cadence = crate::schedule::Cadence::parse(&every).ok_or_else(|| {
                AgentError::config(format!("unknown cadence `{every}` (hourly|daily|weekly)"))
            })?;
            let (hour, minute) = parse_hhmm(&at)?;

            // Validate the preset now rather than letting a scheduled run fail
            // silently at 07:30 every morning.
            if let Some(name) = &preset {
                config.resolve_preset(name)?;
            }

            let spec = crate::schedule::ScheduleSpec {
                cadence,
                hour,
                minute,
                preset,
                binary: crate::schedule::current_binary()?,
                extra_args: Vec::new(),
            };

            println!("{} {}", s.dim("command:"), s.dim(&spec.command_line()));
            let status = crate::schedule::install(&spec)?;
            // The files may be written without the scheduler accepting them;
            // the marker must not claim success in that case.
            let marker = if status.installed {
                s.green("✓")
            } else {
                s.yellow("!")
            };
            println!("{marker} {}", status.detail);
            if let Some(path) = status.path {
                println!(
                    "{} {}",
                    s.dim("definition:"),
                    s.dim(&path.display().to_string())
                );
            }
            println!("{} {}", s.dim("mechanism:"), s.dim(status.mechanism));
            Ok(())
        }
        ScheduleCommand::Status => {
            let status = crate::schedule::status()?;
            println!("{}", s.heading("Background schedule"));
            println!(
                "  {} {}",
                if status.installed {
                    s.green("●")
                } else {
                    s.dim("○")
                },
                if status.installed {
                    "installed"
                } else {
                    "not installed"
                }
            );
            println!("  {} {}", s.dim("mechanism:"), status.mechanism);
            if let Some(path) = status.path {
                println!("  {} {}", s.dim("path:"), path.display());
            }
            for line in status.detail.lines() {
                println!("  {line}");
            }
            Ok(())
        }
        ScheduleCommand::Remove => {
            if crate::schedule::remove()? {
                println!("Schedule removed.");
            } else {
                println!("No schedule was installed.");
            }
            Ok(())
        }
    }
}

fn parse_hhmm(value: &str) -> Result<(u32, u32)> {
    let (h, m) = value
        .split_once(':')
        .ok_or_else(|| AgentError::config(format!("`{value}` is not HH:MM")))?;
    let hour: u32 = h
        .trim()
        .parse()
        .map_err(|_| AgentError::config("bad hour"))?;
    let minute: u32 = m
        .trim()
        .parse()
        .map_err(|_| AgentError::config("bad minute"))?;
    if hour > 23 || minute > 59 {
        return Err(AgentError::config(format!(
            "`{value}` is not a valid time of day"
        )));
    }
    Ok((hour, minute))
}

// ===========================================================================
// export
// ===========================================================================

pub async fn export(config: Arc<Config>, args: ExportArgs) -> Result<()> {
    use crate::export::{ExportTrack, Format, render, safe_filename};

    let format = Format::parse(&args.format).ok_or_else(|| {
        AgentError::config(format!("unknown format `{}` (m3u8|csv|json)", args.format))
    })?;
    let storage = crate::storage::Storage::open(&config.database_path()?).await?;

    let (name, tracks) = if let Some(id) = args.snapshot {
        let rows = storage.snapshot_tracks(id).await?;
        if rows.is_empty() {
            return Err(AgentError::other(format!(
                "snapshot {id} is empty or does not exist"
            )));
        }
        let meta = storage.snapshots(1, None).await?;
        let name = meta
            .into_iter()
            .find(|s| s.id == id)
            .map(|s| s.playlist_name)
            .unwrap_or_else(|| format!("snapshot-{id}"));
        let tracks = rows
            .into_iter()
            .map(|t| ExportTrack {
                position: t.position as usize + 1,
                artist: t.artist,
                title: t.title,
                album: t.album,
                duration_ms: t.duration_ms,
                track_id: t.track_id,
                isrc: None,
                popularity: None,
                reason: None,
                mood: None,
            })
            .collect::<Vec<_>>();
        (name, tracks)
    } else if args.last {
        // The most recent run, with Claude's rationale attached.
        let rows = storage.recommendations(500, None, true).await?;
        if rows.is_empty() {
            return Err(AgentError::other("no recommendations recorded yet"));
        }
        let tracks = rows
            .into_iter()
            .enumerate()
            .map(|(index, r)| ExportTrack {
                position: index + 1,
                artist: r.artist,
                title: r.title,
                album: String::new(),
                duration_ms: 0,
                track_id: String::new(),
                isrc: None,
                popularity: None,
                reason: Some(r.reason),
                mood: Some(r.mood),
            })
            .collect::<Vec<_>>();
        ("spotify-agent recommendations".to_string(), tracks)
    } else {
        let playlist_name = args
            .playlist
            .clone()
            .ok_or_else(|| AgentError::config("pass --playlist NAME, --snapshot ID or --last"))?;
        let handle = Engine::build_spotify_only(Arc::clone(&config)).await?;
        let me = handle.spotify.current_user().await?;
        let playlist = handle
            .spotify
            .find_playlist(&me.id, &playlist_name)
            .await?
            .ok_or_else(|| {
                AgentError::other(format!("no playlist of yours is called `{playlist_name}`"))
            })?;
        let tracks = handle
            .spotify
            .playlist_tracks(&playlist.id)
            .await?
            .iter()
            .enumerate()
            .map(|(index, t)| ExportTrack::from_track(index + 1, t))
            .collect::<Vec<_>>();
        (playlist.name, tracks)
    };

    let rendered = render(format, &name, &tracks);

    match args.out.as_deref() {
        Some("-") => {
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(rendered.as_bytes())
                .map_err(|e| AgentError::io("stdout", e))?;
        }
        Some(path) => {
            std::fs::write(path, &rendered).map_err(|e| AgentError::io(path, e))?;
            println!("Wrote {} tracks to {path}", tracks.len());
        }
        None => {
            let path = format!("{}.{}", safe_filename(&name), format.extension());
            std::fs::write(&path, &rendered).map_err(|e| AgentError::io(&path, e))?;
            println!("Wrote {} tracks to {path}", tracks.len());
        }
    }
    Ok(())
}

// ===========================================================================
// snapshots
// ===========================================================================

pub async fn snapshots(config: Arc<Config>, command: SnapshotCommand, color: bool) -> Result<()> {
    let s = Style::new(color);
    let storage = crate::storage::Storage::open(&config.database_path()?).await?;

    match command {
        SnapshotCommand::List { limit, json } => {
            let rows = storage.snapshots(limit, None).await?;
            if json {
                return print_json(json!(
                    rows.iter()
                        .map(|r| json!({
                            "id": r.id,
                            "playlist_id": r.playlist_id,
                            "playlist_name": r.playlist_name,
                            "reason": r.reason,
                            "track_count": r.track_count,
                            "taken_at": r.taken_at.to_rfc3339(),
                        }))
                        .collect::<Vec<_>>()
                ));
            }
            if rows.is_empty() {
                println!("No snapshots yet. One is taken automatically before every overwrite.");
                return Ok(());
            }
            println!(
                "{:<6} {:<19} {:<28} {:>6}  REASON",
                "ID", "TAKEN", "PLAYLIST", "TRACKS"
            );
            for row in rows {
                println!(
                    "{:<6} {:<19} {:<28} {:>6}  {}",
                    row.id,
                    row.taken_at.format("%Y-%m-%d %H:%M:%S"),
                    fit(&row.playlist_name, 28).trim_end(),
                    row.track_count,
                    row.reason
                );
            }
            Ok(())
        }
        SnapshotCommand::Show { id, json } => {
            let tracks = storage.snapshot_tracks(id).await?;
            if tracks.is_empty() {
                return Err(AgentError::other(format!("snapshot {id} has no tracks")));
            }
            if json {
                return print_json(json!(
                    tracks
                        .iter()
                        .map(|t| json!({
                            "position": t.position,
                            "artist": t.artist,
                            "title": t.title,
                            "album": t.album,
                            "track_id": t.track_id,
                        }))
                        .collect::<Vec<_>>()
                ));
            }
            for track in tracks {
                println!(
                    "  {:>3}. {} {}",
                    track.position + 1,
                    fit(&track.artist, 26),
                    fit(&track.title, 40)
                );
            }
            Ok(())
        }
        SnapshotCommand::Restore { id, yes } => {
            let tracks = storage.snapshot_tracks(id).await?;
            if tracks.is_empty() {
                return Err(AgentError::other(format!("snapshot {id} has no tracks")));
            }
            let meta = storage
                .snapshots(1000, None)
                .await?
                .into_iter()
                .find(|s| s.id == id)
                .ok_or_else(|| AgentError::other(format!("snapshot {id} not found")))?;

            if !yes {
                println!(
                    "{} restoring snapshot {id} would replace the current contents of `{}` with {} tracks.",
                    s.yellow("!"),
                    meta.playlist_name,
                    tracks.len()
                );
                return Err(AgentError::other("re-run with --yes to proceed"));
            }

            let handle = Engine::build_spotify_only(Arc::clone(&config)).await?;

            // Snapshot the current state first: restoring is itself a
            // destructive write, and undoing an undo must also be possible.
            let current = handle.spotify.playlist_tracks(&meta.playlist_id).await?;
            if !current.is_empty() {
                let new_id = handle
                    .storage
                    .snapshot_playlist(
                        meta.playlist_id.clone(),
                        meta.playlist_name.clone(),
                        None,
                        "pre-restore",
                        current,
                    )
                    .await?;
                println!("{} current state saved as snapshot {new_id}", s.dim("·"));
            }

            let uris: Vec<String> = tracks
                .iter()
                .map(|t| format!("spotify:track:{}", t.track_id))
                .collect();
            handle
                .spotify
                .replace_playlist_tracks(&meta.playlist_id, &uris)
                .await?;
            handle
                .storage
                .record_members(
                    meta.playlist_id.clone(),
                    tracks.iter().map(|t| t.track_id.clone()).collect(),
                )
                .await?;

            println!(
                "{} restored {} tracks to `{}`",
                s.green("✓"),
                uris.len(),
                meta.playlist_name
            );
            Ok(())
        }
    }
}

// ===========================================================================
// feedback & bans
// ===========================================================================

pub async fn feedback(config: Arc<Config>, limit: usize, as_json: bool, color: bool) -> Result<()> {
    let storage = crate::storage::Storage::open(&config.database_path()?).await?;
    let scores = storage
        .feedback_scores(config.feedback.half_life_days)
        .await?;
    let artists = storage.artists_map().await?;

    let mut names: HashMap<String, String> = HashMap::new();
    for (id, artist) in &artists {
        names.insert(id.clone(), artist.name.clone());
        names.insert(
            crate::util::text::normalize(&artist.name),
            artist.name.clone(),
        );
    }

    let (resonated, rejected) = crate::engine::feedback::artist_verdicts(
        &config.feedback,
        &scores.by_artist,
        &names,
        limit,
    );

    if as_json {
        return print_json(json!({
            "enabled": config.feedback.enabled,
            "half_life_days": config.feedback.half_life_days,
            "boost_threshold": config.feedback.boost_threshold,
            "avoid_threshold": config.feedback.avoid_threshold,
            "tracks_with_signals": scores.by_track.len(),
            "resonated": resonated,
            "rejected": rejected,
        }));
    }

    let s = Style::new(color);
    println!("{}", s.heading("Feedback"));
    if !config.feedback.enabled {
        println!(
            "  {}",
            s.yellow("disabled in config — signals are not being collected")
        );
    }
    println!(
        "  {} tracks carry signals · half-life {} days",
        scores.by_track.len(),
        config.feedback.half_life_days
    );

    if resonated.is_empty() && rejected.is_empty() {
        println!(
            "\n  {}",
            s.dim("nothing conclusive yet — signals accumulate as you listen")
        );
        return Ok(());
    }
    if !resonated.is_empty() {
        println!("\n{}", s.heading("Resonated"));
        for name in &resonated {
            println!("  {} {name}", s.green("+"));
        }
    }
    if !rejected.is_empty() {
        println!("\n{}", s.heading("Did not land"));
        for name in &rejected {
            println!("  {} {name}", s.dim("-"));
        }
    }
    Ok(())
}

pub async fn ban(config: Arc<Config>, artists: Vec<String>, reason: Option<String>) -> Result<()> {
    let storage = crate::storage::Storage::open(&config.database_path()?).await?;
    for artist in &artists {
        storage
            .ban_artist(artist.clone(), None, reason.clone())
            .await?;
        println!("Banned {artist}");
    }
    println!("Banned artists are never recommended again, on any preset.");
    Ok(())
}

pub async fn unban(config: Arc<Config>, artists: Vec<String>) -> Result<()> {
    let storage = crate::storage::Storage::open(&config.database_path()?).await?;
    for artist in &artists {
        if storage.unban_artist(artist.clone()).await? {
            println!("Unbanned {artist}");
        } else {
            println!("{artist} was not banned");
        }
    }
    Ok(())
}

pub async fn bans(config: Arc<Config>, as_json: bool) -> Result<()> {
    let storage = crate::storage::Storage::open(&config.database_path()?).await?;
    let rows = storage.banned_artists().await?;

    if as_json {
        return print_json(json!(
            rows.iter()
                .map(|b| json!({
                    "name": b.name,
                    "reason": b.reason,
                    "created_at": b.created_at.to_rfc3339(),
                }))
                .collect::<Vec<_>>()
        ));
    }
    if rows.is_empty() {
        println!("No artists are banned.");
        return Ok(());
    }
    for row in rows {
        println!(
            "  {}  {}{}",
            row.created_at.format("%Y-%m-%d"),
            row.name,
            row.reason.map(|r| format!("  ({r})")).unwrap_or_default()
        );
    }
    Ok(())
}

/// Best-effort notification for a finished run. Never changes the outcome.
async fn notify_run(config: &Config, outcome: Option<&RunOutcome>, error: Option<&AgentError>) {
    let cfg = &config.notifications;
    if !cfg.desktop && cfg.webhook_url.is_none() {
        return;
    }

    let http = match reqwest::Client::builder()
        .user_agent(concat!("spotify-agent/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::debug!(error = %e, "could not build the notification client");
            return;
        }
    };

    let notification = match (outcome, error) {
        (Some(outcome), _) => {
            let body = format!(
                "{} tracks · {} preset · {:.0}s{}",
                outcome.accepted.len(),
                outcome.preset,
                outcome.duration.as_secs_f32(),
                if outcome.dry_run { " (dry run)" } else { "" }
            );
            crate::notify::Notification::success(outcome.playlist_title.clone(), body)
                .with_url(outcome.playlist.as_ref().map(|p| p.web_url()))
        }
        (None, Some(e)) => {
            crate::notify::Notification::failure("spotify-agent failed", e.to_string())
        }
        (None, None) => return,
    };

    crate::notify::send(cfg, &http, &notification).await;
}

// ===========================================================================
// history
// ===========================================================================

pub async fn history(config: Arc<Config>, limit: usize, as_json: bool) -> Result<()> {
    let storage = crate::storage::Storage::open(&config.database_path()?).await?;
    let runs = storage.recent_runs(limit).await?;

    if as_json {
        return print_json(json!(
            runs.iter()
                .map(|r| json!({
                    "id": r.id,
                    "preset": r.preset,
                    "started_at": r.started_at.to_rfc3339(),
                    "status": r.status,
                    "written": r.written,
                    "playlist_id": r.playlist_id,
                }))
                .collect::<Vec<_>>()
        ));
    }

    if runs.is_empty() {
        println!("No runs recorded yet.");
        return Ok(());
    }

    println!(
        "{:<6} {:<19} {:<14} {:<9} TRACKS",
        "ID", "STARTED", "PRESET", "STATUS"
    );
    for run in runs {
        println!(
            "{:<6} {:<19} {:<14} {:<9} {}",
            run.id,
            run.started_at.format("%Y-%m-%d %H:%M:%S"),
            fit(&run.preset, 14).trim_end(),
            run.status,
            run.written
        );
    }
    Ok(())
}

// ===========================================================================
// cache
// ===========================================================================

pub async fn cache(config: Arc<Config>, command: CacheCommand) -> Result<()> {
    let db_path = config.database_path()?;

    match command {
        CacheCommand::Path => {
            println!("{}", db_path.display());
            Ok(())
        }
        CacheCommand::Stats { json } => {
            let storage = crate::storage::Storage::open(&db_path).await?;
            let stats = storage.stats().await?;
            if json {
                return print_json(json!({
                    "path": db_path.display().to_string(),
                    "tracks": stats.tracks,
                    "artists": stats.artists,
                    "saved": stats.saved,
                    "plays": stats.plays,
                    "distinct_played": stats.distinct_played,
                    "recommendations": stats.recommendations,
                    "runs": stats.runs,
                    "db_bytes": stats.db_bytes,
                    "last_sync": stats.last_sync.map(|t| t.to_rfc3339()),
                    "oldest_play": stats.oldest_play.map(|t| t.to_rfc3339()),
                    "newest_play": stats.newest_play.map(|t| t.to_rfc3339()),
                }));
            }
            println!("path             {}", db_path.display());
            println!("tracks           {}", stats.tracks);
            println!("artists          {}", stats.artists);
            println!("liked            {}", stats.saved);
            println!(
                "plays            {} ({} distinct tracks)",
                stats.plays, stats.distinct_played
            );
            println!("recommendations  {}", stats.recommendations);
            println!("runs             {}", stats.runs);
            println!(
                "size             {:.2} MB",
                stats.db_bytes as f64 / 1_048_576.0
            );
            if let (Some(oldest), Some(newest)) = (stats.oldest_play, stats.newest_play) {
                println!(
                    "history span     {} → {}",
                    oldest.format("%Y-%m-%d"),
                    newest.format("%Y-%m-%d")
                );
            }
            Ok(())
        }
        CacheCommand::Prune => {
            let storage = crate::storage::Storage::open(&db_path).await?;
            let (plays, recs) = storage
                .prune(
                    config.storage.retain_plays_days,
                    config.storage.retain_recommendations_days,
                )
                .await?;
            println!("Pruned {plays} play events and {recs} recommendation records.");
            Ok(())
        }
        CacheCommand::Clear { yes } => {
            if !yes {
                return Err(AgentError::other(
                    "refusing to clear the cache without --yes (this deletes all local history, \
                     including the play frequency data Spotify cannot give back)",
                ));
            }
            let storage = crate::storage::Storage::open(&db_path).await?;
            storage.clear().await?;
            println!("Cache cleared. Stored Spotify tokens were not touched.");
            Ok(())
        }
    }
}

// ===========================================================================
// config
// ===========================================================================

pub fn config_cmd(config: Arc<Config>, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Path => {
            let path = config
                .source_path
                .clone()
                .map(Ok)
                .unwrap_or_else(paths::config_file)?;
            println!("{}", path.display());
            Ok(())
        }
        ConfigCommand::Show => {
            // `Secret` serialises as "<redacted>", so this is safe to paste
            // into a bug report.
            let rendered = toml::to_string_pretty(&*config)
                .map_err(|e| AgentError::config(format!("cannot render config: {e}")))?;
            println!("{rendered}");
            Ok(())
        }
        ConfigCommand::Language { value } => {
            let data_dir = config.data_dir()?;
            let mut prefs = crate::prefs::Preferences::load(&data_dir);

            match value {
                None => {
                    let active = crate::prefs::resolve_language(config.general.language, &prefs);
                    println!("{} ({})", active.endonym(), active.code());
                    if config.general.language.is_some() {
                        println!("set by general.language in the config file");
                    }
                    println!(
                        "available: {}",
                        crate::i18n::Lang::ALL
                            .iter()
                            .map(|l| format!("{} ({})", l.code(), l.endonym()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                Some(raw) => {
                    let lang = crate::i18n::Lang::parse(&raw).ok_or_else(|| {
                        AgentError::config(format!("unknown language `{raw}` (en|ru|pl|lt)"))
                    })?;
                    prefs.language = Some(lang);
                    prefs.language_chosen = true;
                    prefs.save(&data_dir)?;
                    println!(
                        "Interface language set to {} ({}).",
                        lang.endonym(),
                        lang.code()
                    );
                    if config.general.language.is_some() {
                        // Saying nothing here would leave the user staring at
                        // an unchanged UI with no explanation.
                        println!(
                            "note: general.language in the config file still overrides this — remove it to use the saved preference."
                        );
                    }
                }
            }
            Ok(())
        }
        ConfigCommand::Init { force } => {
            let path = paths::config_file()?;
            if path.exists() && !force {
                return Err(AgentError::config(format!(
                    "{} already exists (use --force to overwrite)",
                    path.display()
                )));
            }
            if let Some(parent) = path.parent() {
                paths::ensure_dir(parent)?;
            }
            std::fs::write(&path, EXAMPLE_CONFIG)
                .map_err(|e| AgentError::io(path.display().to_string(), e))?;
            crate::util::fs::harden_file(&path)?;
            println!("Wrote {}", path.display());
            println!("Next: fill in spotify.client_id, then run `spotify-agent login`.");
            Ok(())
        }
    }
}

// ===========================================================================
// update
// ===========================================================================

/// `spotify-agent update` — check, report, and replace this binary.
///
/// The flow is deliberately conservative. Nothing is downloaded until the user
/// has seen what is on offer and agreed, `--check` never downloads at all, and
/// a non-interactive invocation without `--yes` reports and stops rather than
/// assuming consent it cannot ask for.
pub async fn update(
    config: Arc<Config>,
    args: crate::cli::UpdateArgs,
    use_color: bool,
) -> Result<()> {
    use crate::update::{Check, Updater};

    let style = Style::new(use_color);
    let updater = Updater::new(&config)?;
    let current = crate::update::version::Version::current();

    // An explicit tag is a different question ("give me this one") and is
    // answered without the newer/older comparison, so a downgrade works.
    let (release, is_upgrade) = match &args.to {
        Some(tag) => {
            let release = updater.release_by_tag(tag).await?;
            let newer = release.version > current;
            (Some(release), newer)
        }
        None => match updater.check(true).await? {
            Check::Available { release, .. } => (Some(*release), true),
            Check::UpToDate { .. } | Check::Disabled => (None, false),
        },
    };

    if args.json {
        return print_json(json!({
            "current": current.to_string(),
            "latest": release.as_ref().map(|r| r.version.to_string()),
            "tag": release.as_ref().map(|r| r.tag.clone()),
            "url": release.as_ref().map(|r| r.url.clone()),
            "update_available": release.is_some() && is_upgrade,
            "target": crate::update::install::target_triple(),
        }));
    }

    let Some(release) = release else {
        println!(
            "{} is the latest release.",
            style.bold(&format!("spotify-agent {current}"))
        );
        return Ok(());
    };

    let direction = if is_upgrade { "→" } else { "↓ (downgrade)" };
    println!(
        "{}  {current} {direction} {}",
        style.bold("spotify-agent"),
        style.green(&release.version.to_string())
    );
    if !release.url.is_empty() {
        println!("{}", style.dim(&release.url));
    }
    let summary = release.summary(8);
    if !summary.is_empty() {
        println!();
        for line in summary.lines() {
            println!("  {line}");
        }
    }
    println!();

    if args.check {
        println!("{}", style.dim("run `spotify-agent update` to install it"));
        return Ok(());
    }

    if !args.yes && !confirm("Install it now?")? {
        println!("{}", style.dim("left alone"));
        return Ok(());
    }

    let exe = install_release(&updater, &release, use_color).await?;
    println!(
        "{} {} is installed at {}",
        style.green("✓"),
        style.bold(&format!("spotify-agent {}", release.version)),
        exe.display()
    );
    println!(
        "{}",
        style.dim("any already-running instance keeps the old code until it restarts")
    );
    Ok(())
}

/// Download with a progress line, then swap. Shared by the CLI and the TUI's
/// "update now" so both report the same thing and verify the same way.
async fn install_release(
    updater: &crate::update::Updater,
    release: &crate::update::Release,
    use_color: bool,
) -> Result<std::path::PathBuf> {
    use std::io::IsTerminal;

    let style = Style::new(use_color);
    println!("{}", style.dim(&format!("downloading {}…", release.tag)));

    // Progress is only drawn on a terminal: a carriage-return animation in a
    // log file or a CI transcript is noise, and `\r` in a pipe is worse.
    let animate = std::io::stderr().is_terminal();
    let exe = updater
        .install(release, move |done, total| {
            if !animate {
                return;
            }
            let mut err = std::io::stderr().lock();
            let _ = match total {
                Some(total) if total > 0 => write!(
                    err,
                    "\r  {:>3}%  {:.1} MB",
                    done * 100 / total,
                    done as f64 / 1_048_576.0
                ),
                _ => write!(err, "\r  {:.1} MB", done as f64 / 1_048_576.0),
            };
            let _ = err.flush();
        })
        .await;

    if animate {
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r{:40}\r", "");
        let _ = err.flush();
    }
    exe
}

/// Ask a yes/no question on the terminal.
///
/// A non-terminal stdin answers "no": a piped or absent stdin cannot consent,
/// and treating end-of-file as agreement is how unattended scripts get
/// surprises they never asked for.
fn confirm(question: &str) -> Result<bool> {
    use std::io::{BufRead, IsTerminal};

    if !std::io::stdin().is_terminal() {
        println!("not a terminal — re-run with --yes to install");
        return Ok(false);
    }

    print!("{question} [y/N] ");
    std::io::stdout()
        .flush()
        .map_err(|e| AgentError::io("stdout", e))?;

    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| AgentError::io("stdin", e))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_pads_and_truncates_on_char_boundaries() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdefgh", 4), "abc…");
        assert_eq!(fit("привет мир", 6).chars().count(), 6);
    }

    #[test]
    fn example_config_parses() {
        let parsed: std::result::Result<Config, _> = toml::from_str(EXAMPLE_CONFIG);
        assert!(
            parsed.is_ok(),
            "config.example.toml does not parse: {parsed:?}"
        );
    }
}
