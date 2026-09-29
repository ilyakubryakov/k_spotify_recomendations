//! Rendering. Reads [`App`], draws frames, mutates nothing but widget state.
//!
//! The palette is a muted dark scheme with one accent (Spotify green) reserved
//! for "this is the thing you act on". Everything informational is grey; only
//! status uses colour, so colour stays meaningful.

use super::app::{App, Busy, FeedLine, Focus, Tab};
use crate::i18n::Lang;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Gauge, List, ListItem, Paragraph, Row, Table, Tabs, Wrap,
};

// --- palette ---------------------------------------------------------------
const ACCENT: Color = Color::Rgb(30, 215, 96); // Spotify green
const ACCENT_DIM: Color = Color::Rgb(22, 150, 68);
const FG: Color = Color::Rgb(220, 220, 224);
const MUTED: Color = Color::Rgb(128, 128, 136);
const FAINT: Color = Color::Rgb(78, 78, 86);
const WARN: Color = Color::Rgb(232, 168, 72);
const ERROR: Color = Color::Rgb(226, 96, 96);
const BORDER: Color = Color::Rgb(56, 56, 64);

fn panel(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { ACCENT_DIM } else { BORDER }))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(if focused { ACCENT } else { MUTED })
                .add_modifier(Modifier::BOLD),
        ))
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // A terminal this small cannot show anything useful; say so instead of
    // rendering a scrambled layout.
    if area.width < 70 || area.height < 18 {
        let message = Paragraph::new(app.t().terminal_too_small)
            .style(Style::default().fg(WARN))
            .alignment(Alignment::Center);
        frame.render_widget(message, area);
        return;
    }

    let [header, body, progress, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);

    render_header(frame, app, header);

    let [rail, main] =
        Layout::horizontal([Constraint::Length(30), Constraint::Min(40)]).areas(body);
    let [presets, options] =
        Layout::vertical([Constraint::Min(8), Constraint::Length(9)]).areas(rail);

    app.regions.presets = presets;
    render_presets(frame, app, presets);
    render_options(frame, app, options);
    render_main(frame, app, main);
    render_progress(frame, app, progress);
    render_footer(frame, app, footer);

    if app.show_help {
        render_help(frame, app, area);
    }
    // The picker is modal and painted last so it sits above everything.
    if app.wizard_open {
        render_language_wizard(frame, app, area);
    }
}

// ---------------------------------------------------------------------------

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let t = app.t();
    let auth = if app.authorized {
        Span::styled(format!("● {}", t.authorised), Style::default().fg(ACCENT))
    } else {
        Span::styled(
            format!("● {}", t.not_authorised),
            Style::default().fg(ERROR),
        )
    };

    let line = Line::from(vec![
        Span::styled(
            "spotify-agent",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  │  ", Style::default().fg(FAINT)),
        Span::styled(format!("{} ", t.model), Style::default().fg(MUTED)),
        Span::styled(app.config.claude.model.clone(), Style::default().fg(FG)),
        Span::styled("  │  ", Style::default().fg(FAINT)),
        Span::styled(format!("{} ", t.effort), Style::default().fg(MUTED)),
        Span::styled(app.config.claude.effort.as_str(), Style::default().fg(FG)),
        Span::styled("  │  ", Style::default().fg(FAINT)),
        auth,
    ]);

    frame.render_widget(
        Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER)),
        ),
        area,
    );
}

