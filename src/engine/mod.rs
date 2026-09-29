//! Orchestration: the end-to-end pipeline.
//!
//! ```text
//!  sync ──► analyse ──► prompt ──► Claude ──► resolve ──► select ──► publish
//!   │          │                     │          │           │          │
//!  SQLite   profile              structured   Spotify    filters    playlist
//!                                   JSON       search    + quotas
//! ```
//!
//! Every stage reports through an optional [`EventSink`], which is how the TUI
//! gets progress without the engine knowing a terminal exists. In headless
//! mode the sink is `None` and the same information goes to `tracing`.
//!
//! The pipeline is transactional in spirit: a run is recorded in `runs` before
//! any network call, and finished with a terminal status on every exit path,
//! including failure. A crashed run therefore leaves a `running` row that is
//! visible in `history`, rather than vanishing.

pub mod analyze;
pub mod feedback;
pub mod filter;
pub mod resolve;
pub mod rolling;

use crate::config::{Config, FillStrategy, LanguagePolicy, ResolvedPreset};
use crate::domain::{
    CurationResponse, Playlist, RejectReason, RejectedSuggestion, ResolvedSuggestion, TasteProfile,
    TimeRange, Track,
};
use crate::error::{AgentError, Result};
use crate::llm::chain::LlmChain;
use crate::llm::{CurationRequest, StreamEvent, prompt, schema};
use crate::spotify::{Authenticator, SpotifyClient};
use crate::storage::{RecommendationRecord, Storage};
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

// ===========================================================================
// Events
// ===========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Sync,
    Analyze,
    Prompt,
    Model,
    Resolve,
    Select,
    Publish,
    Done,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sync => "Syncing library",
            Self::Analyze => "Analysing taste",
            Self::Prompt => "Building prompt",
            Self::Model => "Consulting Claude",
            Self::Resolve => "Resolving tracks",
            Self::Select => "Applying filters",
            Self::Publish => "Writing playlist",
            Self::Done => "Done",
        }
    }

    /// Rough share of total wall-clock, for the progress bar.
    pub fn ordinal(self) -> usize {
        match self {
            Self::Sync => 0,
            Self::Analyze => 1,
            Self::Prompt => 2,
            Self::Model => 3,
            Self::Resolve => 4,
            Self::Select => 5,
            Self::Publish => 6,
            Self::Done => 7,
        }
    }

    pub const COUNT: usize = 8;
}

#[derive(Debug, Clone)]
pub enum EngineEvent {
    Stage(Stage),
    /// Within-stage progress.
    Progress {
        done: usize,
        total: usize,
    },
    Log(String),
    /// Streamed reasoning summary from the model.
    Thinking(String),
    Accepted(String),
    Rejected(String, String),
    Finished(Box<RunOutcome>),
    Failed(String),
}

pub type EventSink = Option<mpsc::UnboundedSender<EngineEvent>>;

fn emit(sink: &EventSink, event: EngineEvent) {
    if let Some(tx) = sink {
        // A closed receiver means the TUI exited; the pipeline continues so
        // the playlist still gets written.
        let _ = tx.send(event);
    }
}

// ===========================================================================
// Options & results
// ===========================================================================

#[derive(Debug, Clone, Default)]
pub struct GenerateOptions {
    pub preset: Option<String>,
    pub size: Option<usize>,
    pub playlist_name: Option<String>,
    pub strategy: Option<FillStrategy>,
    pub language: Option<LanguagePolicy>,
    pub extra_instructions: Option<String>,
    /// Resolve and filter, but write nothing to Spotify.
    pub dry_run: bool,
    /// Skip the freshness check entirely.
    pub skip_sync: bool,
    /// Extra one-run artist blocks, merged over the configured list.
    pub block_artists: Vec<String>,
    /// Extra one-run genre restriction.
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub saved: usize,
    pub top_tracks: usize,
    pub top_artists: usize,
    pub new_plays: usize,
    pub artists_hydrated: usize,
    /// New feedback signals derived this run.
    pub feedback_signals: usize,
    pub skipped: bool,
    pub duration: Duration,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub run_id: i64,
    pub preset: String,
    pub playlist: Option<Playlist>,
    pub playlist_title: String,
    pub summary: String,
    pub accepted: Vec<ResolvedSuggestion>,
    pub rejected: Vec<RejectedSuggestion>,
    pub requested: usize,
    pub suggested: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model: String,
    pub dry_run: bool,
    pub duration: Duration,
}

// ===========================================================================
// Engine
// ===========================================================================

pub struct Engine {
    pub config: Arc<Config>,
    pub storage: Storage,
    pub spotify: Arc<SpotifyClient>,
    /// Ordered LLM backends: primary first, then the configured fallbacks.
    pub llm: Arc<LlmChain>,
    pub auth: Arc<Authenticator>,
}

impl Engine {
    /// Wire everything up. Fails early and specifically on missing credentials
    /// rather than at the first request.
    pub async fn build(config: Arc<Config>) -> Result<Self> {
        let client_id = config.require_client_id()?.to_string();

        let http = reqwest::Client::builder()
            .user_agent(concat!("spotify-agent/", env!("CARGO_PKG_VERSION")))
            .timeout(config.spotify.timeout().max(config.claude.timeout()))
            // Connection reuse matters: resolution issues dozens of searches.
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;

        let data_dir = config.data_dir()?;
        crate::config::paths::ensure_dir(&data_dir)?;

        let auth = Authenticator::new(
            config.spotify.clone(),
            client_id,
            http.clone(),
            config.token_path()?,
        )
        .await?;

        let spotify = Arc::new(SpotifyClient::new(
            &config.spotify,
            http.clone(),
            Arc::clone(&auth),
        ));
        let llm = Arc::new(LlmChain::build(&config, http)?);
        let storage = Storage::open(&config.database_path()?).await?;

        Ok(Self {
            config,
            storage,
            spotify,
            llm,
            auth,
        })
    }

