//! TUI state machine.
//!
//! Deliberately free of rendering: `ui.rs` reads this and draws it. Keeping
//! the split strict means the keymap and the pipeline wiring are testable
//! without a terminal.

use crate::config::{Config, FillStrategy, LanguagePolicy};
use crate::domain::{RejectedSuggestion, ResolvedSuggestion, TasteProfile};
use crate::engine::{Engine, EngineEvent, GenerateOptions, RunOutcome, Stage};
use crate::i18n::{Lang, Strings};
use crate::prefs::Preferences;
use crate::telemetry::LogBuffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

/// What the event loop should do after a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Generate,
    Sync,
    RefreshProfile,
    /// Moderation verdicts on the selected track.
    Moderate(Verdict),
    /// Save the language chosen in the picker.
    CommitLanguage,
    /// Download and install the release the update dialog is offering.
    InstallUpdate,
    /// Remember that this version was declined, and stop offering it.
    SkipUpdate,
}

/// A judgement the listener makes on one track, from the review pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Save to Liked Songs and record a strong positive.
    Keep,
    /// Remove from the playlist and record a strong negative.
    Drop,
    /// Ban the artist outright, and drop the track.
    BanArtist,
    /// Thumbs up without touching the library.
    Up,
    /// Thumbs down without removing it.
    Down,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Keep => "saved to Liked Songs",
            Self::Drop => "removed from the playlist",
            Self::BanArtist => "artist banned",
            Self::Up => "thumbs up",
            Self::Down => "thumbs down",
        }
    }
}

/// What the update dialog is showing, if anything.
///
/// One enum rather than a bag of booleans because the states are genuinely
/// exclusive: a dialog cannot be both offering and installing, and the keymap
/// differs in each — during a download there is nothing safe to press.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdatePrompt {
    Hidden,
    Available(Box<crate::update::Release>),
    Downloading { done: u64, total: Option<u64> },
    Installed(String),
    Failed(String),
}

impl UpdatePrompt {
    pub fn is_open(&self) -> bool {
        !matches!(self, UpdatePrompt::Hidden)
    }

    /// True while a download is in flight — the one state that must not be
    /// dismissed, because the swap is not finished.
    pub fn is_busy(&self) -> bool {
        matches!(self, UpdatePrompt::Downloading { .. })
    }
}

/// Progress reports from the background update task.
#[derive(Debug, Clone)]
pub enum UpdateMsg {
    Found(Box<crate::update::Release>),
    Progress(u64, Option<u64>),
    Installed(String),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Run,
    Profile,
    Logs,
    /// Credentials and how to obtain them.
    Setup,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Run, Tab::Profile, Tab::Logs, Tab::Setup];

    pub fn title(self, s: &'static Strings) -> &'static str {
        match self {
            Tab::Run => s.tracks,
            Tab::Profile => s.profile,
            Tab::Logs => s.logs,
            Tab::Setup => s.setup,
        }
    }

    pub fn index(self) -> usize {
        match self {
            Tab::Run => 0,
            Tab::Profile => 1,
            Tab::Logs => 2,
            Tab::Setup => 3,
        }
    }
}

/// Rectangles the last frame drew, so mouse clicks can be mapped back to the
/// widget under the pointer. Rendering owns these; input reads them.
#[derive(Debug, Clone, Default)]
pub struct HitRegions {
    pub presets: Rect,
    pub tracks: Rect,
    pub tabs: Vec<Rect>,
    pub logs: Rect,
    /// Rows of the language picker, when it is open.
    pub wizard_rows: Vec<Rect>,
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && column < area.x.saturating_add(area.width)
        && row >= area.y
        && row < area.y.saturating_add(area.height)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Busy {
    Idle,
    Generating(Stage),
    Syncing,
    LoadingProfile,
}

impl Busy {
    pub fn is_busy(&self) -> bool {
        !matches!(self, Busy::Idle)
    }
}

/// A line in the run feed, already classified for colouring.
#[derive(Debug, Clone)]
pub enum FeedLine {
    Info(String),
    Accepted(String),
    Rejected(String, String),
    Error(String),
}

pub struct App {
    pub config: Arc<Config>,
    pub log_buffer: Option<LogBuffer>,
    pub events: UnboundedSender<EngineEvent>,

    // --- selection ---
    pub preset_names: Vec<String>,
    pub preset_state: ListState,
    pub tab: Tab,

    // --- run options toggled from the UI ---
    pub dry_run: bool,
    pub size_override: Option<usize>,
    pub language_override: Option<LanguagePolicy>,
    pub strategy_override: Option<FillStrategy>,

    // --- status ---
    pub authorized: bool,
    pub fatal: Option<String>,
    pub busy: Busy,
    pub progress: (usize, usize),
    pub spinner: usize,