fn render_presets(frame: &mut Frame, app: &mut App, area: Rect) {
    let selected = app.preset_state.selected();
    let items: Vec<ListItem> = app
        .preset_names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let is_selected = selected == Some(index);
            let is_default = *name == app.config.defaults.preset;
            let mut spans = vec![Span::styled(
                name.clone(),
                Style::default()
                    .fg(if is_selected { ACCENT } else { FG })
                    .add_modifier(if is_selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            )];
            if is_default {
                spans.push(Span::styled(" ·", Style::default().fg(FAINT)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(panel(app.t().presets, app.focus == Focus::Presets))
        .highlight_style(Style::default().bg(Color::Rgb(28, 40, 32)))
        .highlight_symbol("▌");

    frame.render_stateful_widget(list, area, &mut app.preset_state);
}

fn render_options(frame: &mut Frame, app: &App, area: Rect) {
    let preset = app.selected_preset();
    let preset_label = app
        .config
        .resolve_preset(&preset)
        .map(|p| p.label)
        .unwrap_or_default();

    let t = app.t();
    let toggle = |on: bool| {
        if on {
            Span::styled(t.on, Style::default().fg(WARN))
        } else {
            Span::styled(t.off, Style::default().fg(MUTED))
        }
    };
    let label = |text: &str| format!("{text:<10}");

    let lines = vec![
        Line::from(Span::styled(preset_label, Style::default().fg(MUTED))),
        Line::from(""),
        Line::from(vec![
            Span::styled(label(t.size), Style::default().fg(MUTED)),
            Span::styled(app.effective_size().to_string(), Style::default().fg(FG)),
            Span::styled("  -/+", Style::default().fg(FAINT)),
        ]),
        Line::from(vec![
            Span::styled(label(t.language), Style::default().fg(MUTED)),
            Span::styled(app.effective_language().as_str(), Style::default().fg(FG)),
            Span::styled("  l", Style::default().fg(FAINT)),
        ]),
        Line::from(vec![
            Span::styled(label(t.mode), Style::default().fg(MUTED)),
            Span::styled(app.effective_strategy().as_str(), Style::default().fg(FG)),
            Span::styled("  m", Style::default().fg(FAINT)),
        ]),
        Line::from(vec![
            Span::styled(label(t.dry_run), Style::default().fg(MUTED)),
            toggle(app.dry_run),
            Span::styled("  d", Style::default().fg(FAINT)),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(panel(t.run_options, false)),
        area,
    );
}

fn render_main(frame: &mut Frame, app: &mut App, area: Rect) {
    let [tabs_area, content] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(4)]).areas(area);

    let t = app.t();
    let labels: Vec<String> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(index, tab)| format!("{} {}", index + 1, tab.title(t)))
        .collect();

    // Record where each tab landed so a click can select it. `Tabs` renders
    // `label` + a one-space divider on each side, which is what the +3 below
    // accounts for; getting it slightly wrong only makes the click target a
    // cell narrow, never wrong.
    app.regions.tabs.clear();
    let mut x = tabs_area.x;
    for label in &labels {
        let width = label.chars().count() as u16;
        app.regions.tabs.push(Rect {
            x,
            y: tabs_area.y,
            width,
            height: 1,
        });
        x = x.saturating_add(width).saturating_add(3);
    }

    let titles: Vec<Line> = labels
        .iter()
        .map(|label| Line::from(Span::raw(label.clone())))
        .collect();

    frame.render_widget(
        Tabs::new(titles)
            .select(app.tab.index())
            .style(Style::default().fg(MUTED))
            .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
            .divider(Span::styled("│", Style::default().fg(FAINT))),
        tabs_area,
    );

    match app.tab {
        Tab::Run => render_run_tab(frame, app, content),
        Tab::Profile => render_profile_tab(frame, app, content),
        Tab::Logs => render_logs_tab(frame, app, content),
        Tab::Setup => render_setup_tab(frame, app, content),
    }
}

fn render_run_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    // While the model is thinking there are no tracks yet, so the pane shows
    // the reasoning summary instead of an empty table.
    let show_thinking = matches!(app.busy, Busy::Generating(crate::engine::Stage::Model))
        && !app.thinking.is_empty();

    let [top, bottom] = if show_thinking {
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(area)
    } else {
        Layout::vertical([Constraint::Min(6), Constraint::Length(9)]).areas(area)
    };

    if show_thinking {
        let tail: String = app
            .thinking
            .chars()
            .rev()
            .take(1_200)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        frame.render_widget(
            Paragraph::new(tail)
                .style(Style::default().fg(MUTED).add_modifier(Modifier::ITALIC))
                .wrap(Wrap { trim: true })
                .block(panel(app.t().reasoning, false)),
            top,
        );
        app.regions.tracks = Rect::default();
    } else {
        app.regions.tracks = top;
        render_tracks(frame, app, top);
    }

    // With a tracklist on screen, the lower pane shows the model's rationale
    // for the selected track — the "why this track?" journal — instead of the
    // activity feed, which has already served its purpose by then.
    if !app.accepted.is_empty() && !show_thinking {
        let [reason, feed] =
            Layout::vertical([Constraint::Length(5), Constraint::Min(4)]).areas(bottom);
        render_reason(frame, app, reason);
        render_feed(frame, app, feed);
    } else {
        render_feed(frame, app, bottom);
    }
}

/// The stored rationale for the highlighted track.
fn render_reason(frame: &mut Frame, app: &App, area: Rect) {
    let t = app.t();
    let Some(item) = app.selected_track() else {
        frame.render_widget(
            Paragraph::new("").block(panel(t.why_this_track, false)),
            area,
        );
        return;
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(item.track.artist_line(), Style::default().fg(ACCENT)),
        Span::styled(" — ", Style::default().fg(FAINT)),
        Span::styled(item.track.name.clone(), Style::default().fg(FG)),
    ])];

    if !item.suggestion.reason.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            item.suggestion.reason.trim().to_string(),
            Style::default().fg(MUTED),
        )));
    }
    if let Some(verdict) = app.verdict_for(&item.track.id) {
        let label = match verdict {
            super::app::Verdict::Keep => t.verdict_keep,
            super::app::Verdict::Drop => t.verdict_drop,
            super::app::Verdict::BanArtist => t.verdict_ban,
            super::app::Verdict::Up => t.verdict_up,
            super::app::Verdict::Down => t.verdict_down,
        };
        lines.push(Line::from(Span::styled(
            format!("✓ {label}"),
            Style::default().fg(WARN),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel(t.why_this_track, false)),
        area,
    );
}

fn render_tracks(frame: &mut Frame, app: &App, area: Rect) {
    let t = app.t();
    let title = match &app.outcome {
        Some(outcome) => format!(
            "{} — {}{}",
            outcome.playlist_title,
            outcome.accepted.len(),
            if outcome.dry_run {
                format!(" ({})", t.dry_run)
            } else {
                String::new()
            }
        ),
        None => t.tracks.to_string(),
    };

    if app.accepted.is_empty() {
        let hint = if app.busy.is_busy() {
            app.t().working
        } else {
            app.t().select_preset_hint
        };
        frame.render_widget(
            Paragraph::new(hint)
                .style(Style::default().fg(FAINT))
                .alignment(Alignment::Center)
                .block(panel(&title, false)),
            area,
        );
        return;
    }

    let rows: Vec<Row> = app
        .accepted
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let selected = index == app.track_cursor && app.focus == Focus::Tracks;
            // One glyph carries both the cursor and any verdict already given,
            // so the review pass needs no extra column.
            let marker = match app.verdict_for(&item.track.id) {
                Some(super::app::Verdict::Keep) | Some(super::app::Verdict::Up) => "+",
                Some(super::app::Verdict::Drop) | Some(super::app::Verdict::Down) => "-",
                Some(super::app::Verdict::BanArtist) => "×",
                None if selected => "▌",
                None => " ",
            };
            let base = if selected {
                Style::default().bg(Color::Rgb(28, 40, 32))
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(format!("{marker}{:>3}", index + 1)).style(base.fg(if selected {
                    ACCENT
                } else {
                    FAINT
                })),
                Cell::from(item.track.artist_line()).style(base.fg(ACCENT)),
                Cell::from(item.track.name.clone()).style(base.fg(FG)),
                Cell::from(item.track.duration_display()).style(base.fg(MUTED)),
                Cell::from(item.suggestion.mood.clone()).style(base.fg(MUTED)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Percentage(30),
            Constraint::Percentage(44),
            Constraint::Length(6),
            Constraint::Percentage(16),
        ],
    )
    .header(
        Row::new(vec![" #", "ARTIST", "TITLE", "TIME", "MOOD"])
            .style(Style::default().fg(FAINT).add_modifier(Modifier::BOLD)),
    )
    .block(panel(&title, app.focus == Focus::Tracks))
    .column_spacing(1);

    frame.render_widget(table, area);
}

fn render_feed(frame: &mut Frame, app: &App, area: Rect) {
    // Reserve two rows for the border.
    let capacity = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = app
        .feed
        .iter()
        .rev()
        .take(capacity)
        .rev()
        .map(|line| match line {
            FeedLine::Info(text) => Line::from(vec![
                Span::styled("· ", Style::default().fg(FAINT)),
                Span::styled(text.clone(), Style::default().fg(MUTED)),
            ]),
            FeedLine::Accepted(text) => Line::from(vec![
                Span::styled("+ ", Style::default().fg(ACCENT)),
                Span::styled(text.clone(), Style::default().fg(FG)),
            ]),
            FeedLine::Rejected(what, why) => Line::from(vec![
                Span::styled("– ", Style::default().fg(FAINT)),
                Span::styled(what.clone(), Style::default().fg(FAINT)),
                Span::styled(format!("  {why}"), Style::default().fg(FAINT)),
            ]),
            FeedLine::Error(text) => Line::from(vec![
                Span::styled("! ", Style::default().fg(ERROR)),
                Span::styled(text.clone(), Style::default().fg(ERROR)),
            ]),
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(panel(app.t().activity, false)),
        area,
    );
}

fn render_profile_tab(frame: &mut Frame, app: &App, area: Rect) {
    let t = app.t();
    let Some(profile) = &app.profile else {
        frame.render_widget(
            Paragraph::new(t.no_profile_hint)
                .style(Style::default().fg(FAINT))
                .alignment(Alignment::Center)
                .block(panel(t.profile, false)),
            area,
        );
        return;
    };

    let [summary, columns] =
        Layout::vertical([Constraint::Length(7), Constraint::Min(6)]).areas(area);

    let stat = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label:<12}"), Style::default().fg(MUTED)),
            Span::styled(value, Style::default().fg(FG)),
        ])
    };

    frame.render_widget(
        Paragraph::new(vec![
            stat(
                t.library,
                format!(
                    "{} tracks · {} artists · {} liked",
                    profile.known_tracks, profile.known_artists, profile.saved_tracks
                ),
            ),
            stat(
                t.plays,
                format!(
                    "{} logged · {} distinct",
                    profile.play_events, profile.distinct_played
                ),
            ),
            stat(t.era, profile.era.describe()),
            stat(t.languages, profile.script_mix.describe()),
            stat(
                t.typical,
                format!(
                    "popularity {}/100 · {}:{:02}",
                    profile.median_popularity,
                    profile.median_duration_secs / 60,
                    profile.median_duration_secs % 60
                ),
            ),
        ])
        .block(panel(t.overview, false)),
        summary,
    );

    let [artists, genres, looped] = Layout::horizontal([
        Constraint::Percentage(38),
        Constraint::Percentage(30),
        Constraint::Percentage(32),
    ])
    .areas(columns);

    let rows = (artists.height.saturating_sub(2)) as usize;

    frame.render_widget(
        Paragraph::new(
            profile
                .top_artists
                .iter()
                .take(rows)
                .map(|a| {
                    Line::from(vec![
                        Span::styled(format!("{:>5.1} ", a.score), Style::default().fg(FAINT)),
                        Span::styled(a.name.clone(), Style::default().fg(FG)),
                    ])
                })
                .collect::<Vec<_>>(),
        )
        .block(panel(t.core_artists, false)),
        artists,
    );

    frame.render_widget(
        Paragraph::new(
            profile
                .genres
                .iter()
                .take(rows)
                .map(|g| {
                    Line::from(vec![
                        Span::styled(
                            format!("{:>3} ", g.artist_count),
                            Style::default().fg(FAINT),
                        ),
                        Span::styled(g.genre.clone(), Style::default().fg(FG)),
                    ])
                })
                .collect::<Vec<_>>(),
        )
        .block(panel(t.genres, false)),
        genres,
    );

    frame.render_widget(
        Paragraph::new(if profile.looped.is_empty() {
            vec![Line::from(Span::styled(
                t.nothing_on_repeat,
                Style::default().fg(FAINT),
            ))]
        } else {
            profile
                .looped
                .iter()
                .take(rows)
                .map(|t| {
                    Line::from(vec![
                        Span::styled(
                            format!("{:>3}× ", t.recent_plays),
                            Style::default().fg(ACCENT),
                        ),
                        Span::styled(t.name.clone(), Style::default().fg(FG)),
                    ])
                })
                .collect::<Vec<_>>()
        })
        .block(panel(t.on_repeat, false)),
        looped,
    );
}