    /// Cheaper constructor for commands that never touch Anthropic
    /// (`login`, `sync`, `profile`, `cache`). Avoids demanding an API key for
    /// operations that do not need one.
    pub async fn build_spotify_only(config: Arc<Config>) -> Result<SpotifyOnly> {
        let client_id = config.require_client_id()?.to_string();
        let http = reqwest::Client::builder()
            .user_agent(concat!("spotify-agent/", env!("CARGO_PKG_VERSION")))
            .timeout(config.spotify.timeout())
            .build()?;

        crate::config::paths::ensure_dir(&config.data_dir()?)?;
        let auth = Authenticator::new(
            config.spotify.clone(),
            client_id,
            http.clone(),
            config.token_path()?,
        )
        .await?;
        let spotify = Arc::new(SpotifyClient::new(&config.spotify, http, Arc::clone(&auth)));
        let storage = Storage::open(&config.database_path()?).await?;
        Ok(SpotifyOnly {
            config,
            storage,
            spotify,
            auth,
        })
    }

    // -----------------------------------------------------------------
    // Sync
    // -----------------------------------------------------------------

    pub async fn sync(&self, force: bool, sink: &EventSink) -> Result<SyncReport> {
        sync_library(&self.storage, &self.spotify, &self.config, force, sink).await
    }

    pub async fn profile(&self) -> Result<TasteProfile> {
        build_profile(&self.storage).await
    }

    // -----------------------------------------------------------------
    // Generate
    // -----------------------------------------------------------------

    pub async fn generate(&self, opts: GenerateOptions, sink: EventSink) -> Result<RunOutcome> {
        let started = Instant::now();
        let preset = self.resolve_preset(&opts)?;
        let requested = preset.run.size;

        let run_id = self
            .storage
            .start_run(
                preset.name.clone(),
                requested as u32,
                self.llm.primary_label(),
            )
            .await?;

        match self
            .generate_inner(&opts, &preset, run_id, started, &sink)
            .await
        {
            Ok(outcome) => {
                self.storage
                    .finish_run(
                        run_id,
                        if outcome.dry_run { "dry-run" } else { "ok" },
                        outcome.suggested as u32,
                        outcome.accepted.len() as u32,
                        if outcome.dry_run {
                            0
                        } else {
                            outcome.accepted.len() as u32
                        },
                        outcome.playlist.as_ref().map(|p| p.id.clone()),
                        None,
                        outcome.input_tokens,
                        outcome.output_tokens,
                    )
                    .await?;
                emit(&sink, EngineEvent::Stage(Stage::Done));
                emit(&sink, EngineEvent::Finished(Box::new(outcome.clone())));
                Ok(outcome)
            }
            Err(e) => {
                // Record the failure before propagating, so `history` explains
                // what happened without the user re-reading the log.
                let _ = self
                    .storage
                    .finish_run(run_id, "failed", 0, 0, 0, None, Some(e.to_string()), 0, 0)
                    .await;
                emit(&sink, EngineEvent::Failed(e.to_string()));
                Err(e)
            }
        }
    }

    fn resolve_preset(&self, opts: &GenerateOptions) -> Result<ResolvedPreset> {
        let name = opts
            .preset
            .clone()
            .unwrap_or_else(|| self.config.defaults.preset.clone());
        let mut preset = self.config.resolve_preset(&name)?;

        // CLI flags are the last layer and win over both preset and defaults.
        if let Some(size) = opts.size {
            preset.run.size = size.clamp(1, 500);
        }
        if let Some(strategy) = opts.strategy {
            preset.run.strategy = strategy;
        }
        if let Some(language) = opts.language {
            preset.run.language = language;
        }
        if let Some(name) = &opts.playlist_name {
            preset.run.playlist_name = name.clone();
        }
        if !opts.genres.is_empty() {
            preset.filters.genres_include = opts.genres.clone();
        }
        for artist in &opts.block_artists {
            if !preset
                .filters
                .artists_block
                .iter()
                .any(|a| a.eq_ignore_ascii_case(artist))
            {
                preset.filters.artists_block.push(artist.clone());
            }
        }
        Ok(preset)
    }

