//! Interactive terminal UI.
//!
//! Layout follows the dense-utility convention (`lazygit`, `btop`): a fixed
//! header, a narrow left rail for selection, a wide main pane that switches
//! views, and a persistent footer of keybindings.
//!
//! Threading model:
//!
//! ```text
//!   input thread ──┐
//!                  ├─► select! ─► App::update ─► terminal.draw
//!   engine task  ──┤
//!   tick timer   ──┘
//! ```
//!
//! Terminal input is read on a dedicated OS thread because `crossterm`'s read
//! is blocking; the pipeline runs as a normal tokio task and reports through
//! the same [`EngineEvent`] channel the CLI uses. Nothing else may write to
//! stdout while the alternate screen is active — which is why `telemetry`
//! swaps its writer for a ring buffer in this mode.

mod app;
mod ui;

use crate::config::Config;
use crate::engine::Engine;
use crate::error::{AgentError, Result};
use crate::telemetry::LogBuffer;
use app::{Action, App, Verdict};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event as CtEvent, KeyEventKind};
use ratatui::crossterm::{execute, terminal};
use std::io::Stdout;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// UI refresh cadence. 20 fps is smooth for a progress gauge and costs
/// nothing; the engine pushes events independently, so this only drives
/// animations and the clock.
const TICK: Duration = Duration::from_millis(50);

pub async fn run(config: Arc<Config>, log_buffer: Option<LogBuffer>) -> Result<()> {
    let mut terminal = setup()?;

    // The result is captured rather than propagated so the terminal is always
    // restored, even on error. `?` before `restore()` would leave the user in
    // the alternate screen with no echo.
    let result = event_loop(&mut terminal, config, log_buffer).await;

    restore(&mut terminal)?;
    result
}

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn setup() -> Result<Tui> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return Err(AgentError::config(
            "the TUI needs a terminal; use `spotify-agent generate --headless` when piping output",
        ));
    }

    terminal::enable_raw_mode().map_err(|e| AgentError::io("terminal", e))?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        terminal::EnterAlternateScreen,
        event::EnableMouseCapture
    )
    .map_err(|e| AgentError::io("terminal", e))?;

    install_panic_hook();

    Terminal::new(CrosstermBackend::new(stdout)).map_err(|e| AgentError::io("terminal", e))
}

fn restore(terminal: &mut Tui) -> Result<()> {
    terminal::disable_raw_mode().map_err(|e| AgentError::io("terminal", e))?;
    execute!(
        terminal.backend_mut(),
        terminal::LeaveAlternateScreen,
        event::DisableMouseCapture
    )
    .map_err(|e| AgentError::io("terminal", e))?;
    terminal
        .show_cursor()
        .map_err(|e| AgentError::io("terminal", e))?;
    Ok(())
}

/// A panic inside the draw path must not leave a raw-mode terminal behind.
/// The default hook runs afterwards so the backtrace is still printed.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            terminal::LeaveAlternateScreen,
            event::DisableMouseCapture
        );
        default(info);
    }));
}

async fn event_loop(
    terminal: &mut Tui,
    config: Arc<Config>,
    log_buffer: Option<LogBuffer>,
) -> Result<()> {
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<CtEvent>();
    spawn_input_thread(input_tx);

    let (engine_tx, mut engine_rx) = mpsc::unbounded_channel();
    let mut app = App::new(Arc::clone(&config), log_buffer, engine_tx);

    // Interface language: config wins, then the saved preference, then the
    // system locale. A first run with none of those opens the picker.
    let data_dir = config.data_dir()?;
    let prefs = crate::prefs::Preferences::load(&data_dir);
    let lang = crate::prefs::resolve_language(config.general.language, &prefs);
    let prompt = crate::prefs::should_prompt_for_language(config.general.language, &prefs);
    app.set_language(lang, prefs, prompt);

    // Building the engine can fail on a missing API key. That is a state the
    // UI should display, not a reason to refuse to start — the user may well
    // have opened the TUI precisely to find out what is misconfigured.
    let engine = match Engine::build(Arc::clone(&config)).await {
        Ok(engine) => {
            let engine = Arc::new(engine);
            app.set_authorized(engine.auth.is_authorized().await);
            app.load_initial(&engine).await;
            Some(engine)
        }
        Err(e) => {
            app.set_fatal(e.to_string());
            None
        }
    };

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        terminal
            .draw(|frame| ui::render(frame, &mut app))
            .map_err(|e| AgentError::io("terminal", e))?;

        let action = tokio::select! {
            Some(event) = input_rx.recv() => match event {
                CtEvent::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                CtEvent::Mouse(mouse) => app.on_mouse(mouse),
                CtEvent::Resize(_, _) => Action::None,
                _ => Action::None,
            },
            Some(event) = engine_rx.recv() => {
                app.on_engine_event(event);
                Action::None
            }
            _ = ticker.tick() => {
                app.on_tick();
                Action::None
            }
            else => Action::Quit,
        };

        match action {
            Action::None => {}
            Action::Quit => break,
            Action::Generate => {
                if let Some(engine) = &engine {
                    app.start_generate(Arc::clone(engine));
                }
            }
            Action::Sync => {
                if let Some(engine) = &engine {
                    app.start_sync(Arc::clone(engine));
                }
            }
            Action::RefreshProfile => {
                if let Some(engine) = &engine {
                    app.refresh_profile(Arc::clone(engine));
                }
            }
            Action::Moderate(verdict) => {
                if let Some(engine) = &engine {
                    moderate(&mut app, Arc::clone(engine), verdict).await;
                }
            }
            Action::CommitLanguage => {
                let data_dir = config.data_dir()?;
                app.commit_language(&data_dir);
            }
        }
    }

    Ok(())
}