fn render_logs_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    app.regions.logs = area;
    let capacity = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = match &app.log_buffer {
        Some(buffer) => buffer
            .tail(capacity)
            .into_iter()
            .map(|line| {
                let color = match line.level {
                    tracing::Level::ERROR => ERROR,
                    tracing::Level::WARN => WARN,
                    tracing::Level::INFO => FG,
                    _ => FAINT,
                };
                // The crate prefix is the same on every line; only the
                // module tail carries information.
                let module = line.target.rsplit("::").next().unwrap_or("").to_string();
                Line::from(vec![
                    Span::styled(
                        line.at.format("%H:%M:%S ").to_string(),
                        Style::default().fg(FAINT),
                    ),
                    Span::styled(format!("{:<5} ", line.level), Style::default().fg(color)),
                    Span::styled(format!("{module:<10} "), Style::default().fg(FAINT)),
                    Span::styled(line.message, Style::default().fg(MUTED)),
                ])
            })
            .collect(),
        None => vec![Line::from(Span::styled(
            app.t().log_capture_off,
            Style::default().fg(FAINT),
        ))],
    };

    let title = match &app.log_buffer {
        Some(buffer) => format!("{} ({})", app.t().logs, buffer.len()),
        None => app.t().logs.to_string(),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((app.scroll, 0))
            .block(panel(&title, false)),
        area,
    );
}