    async fn generate_inner(
        &self,
        opts: &GenerateOptions,
        preset: &ResolvedPreset,
        run_id: i64,
        started: Instant,
        sink: &EventSink,
    ) -> Result<RunOutcome> {
        // ---- 1. sync ------------------------------------------------
        emit(sink, EngineEvent::Stage(Stage::Sync));
        if opts.skip_sync {
            emit(sink, EngineEvent::Log("sync skipped (--no-sync)".into()));
        } else {
            let report = self.sync(false, sink).await?;
            if report.skipped {
                emit(
                    sink,
                    EngineEvent::Log("library already fresh; sync skipped".into()),
                );
            } else {
                emit(
                    sink,
                    EngineEvent::Log(format!(
                        "synced {} saved, {} plays, {} artists",
                        report.saved, report.new_plays, report.artists_hydrated
                    )),
                );
            }
        }

        // ---- 2. analyse --------------------------------------------
        emit(sink, EngineEvent::Stage(Stage::Analyze));
        let profile = self.profile().await?;
        if profile.is_empty() {
            return Err(AgentError::other(
                "the local library cache is empty — run `spotify-agent sync` first",
            ));
        }
        emit(
            sink,
            EngineEvent::Log(format!(
                "{} tracks, {} artists, {} genres in profile",
                profile.known_tracks,
                profile.top_artists.len(),
                profile.genres.len()
            )),
        );

        // ---- 3. prompt ---------------------------------------------
        emit(sink, EngineEvent::Stage(Stage::Prompt));
        let run = &preset.run;
        let request_count =
            ((run.size as f32 * run.oversample).ceil() as usize).clamp(run.size, 200);

        let exclusions = self
            .storage
            .exclusion_lines(
                run.exclude_recent_days.max(1),
                run.exclude_saved,
                run.prompt_exclusion_limit,
            )
            .await?;
        let excluded_ids = self
            .storage
            .excluded_track_ids(
                run.exclude_recent_days,
                run.exclude_saved,
                run.exclude_played,
            )
            .await?;
        let mut artist_cooldown = self
            .storage
            .artists_on_cooldown(run.artist_cooldown_days)
            .await?;
        // A ban is permanent, so it is folded into the same check rather than
        // relying on the cooldown window.
        artist_cooldown.extend(self.storage.ban_keys().await?);

        // Artists on cooldown are also named in the prompt. Filtering them out
        // after the fact works, but it wastes the model's slots — telling it
        // up front is what actually breaks the loop.
        let mut blocked_artists = preset.filters.artists_block.clone();
        for banned in self.storage.banned_artists().await? {
            if !blocked_artists
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&banned.name))
            {
                blocked_artists.push(banned.name);
            }
        }
        let cooling = self
            .storage
            .recent_recommended_artists(run.artist_cooldown_days, 120)
            .await?;
        for artist in &cooling {
            if !blocked_artists
                .iter()
                .any(|a| a.eq_ignore_ascii_case(artist))
            {
                blocked_artists.push(artist.clone());
            }
        }

        // ---- feedback verdicts ----
        let scores = self
            .storage
            .feedback_scores(self.config.feedback.half_life_days)
            .await?;
        let artist_names: HashMap<String, String> = self
            .storage
            .artists_map()
            .await?
            .into_iter()
            .flat_map(|(id, artist)| {
                // Indexed by id and by normalised name, matching how the
                // scores themselves are keyed.
                let norm = crate::util::text::normalize(&artist.name);
                [(id, artist.name.clone()), (norm, artist.name)]
            })
            .collect();
        let (resonated, rejected_artists) = if self.config.feedback.apply_to_prompt {
            feedback::artist_verdicts(&self.config.feedback, &scores.by_artist, &artist_names, 25)
        } else {
            (Vec::new(), Vec::new())
        };
        if !resonated.is_empty() || !rejected_artists.is_empty() {
            emit(
                sink,
                EngineEvent::Log(format!(
                    "feedback: {} artists resonated, {} to avoid",
                    resonated.len(),
                    rejected_artists.len()
                )),
            );
        }

        // Artists the feedback loop says to avoid are named in the prompt too,
        // so the model does not spend slots on picks that would be filtered
        // out downstream anyway.
        for artist in &rejected_artists {
            if !blocked_artists
                .iter()
                .any(|a| a.eq_ignore_ascii_case(artist))
            {
                blocked_artists.push(artist.clone());
            }
        }

        let input = prompt::PromptInput {
            preset,
            profile: &profile,
            resonated: &resonated,
            rejected: &rejected_artists,
            request_count,
            exclusions: &exclusions,
            blocked_artists: &blocked_artists,
            extra_instructions: opts.extra_instructions.as_deref(),
            market: self.config.spotify.market.as_deref(),
        };
        let user_message = prompt::user_message(&input);
        let system_volatile = prompt::system_volatile(preset);
        emit(
            sink,
            EngineEvent::Log(format!(
                "prompt ~{} chars, {} exclusions, {} artists on cooldown, asking for {request_count} tracks",
                user_message.len(),
                exclusions.len(),
                cooling.len()
            )),
        );

        // ---- 4. model ----------------------------------------------
        emit(sink, EngineEvent::Stage(Stage::Model));
        // Counts the streamed JSON so a truncated response is visible in the
        // log rather than only as a downstream parse error.
        let streamed_bytes = Arc::new(AtomicU64::new(0));
        let listener = sink.as_ref().map(|tx| {
            let tx = tx.clone();
            let streamed_bytes = Arc::clone(&streamed_bytes);
            Arc::new(move |event: StreamEvent| match event {
                StreamEvent::Thinking(text) => {
                    let _ = tx.send(EngineEvent::Thinking(text));
                }
                StreamEvent::Text(chunk) => {
                    streamed_bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
                StreamEvent::FellBackTo(model) => {
                    let _ = tx.send(EngineEvent::Log(format!("fell back to {model}")));
                }
            }) as crate::llm::Listener
        });

        let curation = CurationRequest {
            system_stable: prompt::SYSTEM_STABLE,
            system_volatile: &system_volatile,
            user: &user_message,
            schema: schema::curation_schema(),
        };

        let outcome = self
            .llm
            .curate(&curation, listener, |backend, reason| {
                emit(
                    sink,
                    EngineEvent::Log(format!("falling back to {backend} ({reason})")),
                );
            })
            .await?;
        let completion = outcome.completion;

        let response: CurationResponse = serde_json::from_value(outcome.value).map_err(|e| {
            AgentError::ModelProtocol(format!("curation response did not match the schema: {e}"))
        })?;
        let suggested = response.tracks.len();
        emit(
            sink,
            EngineEvent::Log(format!(
                "{} returned {suggested} suggestions ({:.1} KB, {} in / {} out tokens)",
                outcome.backend,
                streamed_bytes.load(Ordering::Relaxed) as f32 / 1024.0,
                completion.input_tokens,
                completion.output_tokens
            )),
        );
        let served_by = outcome.backend;

        // ---- 5. resolve --------------------------------------------
        emit(sink, EngineEvent::Stage(Stage::Resolve));
        emit(
            sink,
            EngineEvent::Progress {
                done: 0,
                total: suggested,
            },
        );
        let resolution = resolve::resolve_all(&self.spotify, response.tracks, |done, total| {
            emit(sink, EngineEvent::Progress { done, total });
        })
        .await;
        emit(
            sink,
            EngineEvent::Log(format!(
                "resolved {}/{suggested} on Spotify",
                resolution.resolved.len()
            )),
        );

        // ---- 6. select ---------------------------------------------
        emit(sink, EngineEvent::Stage(Stage::Select));

        // In append mode, everything already in the target playlist is an
        // exclusion — otherwise a repeat run silently duplicates tracks.
        let mut existing_playlist: Option<Playlist> = None;
        let mut existing_ids: HashSet<String> = HashSet::new();
        let playlist_name =
            render_template(&run.playlist_name, &preset.name, &response.playlist_title);

        if !opts.dry_run || run.strategy == FillStrategy::Append {
            let me = self.spotify.current_user().await?;
            if let Some(found) = self.spotify.find_playlist(&me.id, &playlist_name).await? {
                if run.strategy == FillStrategy::Append {
                    existing_ids = self
                        .spotify
                        .playlist_tracks(&found.id)
                        .await?
                        .into_iter()
                        .map(|t| t.id)
                        .collect();
                }
                existing_playlist = Some(found);
            }
        }

        let artists_map = self.storage.artists_map().await?;
        let selection = select(
            resolution.resolved,
            resolution.rejected,
            &SelectionContext {
                size: run.size,
                max_per_artist: run.max_per_artist,
                excluded_ids: &excluded_ids,
                existing_ids: &existing_ids,
                artist_cooldown: &artist_cooldown,
                filters: &filter::FilterContext {
                    filters: &preset.filters,
                    language: run.language,
                    artists: &artists_map,
                },
            },
        );

        for item in &selection.accepted {
            emit(sink, EngineEvent::Accepted(item.track.display()));
        }
        for item in &selection.rejected {
            emit(
                sink,
                EngineEvent::Rejected(item.suggestion.display(), item.reason.label()),
            );
        }

        // ---- 7. publish ---------------------------------------------
        emit(sink, EngineEvent::Stage(Stage::Publish));
        let playlist = if opts.dry_run {
            emit(
                sink,
                EngineEvent::Log("dry run: nothing written to Spotify".into()),
            );
            existing_playlist
        } else if selection.accepted.is_empty() {
            // Writing an empty playlist would destroy the previous one under
            // the replace strategy. Refusing is the safe behaviour.
            emit(
                sink,
                EngineEvent::Log(
                    "no tracks survived filtering; leaving the playlist untouched".into(),
                ),
            );
            existing_playlist
        } else {
            Some(
                self.publish(
                    preset,
                    &playlist_name,
                    &response.summary,
                    existing_playlist,
                    &selection.accepted,
                    sink,
                )
                .await?,
            )
        };

        // ---- 8. record ----------------------------------------------
        let mut records: Vec<RecommendationRecord> = Vec::with_capacity(suggested);
        for item in &selection.accepted {
            records.push(RecommendationRecord {
                track_id: Some(item.track.id.clone()),
                title: item.suggestion.title.clone(),
                artist: item.suggestion.artist.clone(),
                reason: item.suggestion.reason.clone(),
                mood: item.suggestion.mood.clone(),
                accepted: true,
                reject_note: None,
            });
        }
        for item in &selection.rejected {
            records.push(RecommendationRecord {
                track_id: None,
                title: item.suggestion.title.clone(),
                artist: item.suggestion.artist.clone(),
                reason: item.suggestion.reason.clone(),
                mood: item.suggestion.mood.clone(),
                accepted: false,
                reject_note: Some(item.reason.label()),
            });
        }
        self.storage
            .record_recommendations(
                run_id,
                preset.name.clone(),
                playlist.as_ref().map(|p| p.id.clone()),
                records,
            )
            .await?;

        Ok(RunOutcome {
            run_id,
            preset: preset.name.clone(),
            playlist,
            playlist_title: playlist_name,
            summary: response.summary,
            accepted: selection.accepted,
            rejected: selection.rejected,
            requested: run.size,
            suggested,
            input_tokens: completion.input_tokens,
            output_tokens: completion.output_tokens,
            // The model that actually served the turn, which differs from the
            // configured one when a refusal fallback fired.
            // The backend that actually served the turn, which differs from
            // the configured primary when the chain fell forward.
            model: if completion.model.is_empty() {
                served_by
            } else {
                format!("{}/{}", completion.provider.as_str(), completion.model)
            },
            dry_run: opts.dry_run,
            duration: started.elapsed(),
        })
    }

    async fn publish(
        &self,
        preset: &ResolvedPreset,
        playlist_name: &str,
        summary: &str,
        existing: Option<Playlist>,
        accepted: &[ResolvedSuggestion],
        sink: &EventSink,
    ) -> Result<Playlist> {
        let run = &preset.run;
        let uris: Vec<String> = accepted.iter().map(|item| item.track.uri()).collect();

        let description = build_description(&run.playlist_description, &preset.name, summary);

        let playlist = match existing {
            Some(playlist) => {
                emit(
                    sink,
                    EngineEvent::Log(format!("updating playlist {}", playlist.name)),
                );
                self.spotify
                    .update_playlist_details(&playlist.id, None, Some(&description))
                    .await?;
                playlist
            }
            None => {
                let me = self.spotify.current_user().await?;
                emit(
                    sink,
                    EngineEvent::Log(format!("creating playlist {playlist_name}")),
                );
                self.spotify
                    .create_playlist(&me.id, playlist_name, &description, run.playlist_public)
                    .await?
            }
        };

        // ---- snapshot before anything destructive ----
        // Spotify offers no undo. Every overwrite is preceded by a local copy
        // so `snapshots restore` can always put the old contents back.
        let existing_tracks = if matches!(
            run.strategy,
            FillStrategy::Replace | FillStrategy::Rolling
        ) {
            let current = self
                .spotify
                .playlist_tracks(&playlist.id)
                .await
                .unwrap_or_default();
            if !current.is_empty() {
                let reason = if run.strategy == FillStrategy::Rolling {
                    "pre-rolling"
                } else {
                    "pre-write"
                };
                match self
                    .storage
                    .snapshot_playlist(
                        playlist.id.clone(),
                        playlist.name.clone(),
                        Some(playlist.snapshot_id.clone()),
                        reason,
                        current.clone(),
                    )
                    .await
                {
                    Ok(id) => emit(
                        sink,
                        EngineEvent::Log(format!(
                            "snapshot #{id} saved ({} tracks) before overwriting",
                            current.len()
                        )),
                    ),
                    Err(e) => {
                        // A failed snapshot must not silently precede a
                        // destructive write — that is the one case where
                        // continuing loses data irrecoverably.
                        return Err(AgentError::other(format!(
                            "refusing to overwrite the playlist: could not snapshot it first ({e})"
                        )));
                    }
                }
            }
            current
        } else {
            Vec::new()
        };

        let written = match run.strategy {
            FillStrategy::Replace => {
                self.spotify
                    .replace_playlist_tracks(&playlist.id, &uris)
                    .await?;
                accepted
                    .iter()
                    .map(|item| item.track.id.clone())
                    .collect::<Vec<_>>()
            }
            FillStrategy::Append => {
                self.spotify
                    .add_playlist_tracks(&playlist.id, &uris)
                    .await?;
                let mut ids: Vec<String> = existing_tracks.iter().map(|t| t.id.clone()).collect();
                ids.extend(accepted.iter().map(|item| item.track.id.clone()));
                ids
            }
            FillStrategy::Rolling => {
                let plan = self
                    .plan_rolling(&playlist.id, existing_tracks, accepted, run)
                    .await?;
                emit(
                    sink,
                    EngineEvent::Log(format!(
                        "rolling: +{} new, -{} evicted, {} held back, {} total",
                        plan.added,
                        plan.evicted.len(),
                        plan.deferred,
                        plan.final_ids.len()
                    )),
                );
                for gone in &plan.evicted {
                    emit(
                        sink,
                        EngineEvent::Rejected(gone.display.clone(), "evicted".into()),
                    );
                }
                let uris: Vec<String> = plan
                    .final_ids
                    .iter()
                    .map(|id| format!("spotify:track:{id}"))
                    .collect();
                self.spotify
                    .replace_playlist_tracks(&playlist.id, &uris)
                    .await?;
                plan.final_ids
            }
        };

        // Remember membership so the next run can age tracks for eviction and
        // notice anything the listener removed by hand.
        self.storage
            .record_members(playlist.id.clone(), written.clone())
            .await?;

        emit(
            sink,
            EngineEvent::Log(format!(
                "playlist now holds {} tracks — {}",
                written.len(),
                playlist.web_url()
            )),
        );
        Ok(playlist)
    }

    /// Build the rolling plan, pulling engagement data from the local cache.
    async fn plan_rolling(
        &self,
        playlist_id: &str,
        existing: Vec<Track>,
        accepted: &[ResolvedSuggestion],
        run: &crate::config::RunDefaults,
    ) -> Result<rolling::RollingPlan> {
        let scores = self
            .storage
            .feedback_scores(self.config.feedback.half_life_days)
            .await?;
        let members: HashMap<String, i64> = self
            .storage
            .members(playlist_id.to_string())
            .await?
            .into_iter()
            .map(|m| (m.track_id, m.added_at.timestamp_millis()))
            .collect();
        let play_counts: HashMap<String, u32> = self
            .storage
            .track_stats()
            .await?
            .into_iter()
            .map(|s| (s.track.id, s.plays))
            .collect();

        let now_ms = Utc::now().timestamp_millis();
        let incumbents: Vec<rolling::Incumbent> = existing
            .into_iter()
            .map(|track| rolling::Incumbent {
                // A track Spotify reports but we have no membership row for
                // was added by the user; treat it as newly arrived so the
                // grace period protects it.
                added_at_ms: members.get(&track.id).copied().unwrap_or(now_ms),
                feedback: scores.by_track.get(&track.id).copied().unwrap_or(0.0),
                plays: play_counts.get(&track.id).copied().unwrap_or(0),
                display: track.display(),
                track_id: track.id,
            })
            .collect();

        let additions: Vec<String> = accepted.iter().map(|item| item.track.id.clone()).collect();

        Ok(rolling::plan(
            incumbents,
            additions,
            rolling::RollingOptions {
                size: run.size,
                max_evictions: run.rolling_max_evictions,
                grace_days: run.rolling_grace_days,
                now_ms,
            },
        ))
    }
}

