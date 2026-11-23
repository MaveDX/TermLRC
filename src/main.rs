mod cli;
mod config;
mod kitty;
mod lrc;
mod matcher;
mod player;
mod providers;
mod ui;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use config::SettingsManager;
use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use kitty::KittyRenderer;
use lrc::{LyricSyncState, ParsedLrc};
use matcher::{find_sibling_lrc, LrcMatcher, SmartMediaType, TrackQuery};
use player::MprisTracker;
use providers::{FallbackEvent, FallbackWorker, LookupStatus};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};
use ui::UiState;

struct App {
    cli: Cli,
    settings_mgr: SettingsManager,
    matcher: LrcMatcher,
    tracker: MprisTracker,
    fallback_worker: FallbackWorker,
    ui: UiState,
    kitty: KittyRenderer,
    current_query: TrackQuery,
    current_track_length: Option<Duration>,
    current_lrc: Option<ParsedLrc>,
    current_lrc_path: Option<PathBuf>,
    current_lrc_mtime: Option<SystemTime>,
    last_local_hot_reload_check: Instant,
    /// Ensures TermLRC searches for lyrics only once per track play instead of repeatedly retrying.
    has_searched_current_play: bool,
    /// Tracks maximum playback position reached during the current play to detect track replay/restart.
    max_position_in_play: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalLoadOutcome {
    Loaded,
    FailedVerification,
    NotFound,
}

impl App {
    fn new(cli: Cli) -> Self {
        let settings_mgr = SettingsManager::new(cli.clone());
        let s = settings_mgr.settings();
        let lyrics_dir = s.resolved_lyrics_dir(&cli);
        let blacklist_dirs = s.resolved_blacklist_dirs();
        let matcher = LrcMatcher::with_blacklist(lyrics_dir, s.match_threshold, blacklist_dirs);
        let tracker = MprisTracker::new(s.player.clone(), s.mpris_poll_ms);
        let fallback_worker = FallbackWorker::new();
        let ui = UiState::from_settings(s);
        let kitty = KittyRenderer::new(&s.font_path, &s.font_family, &s.fallback_fonts);

        Self {
            cli,
            settings_mgr,
            matcher,
            tracker,
            fallback_worker,
            ui,
            kitty,
            current_query: TrackQuery::default(),
            current_track_length: None,
            current_lrc: None,
            current_lrc_path: None,
            current_lrc_mtime: None,
            last_local_hot_reload_check: Instant::now(),
            has_searched_current_play: false,
            max_position_in_play: Duration::ZERO,
        }
    }

    fn fps(&self) -> u32 {
        self.settings_mgr.settings().fps.clamp(15, 240)
    }

    fn set_local_lrc(&mut self, path: PathBuf, parsed: ParsedLrc) {
        self.current_lrc_mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
        self.current_lrc_path = Some(path);
        self.current_lrc = Some(parsed);
        self.ui.lookup_status = LookupStatus::Idle;
        self.has_searched_current_play = true;
        self.kitty.invalidate();
    }

    fn try_load_local_lrc(&mut self, allow_rescan: bool) -> LocalLoadOutcome {
        let verify_lyrics = self.settings_mgr.settings().verify_lyrics;
        let rescan_failed = self.settings_mgr.settings().rescan_failed_lyrics;
        let mut saw_failed_verification = false;

        // 1. Direct sibling `.lrc` file next to the local audio file
        if let Some(sibling_path) = find_sibling_lrc(&self.current_query) {
            if let Ok(mut parsed) = ParsedLrc::from_file(&sibling_path) {
                let identity_ok = parsed.matches_track_identity(&self.current_query);
                let is_valid = identity_ok
                    && if verify_lyrics {
                        parsed.verify_and_clean()
                    } else {
                        parsed.is_strictly_synced()
                    };
                if is_valid {
                    self.set_local_lrc(sibling_path, parsed);
                    return LocalLoadOutcome::Loaded;
                } else {
                    saw_failed_verification = true;
                    if rescan_failed {
                        let _ = fs::remove_file(&sibling_path);
                    }
                }
            }
        }

        // 2. Recursive `lyrics_dir` search
        let matched = if allow_rescan {
            self.matcher.find_best_match(&self.current_query)
        } else {
            self.matcher.find_best_match_no_rescan(&self.current_query)
        };
        if let Some(m) = matched {
            if let Ok(mut parsed) = ParsedLrc::from_file(&m.path) {
                let identity_ok = parsed.matches_track_identity(&self.current_query);
                let is_valid = identity_ok
                    && if verify_lyrics {
                        parsed.verify_and_clean()
                    } else {
                        parsed.is_strictly_synced()
                    };
                if is_valid {
                    self.set_local_lrc(m.path, parsed);
                    return LocalLoadOutcome::Loaded;
                } else {
                    saw_failed_verification = true;
                    if rescan_failed {
                        let _ = fs::remove_file(&m.path);
                        self.matcher.rescan();
                    }
                }
            }
        }

        if saw_failed_verification {
            LocalLoadOutcome::FailedVerification
        } else {
            LocalLoadOutcome::NotFound
        }
    }