fn render_progress(frame: &mut Frame, app: &App, area: Rect) {
    let (label, ratio) = match &app.busy {
        Busy::Idle => match &app.outcome {
            Some(outcome) => (
                format!(
                    "{} tracks · {} in / {} out tokens · {:.1}s",
                    outcome.accepted.len(),
                    outcome.input_tokens,
                    outcome.output_tokens,
                    outcome.duration.as_secs_f32()
                ),
                1.0,
            ),
            None => (app.t().idle.to_string(), 0.0),
        },
        Busy::Generating(stage) => {
            let (done, total) = app.progress;
            let detail = if total > 0 {
                format!(" {done}/{total}")
            } else {
                String::new()
            };
            (
                format!(
                    "{} {}{}",
                    app.spinner_frame(),
                    stage_label(*stage, app.t()),
                    detail
                ),
                app.progress_ratio(),
            )
        }
        Busy::Syncing => (
            format!("{} {}", app.spinner_frame(), app.t().stage_sync),
            0.5,
        ),
        Busy::LoadingProfile => (
            format!("{} {}", app.spinner_frame(), app.t().stage_analyze),
            0.5,
        ),
    };

    frame.render_widget(
        Gauge::default()
            .block(panel(app.t().pipeline, false))
            .gauge_style(Style::default().fg(ACCENT).bg(Color::Rgb(32, 32, 38)))
            .ratio(ratio.clamp(0.0, 1.0))
            .label(Span::styled(
                label,
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )),
        area,
    );
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let t = app.t();
    let key = |k: &str, what: &str| {
        vec![
            Span::styled(k.to_string(), Style::default().fg(ACCENT)),
            Span::styled(format!(" {what}  "), Style::default().fg(FAINT)),
        ]
    };

    let mut spans = Vec::new();
    spans.extend(key("↑↓", t.key_navigate));
    spans.extend(key("⏎", t.key_generate));
    // With a list on screen the moderation keys are what matters; before that
    // they would be noise.
    if app.focus == Focus::Tracks && !app.accepted.is_empty() {
        spans.extend(key("f", t.key_keep));
        spans.extend(key("x", t.key_drop));
        spans.extend(key("b", t.key_ban));
    } else {
        spans.extend(key("s", t.key_sync));
        spans.extend(key("r", t.key_reload));
        spans.extend(key("d", t.key_dry_run));
    }
    spans.extend(key("tab", t.key_view));
    spans.extend(key("?", t.key_help));
    spans.extend(key("q", t.key_quit));

    if let Some(fatal) = &app.fatal {
        spans = vec![Span::styled(
            format!(" {fatal}"),
            Style::default().fg(ERROR),
        )];
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Stage label in the active language.
fn stage_label(stage: crate::engine::Stage, t: &'static crate::i18n::Strings) -> &'static str {
    use crate::engine::Stage;
    match stage {
        Stage::Sync => t.stage_sync,
        Stage::Analyze => t.stage_analyze,
        Stage::Prompt => t.stage_prompt,
        Stage::Model => t.stage_model,
        Stage::Resolve => t.stage_resolve,
        Stage::Select => t.stage_select,
        Stage::Publish => t.stage_publish,
        Stage::Done => t.stage_done,
    }
}

/// Credentials pane: what is configured, and exactly how to get what is not.
fn render_setup_tab(frame: &mut Frame, app: &App, area: Rect) {
    let t = app.t();
    let cfg = &app.config;

    let mark = |ok: bool| {
        if ok {
            Span::styled(format!("● {} ", t.setup_done), Style::default().fg(ACCENT))
        } else {
            Span::styled(format!("○ {} ", t.setup_missing), Style::default().fg(WARN))
        }
    };

    let spotify_ready = cfg.spotify.client_id.is_some();
    let llm_ready = cfg
        .claude
        .resolve_api_key()
        .map(|k| k.is_some())
        .unwrap_or(false)
        || cfg
            .llm
            .fallbacks
            .iter()
            .any(|b| b.resolve_api_key().map(|k| k.is_some()).unwrap_or(false));

    let step = |text: &str| {
        Line::from(Span::styled(
            format!("   {text}"),
            Style::default().fg(MUTED),
        ))
    };
    let heading = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
    };

    let mut lines = vec![
        Line::from(Span::styled(t.setup_intro, Style::default().fg(FG))),
        Line::from(""),
        Line::from(vec![
            mark(spotify_ready),
            Span::styled(
                t.setup_spotify_title,
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            ),
        ]),
        step(t.setup_spotify_1),
        step(t.setup_spotify_2),
        Line::from(vec![
            Span::styled("      ", Style::default()),
            Span::styled(cfg.spotify.redirect_uri(), Style::default().fg(ACCENT)),
        ]),
        step(t.setup_spotify_3),
        step(t.setup_spotify_4),
        Line::from(""),
        Line::from(vec![
            mark(llm_ready),
            Span::styled(
                t.setup_llm_title,
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            ),
        ]),
        step(t.setup_llm_1),
        step(t.setup_llm_2),
        step(t.setup_llm_3),
        step(t.setup_llm_local),
        Line::from(""),
        heading(t.setup_config_at),
    ];

    let path = cfg
        .source_path
        .clone()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "spotify-agent config init".to_string());
    lines.push(Line::from(Span::styled(
        format!("   {path}"),
        Style::default().fg(MUTED),
    )));

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel(t.setup, false)),
        area,
    );
}