/// Handle for the Spotify-only subset of commands.
pub struct SpotifyOnly {
    pub config: Arc<Config>,
    pub storage: Storage,
    pub spotify: Arc<SpotifyClient>,
    pub auth: Arc<Authenticator>,
}

impl SpotifyOnly {
    pub async fn sync(&self, force: bool, sink: &EventSink) -> Result<SyncReport> {
        sync_library(&self.storage, &self.spotify, &self.config, force, sink).await
    }

    pub async fn profile(&self) -> Result<TasteProfile> {
        build_profile(&self.storage).await
    }
}

// ===========================================================================
// Shared stage implementations
// ===========================================================================

async fn build_profile(storage: &Storage) -> Result<TasteProfile> {
    let stats = storage.track_stats().await?;
    let artists = storage.artists_map().await?;
    let top_artists = storage.top_artist_scores().await?;
    Ok(analyze::build_profile(&stats, &artists, &top_artists))
}

async fn sync_library(
    storage: &Storage,
    spotify: &SpotifyClient,
    config: &Config,
    force: bool,
    sink: &EventSink,
) -> Result<SyncReport> {
    let started = Instant::now();

    if !force && let Some(last) = storage.last_sync().await? {
        let age = Utc::now().signed_duration_since(last);
        let min = chrono::Duration::minutes(config.storage.sync_min_interval_mins as i64);
        if age < min {
            return Ok(SyncReport {
                skipped: true,
                duration: started.elapsed(),
                ..Default::default()
            });
        }
    }

    let mut report = SyncReport::default();

    // ---- Liked Songs ----
    emit(sink, EngineEvent::Log("fetching saved tracks…".into()));
    let saved = spotify.saved_tracks(None).await?;
    report.saved = saved.len();
    let saved_ids: Vec<String> = saved.iter().map(|t| t.id.clone()).collect();
    let saved_id_set: HashSet<String> = saved_ids.iter().cloned().collect();
    storage.upsert_tracks(saved).await?;
    storage.replace_saved(saved_ids).await?;

    // ---- top tracks & artists, all three windows ----
    for range in TimeRange::ALL {
        emit(
            sink,
            EngineEvent::Log(format!("fetching top tracks ({})…", range.label())),
        );
        let tracks = spotify.top_tracks(range).await?;
        report.top_tracks += tracks.len();
        let ids: Vec<String> = tracks.iter().map(|t| t.id.clone()).collect();
        storage.upsert_tracks(tracks).await?;
        storage.replace_top("track", range, ids).await?;

        let artists = spotify.top_artists(range).await?;
        report.top_artists += artists.len();
        let ids: Vec<String> = artists.iter().map(|a| a.id.clone()).collect();
        storage.upsert_artists(artists).await?;
        storage.replace_top("artist", range, ids).await?;
    }

    // ---- recent plays (incremental) ----
    emit(sink, EngineEvent::Log("fetching recent plays…".into()));
    let after = storage.newest_play_ms().await?;
    let (tracks, events) = spotify.recently_played(after).await?;
    storage.upsert_tracks(tracks).await?;
    report.new_plays = storage.insert_plays(events).await?;

    // ---- hydrate artist genres ----
    // Genres live on the artist object, never the track, so a second pass is
    // unavoidable. Only artists we have never resolved are fetched.
    let missing = storage.artists_missing_genres().await?;
    if !missing.is_empty() {
        emit(
            sink,
            EngineEvent::Log(format!(
                "hydrating {} artists for genre data…",
                missing.len()
            )),
        );
        let hydrated = spotify.artists(&missing).await?;
        report.artists_hydrated = storage.upsert_artists(hydrated).await?;
    }

    // ---- feedback ----
    // Runs last: it reads the library and play history this sync just
    // refreshed, so the signals reflect the newest state.
    report.feedback_signals =
        derive_feedback(storage, spotify, config, &saved_id_set, sink).await?;

    storage.mark_synced().await?;
    report.duration = started.elapsed();
    Ok(report)
}

