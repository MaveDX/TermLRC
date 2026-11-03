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
use matcher::{LrcMatcher, TrackQuery};
use player::MprisTracker;
use providers::{is_valid_synced_lrc, FallbackWorker};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
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
}

impl App {
    fn new(cli: Cli) -> Self {
        let settings_mgr = SettingsManager::new(cli.clone());
        let s = settings_mgr.settings();
        let lyrics_dir = s.resolved_lyrics_dir(&cli);
        let matcher = LrcMatcher::new(lyrics_dir, s.match_threshold);
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
        }
    }

    fn fps(&self) -> u32 {
        self.settings_mgr.settings().fps.clamp(15, 240)
    }

    fn force_rescan_and_match(&mut self) {
        self.matcher.rescan();
        self.match_current_track();
    }

    fn match_current_track(&mut self) {
        self.kitty.invalidate();
        if self.current_query.is_empty() {
            self.current_lrc = None;
            return;
        }

        // 1. Try local recursive LRC folder first
        if let Some(m) = self.matcher.find_best_match(&self.current_query) {
            if let Ok(parsed) = ParsedLrc::from_file(&m.path) {
                if is_valid_synced_lrc(&parsed) {
                    self.current_lrc = Some(parsed);
                    return;
                }
            }
        }

        // 2. Local match failed or was unsynced: dispatch remote fallback chain in priority order
        self.current_lrc = None;
        let settings = self.settings_mgr.settings().clone();
        if !settings.enabled_fallback_providers().is_empty() {
            self.fallback_worker.request(
                self.current_query.clone(),
                self.current_track_length,
                settings,
            );
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
            if self.matcher.update_config(new_dir, updated.match_threshold) {
                self.match_current_track();
            }
        }

        let snap = self.tracker.update().clone();
        if snap.query != self.current_query {
            self.current_query = snap.query.clone();
            self.current_track_length = snap.length;
            self.ui.reset_for_new_track();
            self.match_current_track();
        } else {
            self.current_track_length = snap.length;
        }

        // Check if background fallback worker returned synced lyrics for the active track
        if let Some(res) = self.fallback_worker.poll_response() {
            if res.query == self.current_query && self.current_lrc.is_none() {
                if let Some(lrc) = res.lrc {
                    self.current_lrc = Some(lrc);
                    self.kitty.invalidate();
                }
            }
        }

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