/// First-run language picker. Modal, keyboard or mouse.
fn render_language_wizard(frame: &mut Frame, app: &mut App, area: Rect) {
    let t = app.t();
    let width = 46.min(area.width.saturating_sub(4));
    let height = (Lang::ALL.len() as u16 + 7).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new("").block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT_DIM))
                .title(Span::styled(
                    format!(" {} ", t.wizard_title),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );

    app.regions.wizard_rows.clear();
    let first_row = popup.y + 2;
    for (index, lang) in Lang::ALL.iter().enumerate() {
        let row = Rect {
            x: popup.x + 1,
            y: first_row + index as u16,
            width: popup.width.saturating_sub(2),
            height: 1,
        };
        app.regions.wizard_rows.push(row);

        let selected = index == app.wizard_cursor;
        let line = Line::from(vec![
            Span::styled(
                if selected { "  ▌ " } else { "    " },
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                lang.endonym(),
                if selected {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(FG)
                },
            ),
        ]);
        frame.render_widget(Paragraph::new(line), row);
    }

    let hint_y = first_row + Lang::ALL.len() as u16 + 1;
    if hint_y < popup.y + popup.height {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    format!("  {}", t.wizard_prompt),
                    Style::default().fg(MUTED),
                )),
                Line::from(Span::styled(
                    format!("  {}", t.wizard_hint),
                    Style::default().fg(FAINT),
                )),
            ]),
            Rect {
                x: popup.x + 1,
                y: hint_y,
                width: popup.width.saturating_sub(2),
                height: 2.min(popup.y + popup.height - hint_y),
            },
        );
    }
}