/// Apply a moderation verdict: write it to Spotify, record the signal, and
/// reflect it in the UI.
///
/// Done inline rather than in a spawned task so the UI cannot show a verdict
/// that has not actually been applied — these are single, fast requests, and a
/// silent divergence between the screen and the account would be worse than a
/// brief pause.
async fn moderate(app: &mut App, engine: Arc<Engine>, verdict: Verdict) {
    let Some(item) = app.selected_track().cloned() else {
        return;
    };
    let track_id = item.track.id.clone();
    let display = item.track.display();
    let playlist_id = app
        .outcome
        .as_ref()
        .and_then(|o| o.playlist.as_ref())
        .map(|p| p.id.clone());

    if let Err(e) = apply_verdict(&engine, verdict, &item, playlist_id.as_deref()).await {
        app.on_engine_event(crate::engine::EngineEvent::Failed(format!(
            "could not apply that: {e}"
        )));
        return;
    }

    app.apply_verdict(track_id, verdict, display);
}

async fn apply_verdict(
    engine: &Engine,
    verdict: Verdict,
    item: &crate::domain::ResolvedSuggestion,
    playlist_id: Option<&str>,
) -> Result<()> {
    use crate::storage::{FeedbackEntry, Signal};

    let cfg = &engine.config.feedback;
    let track_id = item.track.id.clone();
    let uri = item.track.uri();

    let (signal, weight) = match verdict {
        Verdict::Keep => {
            engine
                .spotify
                .save_tracks(std::slice::from_ref(&track_id))
                .await?;
            (Signal::Up, cfg.weight_up.max(cfg.weight_liked))
        }
        Verdict::Drop => {
            if let Some(playlist) = playlist_id {
                engine
                    .spotify
                    .remove_playlist_tracks(playlist, std::slice::from_ref(&uri))
                    .await?;
            }
            (Signal::Down, cfg.weight_down)
        }
        Verdict::BanArtist => {
            let artist = item.track.primary_artist().to_string();
            let artist_id = item
                .track
                .artists
                .first()
                .map(|a| a.id.clone())
                .filter(|id| !id.is_empty());
            engine
                .storage
                .ban_artist(artist, artist_id, Some("banned from the TUI".into()))
                .await?;
            if let Some(playlist) = playlist_id {
                engine
                    .spotify
                    .remove_playlist_tracks(playlist, std::slice::from_ref(&uri))
                    .await?;
            }
            (Signal::Banned, cfg.weight_down * 2.0)
        }
        Verdict::Up => (Signal::Up, cfg.weight_up),
        Verdict::Down => (Signal::Down, cfg.weight_down),
    };

    engine
        .storage
        .record_feedback(vec![FeedbackEntry {
            track_id: track_id.clone(),
            signal,
            weight,
            source: "tui",
            note: Some(verdict.label().to_string()),
            // Explicit verdicts are re-recordable: changing your mind should
            // register, so the key carries the signal rather than being a
            // once-ever fact like an automatic `liked`.
            dedupe_key: format!(
                "tui|{track_id}|{}|{}",
                signal.as_str(),
                chrono::Utc::now().timestamp()
            ),
        }])
        .await?;

    Ok(())
}

/// Blocking terminal reads live on their own thread. `event::poll` with a
/// timeout would work too, but it burns CPU proportional to the poll rate for
/// no benefit — a dedicated thread parked in `read()` costs nothing.
fn spawn_input_thread(tx: mpsc::UnboundedSender<CtEvent>) {
    std::thread::Builder::new()
        .name("tui-input".into())
        .spawn(move || {
            // Ends when the terminal closes (read error) or the UI drops
            // the receiver.
            while let Ok(event) = event::read() {
                if tx.send(event).is_err() {
                    break;
                }
            }
        })
        // A failure to spawn means the UI simply will not respond to input;
        // the quit path via Ctrl-C in `main` still works.
        .ok();
}