/// Observe how past recommendations fared and record the signals.
///
/// Best-effort by design: a failure to read one playlist must not fail the
/// whole sync, because the library data is the part that matters.
async fn derive_feedback(
    storage: &Storage,
    spotify: &SpotifyClient,
    config: &Config,
    saved: &HashSet<String>,
    sink: &EventSink,
) -> Result<usize> {
    if !config.feedback.enabled {
        return Ok(0);
    }

    let recommended: HashMap<String, i64> = storage
        .accepted_recommendations()
        .await?
        .into_iter()
        .collect();
    if recommended.is_empty() {
        return Ok(0);
    }

    let earliest = recommended.values().copied().min().unwrap_or(0);
    let plays = storage.play_windows(earliest).await?;
    let ever_played = storage.played_track_ids().await?;

    let mut diffs = Vec::new();
    for playlist_id in storage.managed_playlists().await? {
        let recorded: Vec<(String, i64)> = storage
            .members(playlist_id.clone())
            .await?
            .into_iter()
            .map(|m| (m.track_id, m.added_at.timestamp_millis()))
            .collect();
        if recorded.is_empty() {
            continue;
        }
        match spotify.playlist_tracks(&playlist_id).await {
            Ok(tracks) => diffs.push(feedback::PlaylistDiff {
                playlist_id,
                recorded,
                present: tracks.into_iter().map(|t| t.id).collect(),
            }),
            Err(e) => {
                // A deleted playlist 404s here. Treating that as "everything
                // was removed" would punish every track in it, so the
                // playlist is skipped instead.
                tracing::warn!(playlist = %playlist_id, error = %e, "skipping playlist during feedback reconciliation");
            }
        }
    }

    let entries = feedback::derive(
        &config.feedback,
        &feedback::Observations {
            recommended: &recommended,
            saved,
            plays: &plays,
            playlists: &diffs,
            ever_played: &ever_played,
            now_ms: Utc::now().timestamp_millis(),
        },
    );

    let recorded = storage.record_feedback(entries).await?;
    if recorded > 0 {
        emit(
            sink,
            EngineEvent::Log(format!("recorded {recorded} new feedback signals")),
        );
    }
    Ok(recorded)
}