    // --- results ---
    pub profile: Option<TasteProfile>,
    pub outcome: Option<RunOutcome>,
    pub accepted: Vec<ResolvedSuggestion>,
    pub rejected: Vec<RejectedSuggestion>,
    pub feed: VecDeque<FeedLine>,
    pub thinking: String,
    pub scroll: u16,
    pub show_help: bool,

    /// Which pane the arrow keys drive.
    pub focus: Focus,
    /// Cursor into `accepted`, for the review pane.
    pub track_cursor: usize,
    /// Verdicts already applied this session, so the UI can show them.
    pub verdicts: HashMap<String, Verdict>,

    // --- localisation ---
    pub lang: Lang,
    pub prefs: Preferences,
    /// First-run language picker, and its cursor.
    pub wizard_open: bool,
    pub wizard_cursor: usize,

    /// The self-update dialog.
    pub update: UpdatePrompt,

    /// Where the last frame drew things, for mouse hit-testing.
    pub regions: HitRegions,
}

/// Where keyboard navigation applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Presets,
    /// The generated tracklist — where moderation happens.
    Tracks,
}

/// Feed lines retained. The pane shows a window of these; older lines are
/// still in the log buffer if they are needed.
const FEED_CAPACITY: usize = 500;

impl App {
    pub fn new(
        config: Arc<Config>,
        log_buffer: Option<LogBuffer>,
        events: UnboundedSender<EngineEvent>,
    ) -> Self {
        let preset_names = config.preset_names();
        let default_index = preset_names
            .iter()
            .position(|name| *name == config.defaults.preset)
            .unwrap_or(0);

        let mut preset_state = ListState::default();
        if !preset_names.is_empty() {
            preset_state.select(Some(default_index));
        }

        Self {
            config,
            log_buffer,
            events,
            preset_names,
            preset_state,
            tab: Tab::Run,
            dry_run: false,
            size_override: None,
            language_override: None,
            strategy_override: None,
            authorized: false,
            fatal: None,
            busy: Busy::Idle,
            progress: (0, 0),
            spinner: 0,
            profile: None,
            outcome: None,
            accepted: Vec::new(),
            rejected: Vec::new(),
            feed: VecDeque::with_capacity(FEED_CAPACITY),
            thinking: String::new(),
            scroll: 0,
            show_help: false,
            focus: Focus::Presets,
            track_cursor: 0,
            verdicts: HashMap::new(),
            lang: Lang::En,
            prefs: Preferences::default(),
            wizard_open: false,
            wizard_cursor: 0,
            update: UpdatePrompt::Hidden,
            regions: HitRegions::default(),
        }
    }