    fn force_rescan_and_match(&mut self) {
        self.matcher.rescan();
        self.has_searched_current_play = false;
        self.match_current_track(true, true);
    }

    fn match_current_track(&mut self, preserve_existing_on_pending: bool, force_bypass_cache: bool) {
        self.kitty.invalidate();
        if self.current_query.is_empty() {
            self.current_lrc = None;
            self.current_lrc_path = None;
            self.current_lrc_mtime = None;
            self.ui.lookup_status = LookupStatus::Idle;
            self.has_searched_current_play = false;
            self.max_position_in_play = Duration::ZERO;
            return;
        }

        self.has_searched_current_play = true;

        // 1. Check if the playing track originates from a blacklisted directory
        if self.matcher.is_track_blacklisted(&self.current_query) {
            self.current_lrc = None;
            self.current_lrc_path = None;
            self.current_lrc_mtime = None;
            self.ui.lookup_status = LookupStatus::Disabled;
            return;
        }

        self.ui.lookup_status = LookupStatus::Searching;

        // 2. Always check for an existing valid local `.lrc` file first
        let local_outcome = self.try_load_local_lrc(true);
        if local_outcome == LocalLoadOutcome::Loaded {
            return;
        }

        let settings = self.settings_mgr.settings().clone();

        // 3. Smart Identification: if local lyrics are unavailable, check if the track's embedded
        //    file tags, MPRIS metadata, or directory/playlist name identify it as instrumental
        //    or non-vocal media (e.g. "Lo-Fi", "Podcast", "Instrumental", "Audiobook", "Ambient").
        if settings.smart_identification {
            if let Some(media_type) = self.matcher.smart_identify_track(&self.current_query) {
                self.current_lrc = None;
                self.current_lrc_path = None;
                self.current_lrc_mtime = None;
                self.ui.lookup_status = LookupStatus::SmartMedia(media_type);
                return;
            }
        }

        if local_outcome == LocalLoadOutcome::FailedVerification && !settings.rescan_failed_lyrics {
            self.current_lrc = None;
            self.current_lrc_path = None;
            self.current_lrc_mtime = None;
            self.ui.lookup_status = LookupStatus::NotFound;
            return;
        }

        // 4. Local match not found and media is a regular song:
        //    dispatch remote fallback chain ONCE for this play.
        if !preserve_existing_on_pending {
            self.current_lrc = None;
            self.current_lrc_path = None;
            self.current_lrc_mtime = None;
        }
        let bypass_cache =
            force_bypass_cache || local_outcome == LocalLoadOutcome::FailedVerification;
        if !settings.enabled_fallback_providers().is_empty() {
            self.ui.lookup_status = LookupStatus::Searching;
            self.fallback_worker.request(
                self.current_query.clone(),
                self.current_track_length,
                settings,
                bypass_cache,
            );
        } else {
            self.ui.lookup_status = LookupStatus::NotFound;
        }
    }

    /// Periodically check if a local `.lrc` file was newly downloaded or modified on disk
    /// so lyrics hot-reload immediately without needing to switch songs.
    /// Does not trigger network requests or full directory rescans.
    fn poll_local_lrc_hot_reload(&mut self) {
        if self.last_local_hot_reload_check.elapsed() < Duration::from_millis(750) {
            return;
        }
        self.last_local_hot_reload_check = Instant::now();

        if self.current_query.is_empty()
            || self.ui.lookup_status == LookupStatus::Disabled
            || self.matcher.is_track_blacklisted(&self.current_query)
        {
            return;
        }

        let verify_lyrics = self.settings_mgr.settings().verify_lyrics;

        // Case 1: Already displaying a local `.lrc` file — check if it was modified on disk
        if let Some(ref path) = self.current_lrc_path.clone() {
            if let Ok(mtime) = fs::metadata(path).and_then(|m| m.modified()) {
                if self.current_lrc_mtime != Some(mtime) {
                    if let Ok(mut parsed) = ParsedLrc::from_file(path) {
                        let valid = parsed.matches_track_identity(&self.current_query)
                            && if verify_lyrics {
                                parsed.verify_and_clean()
                            } else {
                                parsed.is_strictly_synced()
                            };
                        if valid {
                            self.set_local_lrc(path.clone(), parsed);
                        }
                    }
                }
            }
            return;
        }

        // Case 2: No local `.lrc` was loaded yet — check if a sibling `.lrc` or indexed `.lrc` appeared
        // without rescanning or re-triggering remote lookups
        if self.try_load_local_lrc(false) == LocalLoadOutcome::Loaded {
            self.kitty.invalidate();
        }
    }