// ===========================================================================
// Selection
// ===========================================================================

struct SelectionContext<'a> {
    size: usize,
    max_per_artist: usize,
    excluded_ids: &'a HashSet<String>,
    existing_ids: &'a HashSet<String>,
    /// Artist ids and normalised names used too recently to reuse.
    artist_cooldown: &'a HashSet<String>,
    filters: &'a filter::FilterContext<'a>,
}

#[derive(Default)]
struct Selection {
    accepted: Vec<ResolvedSuggestion>,
    rejected: Vec<RejectedSuggestion>,
}

/// Apply exclusions, filters and quotas in the model's preferred order.
///
/// Order is preserved deliberately: the model was asked to sequence the
/// playlist, and reordering here would discard that work.
fn select(
    resolved: Vec<ResolvedSuggestion>,
    already_rejected: Vec<RejectedSuggestion>,
    ctx: &SelectionContext<'_>,
) -> Selection {
    let mut out = Selection {
        accepted: Vec::with_capacity(ctx.size),
        rejected: already_rejected,
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut per_artist: HashMap<String, usize> = HashMap::new();

    for item in resolved {
        let reason = classify(&item, ctx, &seen, &per_artist, out.accepted.len());
        match reason {
            Some(reason) => out.rejected.push(RejectedSuggestion {
                suggestion: item.suggestion,
                reason,
            }),
            None => {
                seen.insert(item.track.id.clone());
                let key = artist_key(&item.track);
                *per_artist.entry(key).or_insert(0) += 1;
                out.accepted.push(item);
            }
        }
    }

    out
}

fn classify(
    item: &ResolvedSuggestion,
    ctx: &SelectionContext<'_>,
    seen: &HashSet<String>,
    per_artist: &HashMap<String, usize>,
    accepted_so_far: usize,
) -> Option<RejectReason> {
    if accepted_so_far >= ctx.size {
        return Some(RejectReason::Overflow);
    }
    if seen.contains(&item.track.id) {
        return Some(RejectReason::Duplicate);
    }
    if ctx.existing_ids.contains(&item.track.id) {
        return Some(RejectReason::Excluded("already in the playlist"));
    }
    if ctx.excluded_ids.contains(&item.track.id) {
        return Some(RejectReason::Excluded(
            "already known or recently recommended",
        ));
    }
    if !ctx.artist_cooldown.is_empty() && on_cooldown(&item.track, ctx.artist_cooldown) {
        return Some(RejectReason::Excluded("artist used recently"));
    }
    if let Some(reason) = ctx.filters.reject(&item.track) {
        return Some(reason);
    }
    if ctx.max_per_artist > 0 {
        let key = artist_key(&item.track);
        if per_artist.get(&key).copied().unwrap_or(0) >= ctx.max_per_artist {
            return Some(RejectReason::ArtistQuota);
        }
    }
    None
}

/// True when any credited artist is inside the cooldown window.
///
/// Checked by id *and* by normalised name: a track resolved before its artists
/// were hydrated has no id to match on.
fn on_cooldown(track: &Track, cooldown: &HashSet<String>) -> bool {
    track.artists.iter().any(|artist| {
        (!artist.id.is_empty() && cooldown.contains(&artist.id))
            || cooldown.contains(&crate::util::text::normalize(&artist.name))
    })
}

/// Quota key. Uses the primary artist id when available and falls back to the
/// normalised name, so an unhydrated artist still counts against the quota.
fn artist_key(track: &Track) -> String {
    track
        .artists
        .first()
        .map(|a| {
            if a.id.is_empty() {
                crate::util::text::normalize(track.primary_artist())
            } else {
                a.id.clone()
            }
        })
        .unwrap_or_default()
}

// ===========================================================================
// Templates
// ===========================================================================

fn render_template(template: &str, preset: &str, model_title: &Option<String>) -> String {
    let now = chrono::Local::now();
    let rendered = template
        .replace("{preset}", preset)
        .replace("{date}", &now.format("%Y-%m-%d").to_string())
        .replace("{datetime}", &now.format("%Y-%m-%d %H:%M").to_string())
        .replace("{title}", model_title.as_deref().unwrap_or(preset));
    let trimmed = rendered.trim();
    if trimmed.is_empty() {
        format!("AI · {preset}")
    } else {
        trimmed.to_string()
    }
}

fn build_description(template: &str, preset: &str, summary: &str) -> String {
    let base = render_template(template, preset, &None);
    if summary.trim().is_empty() {
        base
    } else {
        // Spotify caps descriptions at 300 characters; the client truncates,
        // but building a sensible order here means the summary is what
        // survives rather than the boilerplate.
        format!("{} — {}", summary.trim(), base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Filters;
    use crate::domain::{ArtistRef, Suggestion};

    fn resolved(id: &str, artist_id: &str, artist: &str) -> ResolvedSuggestion {
        ResolvedSuggestion {
            suggestion: Suggestion {
                title: format!("Song {id}"),
                artist: artist.into(),
                reason: String::new(),
                mood: String::new(),
                confidence: None,
                language: None,
            },
            track: Track {
                id: id.into(),
                name: format!("Song {id}"),
                artists: vec![ArtistRef {
                    id: artist_id.into(),
                    name: artist.into(),
                }],
                album: "A".into(),
                duration_ms: 200_000,
                popularity: 50,
                explicit: false,
                release_year: Some(2020),
                isrc: None,
            },
            match_score: 1.0,
        }
    }

    fn context<'a>(
        size: usize,
        max_per_artist: usize,
        excluded: &'a HashSet<String>,
        existing: &'a HashSet<String>,
        cooldown: &'a HashSet<String>,
        filters: &'a filter::FilterContext<'a>,
    ) -> SelectionContext<'a> {
        SelectionContext {
            size,
            max_per_artist,
            excluded_ids: excluded,
            existing_ids: existing,
            artist_cooldown: cooldown,
            filters,
        }
    }

    #[test]
    fn enforces_size_artist_quota_and_exclusions() {
        let artists = HashMap::new();
        let filters = Filters::default();
        let fctx = filter::FilterContext {
            filters: &filters,
            language: LanguagePolicy::Any,
            artists: &artists,
        };
        let mut excluded = HashSet::new();
        excluded.insert("t3".to_string());
        let existing = HashSet::new();
        let cooldown = HashSet::new();
        let ctx = context(3, 2, &excluded, &existing, &cooldown, &fctx);

        let input = vec![
            resolved("t1", "a1", "One"),
            resolved("t2", "a1", "One"),
            resolved("t5", "a1", "One"), // 3rd by a1 -> quota
            resolved("t3", "a2", "Two"), // excluded
            resolved("t4", "a3", "Three"),
            resolved("t6", "a4", "Four"), // beyond size 3
        ];
        let out = select(input, Vec::new(), &ctx);

        let ids: Vec<&str> = out.accepted.iter().map(|r| r.track.id.as_str()).collect();
        assert_eq!(ids, vec!["t1", "t2", "t4"]);
        assert!(
            out.rejected
                .iter()
                .any(|r| r.reason == RejectReason::ArtistQuota)
        );
        assert!(
            out.rejected
                .iter()
                .any(|r| matches!(r.reason, RejectReason::Excluded(_)))
        );
        assert!(
            out.rejected
                .iter()
                .any(|r| r.reason == RejectReason::Overflow)
        );
    }

    #[test]
    fn duplicate_suggestions_are_dropped_once() {
        let artists = HashMap::new();
        let filters = Filters::default();
        let fctx = filter::FilterContext {
            filters: &filters,
            language: LanguagePolicy::Any,
            artists: &artists,
        };
        let excluded = HashSet::new();
        let existing = HashSet::new();
        let cooldown = HashSet::new();
        let ctx = context(10, 0, &excluded, &existing, &cooldown, &fctx);

        let out = select(
            vec![resolved("t1", "a1", "One"), resolved("t1", "a1", "One")],
            Vec::new(),
            &ctx,
        );
        assert_eq!(out.accepted.len(), 1);
        assert_eq!(out.rejected.len(), 1);
        assert_eq!(out.rejected[0].reason, RejectReason::Duplicate);
    }

    #[test]
    fn artist_cooldown_blocks_by_id_and_by_name() {
        let artists = HashMap::new();
        let filters = Filters::default();
        let fctx = filter::FilterContext {
            filters: &filters,
            language: LanguagePolicy::Any,
            artists: &artists,
        };
        let excluded = HashSet::new();
        let existing = HashSet::new();
        let mut cooldown = HashSet::new();
        cooldown.insert("a1".to_string()); // by Spotify id
        cooldown.insert("two".to_string()); // by normalised name

        let ctx = context(10, 0, &excluded, &existing, &cooldown, &fctx);
        let out = select(
            vec![
                resolved("t1", "a1", "One"),
                resolved("t2", "unhydrated", "Two"),
                resolved("t3", "a3", "Three"),
            ],
            Vec::new(),
            &ctx,
        );

        let ids: Vec<&str> = out.accepted.iter().map(|r| r.track.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["t3"],
            "both cooled-down artists should be dropped"
        );
        assert_eq!(
            out.rejected
                .iter()
                .filter(|r| r.reason == RejectReason::Excluded("artist used recently"))
                .count(),
            2
        );
    }

    #[test]
    fn templates_substitute_and_never_render_empty() {
        let rendered = render_template("AI · {preset} · {date}", "focus", &None);
        assert!(rendered.starts_with("AI · focus · 2"));
        assert_eq!(render_template("   ", "focus", &None), "AI · focus");
        assert_eq!(
            render_template("{title}", "focus", &Some("Night Drive".into())),
            "Night Drive"
        );
    }
}