fn render_help(frame: &mut Frame, app: &App, area: Rect) {
    let t = app.t();
    let width = 62.min(area.width.saturating_sub(4));
    let height = 26.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let row = |k: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<10}"), Style::default().fg(ACCENT)),
            Span::styled(what.to_string(), Style::default().fg(FG)),
        ])
    };

    let heading = |text: &str| {
        Line::from(Span::styled(
            format!("  {text}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
    };

    let lines = vec![
        heading(t.help_navigation),
        row("↑ ↓ j k", t.key_navigate),
        row("← →", t.key_review),
        row("1 2 3 4 / Tab", t.key_view),
        row("PgUp/PgDn", t.key_scroll),
        row("mouse", t.key_mouse),
        Line::from(""),
        heading(t.help_actions),
        row("Enter / g", t.key_generate),
        row("s", t.key_sync),
        row("r", t.key_reload),
        row("d", t.key_dry_run),
        row("- / +", t.key_size),
        row("l", t.key_language),
        row("m", t.key_mode),
        row(",", t.key_settings),
        Line::from(""),
        heading(t.help_moderation),
        row("f", t.key_keep),
        row("x / Del", t.key_drop),
        row("b", t.key_ban),
        row("u", t.key_up),
        row("i", t.key_down),
        Line::from(""),
        row("? / F1", t.key_help),
        row("q / Esc", t.key_quit),
        Line::from(Span::styled(
            format!("  {}", t.any_key_closes),
            Style::default().fg(FAINT),
        )),
    ];

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT_DIM))
                .title(Span::styled(
                    format!(" {} ", t.help_title),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
        ),
        popup,
    );
}