    fn tick(&mut self, dt_secs: f64) -> (LyricSyncState, bool) {
        let mut mode_switched = false;
        if let Some(updated) = self.settings_mgr.poll_reload().cloned() {
            let prev_kitty = self.ui.kitty_graphics;
            self.ui.apply_settings(&updated);
            self.kitty.update_fonts_if_changed(
                &updated.font_path,
                &updated.font_family,
                &updated.fallback_fonts,
            );
            self.kitty.invalidate();
            if prev_kitty != self.ui.kitty_graphics {
                mode_switched = true;
            }
            self.tracker
                .update_config(updated.player.clone(), updated.mpris_poll_ms);
            let new_dir = updated.resolved_lyrics_dir(&self.cli);
            let new_blacklist = updated.resolved_blacklist_dirs();
            if self
                .matcher
                .update_config(new_dir, updated.match_threshold, new_blacklist)
            {
                self.has_searched_current_play = false;
                self.match_current_track(true, false);
            }
        }

        let snap = self.tracker.update().clone();
        if snap.query != self.current_query {
            let is_minor_update = self.current_query.is_same_track(
                &snap.query,
                self.current_track_length,
                snap.length,
            );
            self.current_query = snap.query.clone();
            self.current_track_length = snap.length;

            // Search only once per play: minor metadata/duration updates on the same playing track
            // never re-trigger a search after the track has already been searched once.
            if is_minor_update && self.has_searched_current_play {
                // Keep existing state (whether lyrics were found or the single search completed/failed)
            } else {
                self.ui.reset_for_new_track();
                self.has_searched_current_play = false;
                self.max_position_in_play = snap.position;
                self.match_current_track(false, false);
            }
        } else {
            self.current_track_length = snap.length;
            // Detect if the user replayed/restarted the same track from the beginning
            if snap.position > self.max_position_in_play {
                self.max_position_in_play = snap.position;
            } else if self.max_position_in_play > Duration::from_secs(15)
                && snap.position < Duration::from_secs(2)
            {
                self.max_position_in_play = snap.position;
                if self.current_lrc.is_none() {
                    self.has_searched_current_play = false;
                    self.match_current_track(false, false);
                }
            }
        }

        // Drain status and completion events from the background fallback worker
        while let Some(ev) = self.fallback_worker.poll_event() {
            match ev {
                FallbackEvent::Status { query, status } => {
                    if query.is_same_track(&self.current_query, None, None)
                        || query == self.current_query
                    {
                        if self.current_lrc.is_none()
                            && self.ui.lookup_status != LookupStatus::Disabled
                            && !matches!(self.ui.lookup_status, LookupStatus::SmartMedia(_))
                        {
                            self.ui.lookup_status = status;
                            self.kitty.invalidate();
                        }
                    }
                }
                FallbackEvent::Completed {
                    query,
                    lrc,
                    rate_limited,
                    detected_instrumental,
                } => {
                    if (query.is_same_track(&self.current_query, None, None)
                        || query == self.current_query)
                        && self.ui.lookup_status != LookupStatus::Disabled
                    {
                        if let Some(fetched_lrc) = lrc {
                            let s = self.settings_mgr.settings().clone();
                            let should_save = s.save_lrc_file || s.local_only_lyrics;
                            let saved_path = if should_save {
                                self.matcher.save_fetched_sidecar(
                                    &self.current_query,
                                    &fetched_lrc,
                                    s.save_lrc_threshold,
                                    s.local_only_lyrics,
                                    s.verify_lyrics,
                                )
                            } else {
                                None
                            };

                            if let Some(path) = saved_path {
                                // Instantly hot-reload the newly downloaded `.lrc` file from disk
                                let mut reloaded =
                                    ParsedLrc::from_file(&path).unwrap_or_else(|_| fetched_lrc.clone());
                                if reloaded.source_label.is_none() {
                                    reloaded.source_label = fetched_lrc.source_label.clone();
                                }
                                if s.verify_lyrics {
                                    let _ = reloaded.verify_and_clean();
                                }
                                self.set_local_lrc(path, reloaded);
                            } else if s.local_only_lyrics {
                                // `local_only_lyrics` is enabled, and the remote result could not be saved locally
                                if self.current_lrc.is_none() {
                                    self.ui.lookup_status = LookupStatus::NotFound;
                                    self.kitty.invalidate();
                                }
                            } else {
                                self.current_lrc_path = None;
                                self.current_lrc_mtime = None;
                                self.current_lrc = Some(fetched_lrc);
                                self.ui.lookup_status = LookupStatus::Idle;
                                self.kitty.invalidate();
                            }
                        } else if self.current_lrc.is_none() {
                            let smart_id = self.settings_mgr.settings().smart_identification;
                            self.ui.lookup_status = if smart_id && detected_instrumental {
                                LookupStatus::SmartMedia(SmartMediaType::Instrumental)
                            } else if rate_limited {
                                LookupStatus::RateLimited
                            } else {
                                LookupStatus::NotFound
                            };
                            self.kitty.invalidate();
                        }
                    }
                }
            }
        }

        // Check for newly downloaded or modified local `.lrc` files on disk
        self.poll_local_lrc_hot_reload();

        let sync = match &self.current_lrc {
            Some(lrc) => lrc.sync_state(snap.position, self.ui.user_offset_ms),
            None => LyricSyncState {
                active_index: None,
                line_progress: 0.0,
                continuous_index: 0.0,
                karaoke_progress: 0.0,
            },
        };

        let total_lines = self
            .current_lrc
            .as_ref()
            .map(|l| l.lines.len())
            .unwrap_or(0);
        self.ui.step_animation(&sync, total_lines, dt_secs);
        (sync, mode_switched)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = io::stdout().write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\");
        let _ = io::stdout().flush();
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen, DisableMouseCapture);
        default_panic(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let res = run_event_loop(&mut terminal, cli);

    let _ = io::stdout().write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\");
    let _ = io::stdout().flush();
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        Show,
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    res
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    cli: Cli,
) -> Result<()> {
    let mut app = App::new(cli);
    let mut last_frame = Instant::now();

    loop {
        let now = Instant::now();
        let dt_secs = now.duration_since(last_frame).as_secs_f64();
        last_frame = now;

        let (sync, mode_switched) = app.tick(dt_secs);
        if mode_switched {
            let _ = app.kitty.clear_placements();
            terminal.clear()?;
        }

        if app.ui.kitty_graphics {
            app.kitty
                .render_frame(app.current_lrc.as_ref(), &sync, &app.ui)?;
        } else {
            terminal.draw(|frame| {
                ui::draw(frame, app.current_lrc.as_ref(), &sync, &app.ui);
            })?;
        }

        let frame_duration = Duration::from_secs_f64(1.0 / app.fps() as f64);
        let timeout = frame_duration.saturating_sub(last_frame.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Resize(_, _) => {
                    app.kitty.invalidate();
                    if app.ui.kitty_graphics {
                        let _ = app.kitty.clear_placements();
                    }
                    terminal.clear()?;
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        break;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('x') => {
                            app.ui.kitty_graphics = !app.ui.kitty_graphics;
                            let _ = app.kitty.clear_placements();
                            terminal.clear()?;
                        }
                        KeyCode::Char('r') => app.force_rescan_and_match(),
                        KeyCode::Char('k') => {
                            app.ui.karaoke = !app.ui.karaoke;
                            app.kitty.invalidate();
                        }
                        KeyCode::Char('g') => {
                            app.ui.continuous_glide = !app.ui.continuous_glide;
                        }
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            app.ui.user_offset_ms += 100;
                        }
                        KeyCode::Char('-') | KeyCode::Char('_') => {
                            app.ui.user_offset_ms -= 100;
                        }
                        KeyCode::Char('0') => {
                            app.ui.user_offset_ms = 0;
                        }
                        KeyCode::Char('[') => {
                            if app.ui.kitty_graphics {
                                app.ui.pixel_line_spacing =
                                    (app.ui.pixel_line_spacing - 4.0).max(20.0);
                                app.kitty.invalidate();
                            } else {
                                app.ui.line_spacing = app.ui.line_spacing.saturating_sub(1).max(1);
                            }
                        }
                        KeyCode::Char(']') => {
                            if app.ui.kitty_graphics {
                                app.ui.pixel_line_spacing =
                                    (app.ui.pixel_line_spacing + 4.0).min(160.0);
                                app.kitty.invalidate();
                            } else {
                                app.ui.line_spacing = (app.ui.line_spacing + 1).min(5);
                            }
                        }
                        KeyCode::Up => {
                            app.ui.manual_line_offset -= 1;
                        }
                        KeyCode::Down => {
                            app.ui.manual_line_offset += 1;
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        app.ui.manual_line_offset -= 1;
                    }
                    MouseEventKind::ScrollDown => {
                        app.ui.manual_line_offset += 1;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    let _ = app.kitty.clear_placements();
    Ok(())
}