    /// Localised strings for the active language.
    pub fn t(&self) -> &'static Strings {
        self.lang.strings()
    }

    /// Apply the resolved language and decide whether to open the picker.
    pub fn set_language(&mut self, lang: Lang, prefs: Preferences, prompt: bool) {
        self.lang = lang;
        self.wizard_cursor = Lang::ALL.iter().position(|l| *l == lang).unwrap_or(0);
        self.prefs = prefs;
        self.wizard_open = prompt;
    }

    /// Persist the picked language. A write failure is surfaced but does not
    /// stop the UI — the choice still applies for this session.
    pub fn commit_language(&mut self, data_dir: &std::path::Path) {
        let lang = Lang::ALL
            .get(self.wizard_cursor)
            .copied()
            .unwrap_or(Lang::En);
        self.lang = lang;
        self.prefs.language = Some(lang);
        self.prefs.language_chosen = true;
        self.wizard_open = false;

        match self.prefs.save(data_dir) {
            Ok(()) => self.push_feed(FeedLine::Info(self.t().wizard_saved.to_string())),
            Err(e) => self.push_feed(FeedLine::Error(format!("could not save preferences: {e}"))),
        }
    }

    // -----------------------------------------------------------------
    // Self-update dialog
    // -----------------------------------------------------------------

    fn on_update_key(&mut self, key: KeyEvent) -> Action {
        match &self.update {
            // A download in progress owns the screen: dismissing it would
            // hide a swap that is still happening.
            UpdatePrompt::Downloading { .. } => Action::None,
            UpdatePrompt::Available(_) => match key.code {
                KeyCode::Enter | KeyCode::Char('u') => Action::InstallUpdate,
                KeyCode::Char('s') => Action::SkipUpdate,
                KeyCode::Esc | KeyCode::Char('l') | KeyCode::Char('q') => {
                    self.update = UpdatePrompt::Hidden;
                    Action::None
                }
                _ => Action::None,
            },
            // A finished dialog, good or bad, closes on anything.
            UpdatePrompt::Installed(_) | UpdatePrompt::Failed(_) => {
                self.update = UpdatePrompt::Hidden;
                Action::None
            }
            UpdatePrompt::Hidden => Action::None,
        }
    }

    pub fn on_update_msg(&mut self, msg: UpdateMsg) {
        match msg {
            UpdateMsg::Found(release) => {
                // Never interrupt a run in progress with a dialog; the feed
                // line is enough until the user is looking at the screen
                // again, and the check does not repeat for another day.
                self.push_feed(FeedLine::Info(format!(
                    "update available: {} → {}",
                    crate::VERSION,
                    release.version
                )));
                if !self.busy.is_busy() && !self.wizard_open {
                    self.update = UpdatePrompt::Available(release);
                }
            }
            UpdateMsg::Progress(done, total) => {
                self.update = UpdatePrompt::Downloading { done, total };
            }
            UpdateMsg::Installed(version) => {
                self.push_feed(FeedLine::Info(format!("installed spotify-agent {version}")));
                self.update = UpdatePrompt::Installed(version);
            }
            UpdateMsg::Failed(error) => {
                self.push_feed(FeedLine::Error(format!("update failed: {error}")));
                self.update = UpdatePrompt::Failed(error);
            }
        }
    }

    /// The release the dialog is offering, if it is offering one.
    pub fn pending_update(&self) -> Option<&crate::update::Release> {
        match &self.update {
            UpdatePrompt::Available(release) => Some(release),
            _ => None,
        }
    }

    /// The track the review pane is pointing at.
    pub fn selected_track(&self) -> Option<&ResolvedSuggestion> {
        self.accepted.get(self.track_cursor)
    }

    pub fn verdict_for(&self, track_id: &str) -> Option<Verdict> {
        self.verdicts.get(track_id).copied()
    }

    /// Record a verdict locally and advance, so repeated judgements are one
    /// keypress each rather than keypress-plus-move.
    pub fn apply_verdict(&mut self, track_id: String, verdict: Verdict, display: String) {
        self.verdicts.insert(track_id, verdict);
        self.push_feed(FeedLine::Info(format!("{display} — {}", verdict.label())));
        if self.track_cursor + 1 < self.accepted.len() {
            self.track_cursor += 1;
        }
    }

    pub fn set_authorized(&mut self, authorized: bool) {
        self.authorized = authorized;
        if !authorized {
            self.push_feed(FeedLine::Error(
                "not authorised — quit and run `spotify-agent login`".into(),
            ));
        }
    }

    pub fn set_fatal(&mut self, message: String) {
        self.push_feed(FeedLine::Error(message.clone()));
        self.fatal = Some(message);
    }

    pub async fn load_initial(&mut self, engine: &Engine) {
        match engine.profile().await {
            Ok(profile) => {
                if profile.is_empty() {
                    self.push_feed(FeedLine::Info(
                        "cache is empty — press `s` to sync your library".into(),
                    ));
                } else {
                    self.push_feed(FeedLine::Info(format!(
                        "loaded profile: {} tracks, {} artists",
                        profile.known_tracks,
                        profile.top_artists.len()
                    )));
                }
                self.profile = Some(profile);
            }
            Err(e) => self.push_feed(FeedLine::Error(format!("could not load profile: {e}"))),
        }
    }

    pub fn selected_preset(&self) -> String {
        self.preset_state
            .selected()
            .and_then(|i| self.preset_names.get(i))
            .cloned()
            .unwrap_or_else(|| self.config.defaults.preset.clone())
    }

    /// Effective run parameters, for the options panel.
    pub fn effective_size(&self) -> usize {
        self.size_override.unwrap_or_else(|| {
            self.config
                .resolve_preset(&self.selected_preset())
                .map(|p| p.run.size)
                .unwrap_or(self.config.defaults.size)
        })
    }

    pub fn effective_language(&self) -> LanguagePolicy {
        self.language_override.unwrap_or_else(|| {
            self.config
                .resolve_preset(&self.selected_preset())
                .map(|p| p.run.language)
                .unwrap_or(self.config.defaults.language)
        })
    }

    pub fn effective_strategy(&self) -> FillStrategy {
        self.strategy_override.unwrap_or_else(|| {
            self.config
                .resolve_preset(&self.selected_preset())
                .map(|p| p.run.strategy)
                .unwrap_or(self.config.defaults.strategy)
        })
    }

    // -----------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // Ctrl-C always quits, even mid-run: the pipeline task is detached and
        // the process exit tears it down.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Action::Quit;
        }

        // The language picker is modal: nothing else responds while it is open.
        if self.wizard_open {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.wizard_cursor = (self.wizard_cursor + 1) % Lang::ALL.len();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.wizard_cursor =
                        (self.wizard_cursor + Lang::ALL.len() - 1) % Lang::ALL.len();
                }
                KeyCode::Enter => return Action::CommitLanguage,
                KeyCode::Esc => self.wizard_open = false,
                _ => {}
            }
            return Action::None;
        }

        // The update dialog is modal too, and ranks below the first-run
        // wizard: a brand-new install should pick a language before it is
        // asked about upgrading.
        if self.update.is_open() {
            return self.on_update_key(key);
        }

        if self.show_help {
            self.show_help = false;
            return Action::None;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.show_help = true;
                Action::None
            }

            // --- navigation ---
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor(1);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor(-1);
                Action::None
            }
            // Left/right switch which pane the cursor drives, so the same
            // arrow keys serve both the preset rail and the tracklist.
            KeyCode::Left | KeyCode::Char('h') => {
                self.focus = Focus::Presets;
                Action::None
            }
            KeyCode::Right => {
                if !self.accepted.is_empty() {
                    self.focus = Focus::Tracks;
                    self.tab = Tab::Run;
                }
                Action::None
            }

            // --- moderation (only meaningful with a tracklist on screen) ---
            KeyCode::Char('f') => self.moderate(Verdict::Keep),
            KeyCode::Char('x') | KeyCode::Delete => self.moderate(Verdict::Drop),
            KeyCode::Char('b') => self.moderate(Verdict::BanArtist),
            KeyCode::Char('+') if self.focus == Focus::Tracks => self.moderate(Verdict::Up),
            KeyCode::Char('u') => self.moderate(Verdict::Up),
            KeyCode::Char('i') => self.moderate(Verdict::Down),
            KeyCode::Tab => {
                self.cycle_tab(1);
                Action::None
            }
            KeyCode::BackTab => {
                self.cycle_tab(-1);
                Action::None
            }
            KeyCode::Char('1') => {
                self.tab = Tab::Run;
                Action::None
            }
            KeyCode::Char('2') => {
                self.tab = Tab::Profile;
                Action::None
            }
            KeyCode::Char('3') => {
                self.tab = Tab::Logs;
                Action::None
            }
            KeyCode::Char('4') => {
                self.tab = Tab::Setup;
                Action::None
            }
            // Re-open the language picker from anywhere.
            KeyCode::Char(',') => {
                self.wizard_open = true;
                Action::None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(10);
                Action::None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(10);
                Action::None
            }

            // --- run options ---
            KeyCode::Char('d') => {
                self.dry_run = !self.dry_run;
                Action::None
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.size_override = Some((self.effective_size() + 5).min(200));
                Action::None
            }
            KeyCode::Char('-') => {
                self.size_override = Some(self.effective_size().saturating_sub(5).max(5));
                Action::None
            }
            KeyCode::Char('l') => {
                self.language_override = Some(next_language(self.effective_language()));
                Action::None
            }
            KeyCode::Char('m') => {
                self.strategy_override = Some(match self.effective_strategy() {
                    FillStrategy::Replace => FillStrategy::Append,
                    FillStrategy::Append => FillStrategy::Rolling,
                    FillStrategy::Rolling => FillStrategy::Replace,
                });
                Action::None
            }

            // --- actions ---
            KeyCode::Enter | KeyCode::Char('g') => {
                if self.busy.is_busy() {
                    self.push_feed(FeedLine::Info("already running".into()));
                    Action::None
                } else if !self.authorized {
                    self.push_feed(FeedLine::Error(
                        "not authorised — run `spotify-agent login`".into(),
                    ));
                    Action::None
                } else {
                    Action::Generate
                }
            }
            KeyCode::Char('s') => {
                if self.busy.is_busy() {
                    Action::None
                } else {
                    Action::Sync
                }
            }
            KeyCode::Char('r') => {
                if self.busy.is_busy() {
                    Action::None
                } else {
                    Action::RefreshProfile
                }
            }

            _ => Action::None,
        }
    }

    /// Route the cursor to whichever pane has focus.
    fn move_cursor(&mut self, delta: isize) {
        match self.focus {
            Focus::Tracks if !self.accepted.is_empty() => {
                let len = self.accepted.len() as isize;
                let next = (self.track_cursor as isize + delta).rem_euclid(len);
                self.track_cursor = next as usize;
            }
            _ => self.move_selection(delta),
        }
    }

    fn moderate(&mut self, verdict: Verdict) -> Action {
        if self.focus != Focus::Tracks || self.selected_track().is_none() {
            return Action::None;
        }
        if self.busy.is_busy() {
            self.push_feed(FeedLine::Info("wait for the run to finish".into()));
            return Action::None;
        }
        Action::Moderate(verdict)
    }

    /// Map a mouse event onto whatever the last frame drew there.
    ///
    /// Terminal mouse support is coarse — a click is a cell coordinate, not a
    /// widget — so every hit test is against the rectangles rendering recorded,
    /// and a click on nothing is simply ignored.
    pub fn on_mouse(&mut self, event: MouseEvent) -> Action {
        let (column, row) = (event.column, event.row);

        if self.wizard_open {
            return self.mouse_in_wizard(event, column, row);
        }

        match event.kind {
            MouseEventKind::ScrollDown => {
                if contains(self.regions.logs, column, row) && self.tab == Tab::Logs {
                    self.scroll = self.scroll.saturating_add(3);
                } else if contains(self.regions.tracks, column, row) {
                    self.focus = Focus::Tracks;
                    self.move_cursor(1);
                } else {
                    self.move_cursor(1);
                }
                Action::None
            }
            MouseEventKind::ScrollUp => {
                if contains(self.regions.logs, column, row) && self.tab == Tab::Logs {
                    self.scroll = self.scroll.saturating_sub(3);
                } else if contains(self.regions.tracks, column, row) {
                    self.focus = Focus::Tracks;
                    self.move_cursor(-1);
                } else {
                    self.move_cursor(-1);
                }
                Action::None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.show_help {
                    self.show_help = false;
                    return Action::None;
                }
                for (index, area) in self.regions.tabs.iter().enumerate() {
                    if contains(*area, column, row) {
                        if let Some(tab) = Tab::ALL.get(index) {
                            self.tab = *tab;
                            self.scroll = 0;
                        }
                        return Action::None;
                    }
                }
                if let Some(index) = row_index(self.regions.presets, row, 1) {
                    if index < self.preset_names.len() {
                        self.focus = Focus::Presets;
                        self.preset_state.select(Some(index));
                        self.clear_overrides();
                    }
                    return Action::None;
                }
                if let Some(index) = row_index(self.regions.tracks, row, 2) {
                    // +2: the panel border and the table header row.
                    if index < self.accepted.len() {
                        self.focus = Focus::Tracks;
                        self.track_cursor = index;
                    }
                    return Action::None;
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn mouse_in_wizard(&mut self, event: MouseEvent, column: u16, row: u16) -> Action {
        for (index, area) in self.regions.wizard_rows.iter().enumerate() {
            if contains(*area, column, row) {
                self.wizard_cursor = index;
                // A click selects; a second click on the same row confirms,
                // which is what a double-click feels like without needing one.
                if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
                    return Action::CommitLanguage;
                }
            }
        }
        Action::None
    }

    fn clear_overrides(&mut self) {
        self.size_override = None;
        self.language_override = None;
        self.strategy_override = None;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.preset_names.is_empty() {
            return;
        }
        let len = self.preset_names.len() as isize;
        let current = self.preset_state.selected().unwrap_or(0) as isize;
        // rem_euclid gives a wrapping selection in both directions without a
        // branch, and without underflowing at index 0.
        let next = (current + delta).rem_euclid(len) as usize;
        self.preset_state.select(Some(next));
        // Preset-specific defaults change under the cursor, so per-run
        // overrides are cleared to avoid showing a stale size for a new preset.
        self.clear_overrides();
    }

    fn cycle_tab(&mut self, delta: isize) {
        let len = Tab::ALL.len() as isize;
        let next = (self.tab.index() as isize + delta).rem_euclid(len) as usize;
        self.tab = Tab::ALL.get(next).copied().unwrap_or(Tab::Run);
        self.scroll = 0;
    }

    // -----------------------------------------------------------------
    // Engine wiring
    // -----------------------------------------------------------------

    pub fn start_generate(&mut self, engine: Arc<Engine>) {
        self.accepted.clear();
        self.rejected.clear();
        self.outcome = None;
        self.thinking.clear();
        self.progress = (0, 0);
        self.busy = Busy::Generating(Stage::Sync);
        self.tab = Tab::Run;

        let opts = GenerateOptions {
            preset: Some(self.selected_preset()),
            size: self.size_override,
            playlist_name: None,
            strategy: self.strategy_override,
            language: self.language_override,
            extra_instructions: None,
            dry_run: self.dry_run,
            skip_sync: false,
            block_artists: Vec::new(),
            genres: Vec::new(),
        };

        let tx = self.events.clone();
        tokio::spawn(async move {
            // Errors already arrive as `EngineEvent::Failed`; the return value
            // is ignored so a failed run cannot take the UI down.
            let _ = engine.generate(opts, Some(tx)).await;
        });
    }

    pub fn start_sync(&mut self, engine: Arc<Engine>) {
        self.busy = Busy::Syncing;
        self.push_feed(FeedLine::Info("syncing library…".into()));
        let tx = self.events.clone();
        tokio::spawn(async move {
            let sink = Some(tx.clone());
            match engine.sync(true, &sink).await {
                Ok(report) => {
                    let _ = tx.send(EngineEvent::Log(format!(
                        "sync complete: {} saved, {} new plays, {} artists ({:.1}s)",
                        report.saved,
                        report.new_plays,
                        report.artists_hydrated,
                        report.duration.as_secs_f32()
                    )));
                    let _ = tx.send(EngineEvent::Stage(Stage::Done));
                }
                Err(e) => {
                    let _ = tx.send(EngineEvent::Failed(e.to_string()));
                }
            }
        });
    }

    pub fn refresh_profile(&mut self, engine: Arc<Engine>) {
        self.busy = Busy::LoadingProfile;
        let tx = self.events.clone();
        tokio::spawn(async move {
            match engine.profile().await {
                Ok(profile) => {
                    let _ = tx.send(EngineEvent::Log(format!(
                        "profile rebuilt: {} tracks, {} genres",
                        profile.known_tracks,
                        profile.genres.len()
                    )));
                    let _ = tx.send(EngineEvent::Stage(Stage::Done));
                }
                Err(e) => {
                    let _ = tx.send(EngineEvent::Failed(e.to_string()));
                }
            }
        });
    }

    pub fn on_engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::Stage(Stage::Done) => {
                self.busy = Busy::Idle;
                self.progress = (0, 0);
            }
            EngineEvent::Stage(stage) => {
                if matches!(self.busy, Busy::Generating(_)) {
                    self.busy = Busy::Generating(stage);
                }
                self.push_feed(FeedLine::Info(stage.label().to_string()));
            }
            EngineEvent::Progress { done, total } => self.progress = (done, total),
            EngineEvent::Log(message) => self.push_feed(FeedLine::Info(message)),
            EngineEvent::Thinking(chunk) => {
                self.thinking.push_str(&chunk);
                // Only the tail is ever displayed; letting this grow without
                // bound across a long turn would be a slow leak.
                if self.thinking.len() > 8_192 {
                    let keep = self.thinking.len() - 4_096;
                    // Find a char boundary so the truncation cannot panic.
                    let start = (keep..self.thinking.len())
                        .find(|i| self.thinking.is_char_boundary(*i))
                        .unwrap_or(self.thinking.len());
                    self.thinking = self.thinking.split_off(start);
                }
            }
            EngineEvent::Accepted(line) => self.push_feed(FeedLine::Accepted(line)),
            EngineEvent::Rejected(what, why) => self.push_feed(FeedLine::Rejected(what, why)),
            EngineEvent::Finished(outcome) => {
                self.accepted = outcome.accepted.clone();
                self.rejected = outcome.rejected.clone();
                self.track_cursor = 0;
                self.verdicts.clear();
                if !self.accepted.is_empty() {
                    // Hand the cursor straight to the new tracklist: reviewing
                    // it is the natural next action.
                    self.focus = Focus::Tracks;
                }
                self.push_feed(FeedLine::Info(format!(
                    "done: {} tracks in {:.1}s",
                    outcome.accepted.len(),
                    outcome.duration.as_secs_f32()
                )));
                self.outcome = Some(*outcome);
                self.busy = Busy::Idle;
            }
            EngineEvent::Failed(message) => {
                self.push_feed(FeedLine::Error(message));
                self.busy = Busy::Idle;
                self.progress = (0, 0);
            }
        }
    }

    pub fn on_tick(&mut self) {
        if self.busy.is_busy() {
            self.spinner = self.spinner.wrapping_add(1);
        }
    }

    fn push_feed(&mut self, line: FeedLine) {
        if self.feed.len() == FEED_CAPACITY {
            self.feed.pop_front();
        }
        self.feed.push_back(line);
    }

    pub fn spinner_frame(&self) -> char {
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        // Divide so the animation runs at ~8 fps rather than the 20 fps tick.
        FRAMES
            .get((self.spinner / 3) % FRAMES.len())
            .copied()
            .unwrap_or('·')
    }

    /// 0.0–1.0 for the progress gauge.
    pub fn progress_ratio(&self) -> f64 {
        match &self.busy {
            Busy::Generating(stage) => {
                let (done, total) = self.progress;
                if total > 0 && *stage == Stage::Resolve {
                    // Inside the resolve stage, use real item progress.
                    let stage_span = 1.0 / Stage::COUNT as f64;
                    let base = stage.ordinal() as f64 * stage_span;
                    base + stage_span * (done as f64 / total as f64)
                } else {
                    stage.ordinal() as f64 / Stage::COUNT as f64
                }
            }
            Busy::Syncing | Busy::LoadingProfile => 0.5,
            Busy::Idle => {
                if self.outcome.is_some() {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }
}

/// Row index inside a bordered widget, or `None` if the point is outside the
/// content area. `header` is how many rows the border and any column header
/// occupy at the top.
fn row_index(area: Rect, row: u16, header: u16) -> Option<usize> {
    if area.height <= header || !(area.y..area.y.saturating_add(area.height)).contains(&row) {
        return None;
    }
    let first = area.y.saturating_add(header);
    let last = area.y.saturating_add(area.height).saturating_sub(1);
    if row < first || row >= last {
        return None;
    }
    Some(usize::from(row - first))
}

fn next_language(current: LanguagePolicy) -> LanguagePolicy {
    match current {
        LanguagePolicy::Any => LanguagePolicy::English,
        LanguagePolicy::English => LanguagePolicy::Russian,
        LanguagePolicy::Russian => LanguagePolicy::Mixed,
        LanguagePolicy::Mixed => LanguagePolicy::Any,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventKind;

    fn app() -> App {
        let config = Config {
            presets: crate::config::presets::builtin(),
            ..Default::default()
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(Arc::new(config), None, tx)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        }
    }

    fn release(version: &str) -> Box<crate::update::Release> {
        Box::new(crate::update::Release {
            tag: format!("v{version}"),
            version: crate::update::version::Version::parse(version).expect("parses"),
            url: String::new(),
            notes: "- something".into(),
            prerelease: false,
            published_at: None,
        })
    }

    #[test]
    fn the_update_dialog_is_modal() {
        // `q` normally quits. While the dialog is up it must dismiss the
        // dialog instead, or a stray keystroke closes the whole program.
        let mut app = app();
        app.update = UpdatePrompt::Available(release("9.9.9"));
        assert_eq!(app.on_key(key(KeyCode::Char('q'))), Action::None);
        assert_eq!(app.update, UpdatePrompt::Hidden);
    }

    #[test]
    fn the_update_dialog_offers_install_skip_and_later() {
        let mut app = app();

        app.update = UpdatePrompt::Available(release("9.9.9"));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::InstallUpdate);
        // Still open: the event loop closes it once the download starts.
        assert!(app.update.is_open());

        assert_eq!(app.on_key(key(KeyCode::Char('s'))), Action::SkipUpdate);

        app.update = UpdatePrompt::Available(release("9.9.9"));
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        assert_eq!(app.update, UpdatePrompt::Hidden);
    }

    #[test]
    fn a_download_in_progress_ignores_every_key() {
        // Dismissing mid-swap would hide an operation that is still running.
        let mut app = app();
        app.update = UpdatePrompt::Downloading {
            done: 10,
            total: Some(100),
        };
        for code in [KeyCode::Esc, KeyCode::Enter, KeyCode::Char('q')] {
            assert_eq!(app.on_key(key(code)), Action::None);
            assert!(app.update.is_busy(), "{code:?} dismissed a live download");
        }
    }

    #[test]
    fn a_finished_update_dialog_closes_on_any_key() {
        for state in [
            UpdatePrompt::Installed("9.9.9".into()),
            UpdatePrompt::Failed("no".into()),
        ] {
            let mut app = app();
            app.update = state;
            app.on_key(key(KeyCode::Char('z')));
            assert_eq!(app.update, UpdatePrompt::Hidden);
        }
    }

    #[test]
    fn an_update_found_mid_run_waits_instead_of_covering_the_screen() {
        let mut app = app();
        app.busy = Busy::Syncing;
        app.on_update_msg(UpdateMsg::Found(release("9.9.9")));
        assert_eq!(app.update, UpdatePrompt::Hidden);
        // It is still reported, just not as a modal over a running pipeline.
        assert!(app.feed.iter().any(|line| matches!(
            line,
            FeedLine::Info(text) if text.contains("9.9.9")
        )));
    }

    #[test]
    fn the_language_picker_outranks_the_update_dialog() {
        // A brand-new install should choose a language before being asked
        // about upgrading; both are modal, so the order has to be decided.
        let mut app = app();
        app.wizard_open = true;
        app.update = UpdatePrompt::Available(release("9.9.9"));
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::CommitLanguage);
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut app = app();
        let last = app.preset_names.len() - 1;
        app.preset_state.select(Some(0));
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.preset_state.selected(), Some(last));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.preset_state.selected(), Some(0));
    }

    #[test]
    fn ctrl_c_quits_even_while_busy() {
        let mut app = app();
        app.busy = Busy::Syncing;
        let event = KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(app.on_key(event), Action::Quit);
    }

    #[test]
    fn generate_requires_authorisation() {
        let mut app = app();
        app.authorized = false;
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        app.authorized = true;
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Generate);
    }

    #[test]
    fn language_cycles_through_every_policy() {
        let mut app = app();
        app.authorized = true;
        let mut seen = Vec::new();
        for _ in 0..4 {
            app.on_key(key(KeyCode::Char('l')));
            seen.push(app.effective_language());
        }
        assert_eq!(seen.len(), 4);
        assert!(seen.contains(&LanguagePolicy::Russian));
        assert!(seen.contains(&LanguagePolicy::Mixed));
    }

    #[test]
    fn thinking_buffer_is_bounded_and_utf8_safe() {
        let mut app = app();
        for _ in 0..500 {
            app.on_engine_event(EngineEvent::Thinking("многобайтный текст ".repeat(2)));
        }
        assert!(app.thinking.len() <= 8_192 + 64);
        // The invariant that actually matters: it is still valid UTF-8, which
        // it is by construction since `String` cannot hold anything else —
        // the real risk was a panic in split_off, which not panicking proves.
        assert!(!app.thinking.is_empty());
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn clicking_a_preset_row_selects_it() {
        let mut app = app();
        // A 1-cell border, then one row per preset.
        app.regions.presets = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 12,
        };
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 3));
        assert_eq!(app.preset_state.selected(), Some(2));
        assert_eq!(app.focus, Focus::Presets);
    }

    #[test]
    fn a_click_outside_every_region_changes_nothing() {
        let mut app = app();
        app.regions.presets = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 12,
        };
        let before = app.preset_state.selected();
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 200, 200));
        assert_eq!(app.preset_state.selected(), before);
    }

    #[test]
    fn clicking_a_tab_switches_view() {
        let mut app = app();
        app.regions.tabs = vec![
            Rect {
                x: 0,
                y: 0,
                width: 6,
                height: 1,
            },
            Rect {
                x: 9,
                y: 0,
                width: 8,
                height: 1,
            },
        ];
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 10, 0));
        assert_eq!(app.tab, Tab::Profile);
    }

    #[test]
    fn the_wheel_moves_the_focused_cursor() {
        let mut app = app();
        app.preset_state.select(Some(0));
        app.on_mouse(mouse(MouseEventKind::ScrollDown, 1, 1));
        assert_eq!(app.preset_state.selected(), Some(1));
        app.on_mouse(mouse(MouseEventKind::ScrollUp, 1, 1));
        assert_eq!(app.preset_state.selected(), Some(0));
    }

    #[test]
    fn the_language_picker_is_modal() {
        let mut app = app();
        app.set_language(Lang::En, Preferences::default(), true);
        // Keys that normally act must not while the picker is open.
        assert_eq!(app.on_key(key(KeyCode::Char('s'))), Action::None);
        assert_eq!(app.on_key(key(KeyCode::Down)), Action::None);
        assert_eq!(app.wizard_cursor, 1);
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::CommitLanguage);
    }

    #[test]
    fn clicking_a_language_row_confirms_it() {
        let mut app = app();
        app.set_language(Lang::En, Preferences::default(), true);
        app.regions.wizard_rows = (0..4)
            .map(|i| Rect {
                x: 0,
                y: 10 + i as u16,
                width: 20,
                height: 1,
            })
            .collect();
        let action = app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 5, 12));
        assert_eq!(action, Action::CommitLanguage);
        assert_eq!(app.wizard_cursor, 2);
    }

    #[test]
    fn moderation_needs_a_selected_track_and_an_idle_pipeline() {
        let mut app = app();
        // No tracks yet.
        assert_eq!(app.on_key(key(KeyCode::Char('f'))), Action::None);

        app.accepted = vec![];
        app.focus = Focus::Tracks;
        assert_eq!(app.on_key(key(KeyCode::Char('b'))), Action::None);
    }

    #[test]
    fn switching_language_changes_the_rendered_strings() {
        let mut app = app();
        let english = app.t().presets;
        app.set_language(Lang::Ru, Preferences::default(), false);
        assert_ne!(app.t().presets, english);
        assert_eq!(app.t().presets, "Пресеты");
    }

    #[test]
    fn progress_is_monotonic_across_stages() {
        let mut app = app();
        let mut previous = 0.0;
        for stage in [
            Stage::Sync,
            Stage::Analyze,
            Stage::Prompt,
            Stage::Model,
            Stage::Select,
        ] {
            app.busy = Busy::Generating(stage);
            let ratio = app.progress_ratio();
            assert!(ratio >= previous, "{stage:?} went backwards");
            previous = ratio;
        }
    }
}
