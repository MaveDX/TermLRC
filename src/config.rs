use crate::cli::Cli;
use ratatui::layout::Alignment;
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../config.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FallbackProviderKind {
    LrcLib,
    Musixmatch,
    Spotify,
    AppleMusic,
}

impl FallbackProviderKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "lrclib" | "lrc_lib" | "lrc-lib" => Some(Self::LrcLib),
            "musixmatch" | "musicmxtch" | "mxm" => Some(Self::Musixmatch),
            "spotify" => Some(Self::Spotify),
            "apple_music" | "applemusic" | "apple-music" | "apple" => Some(Self::AppleMusic),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::LrcLib => "LRCLIB",
            Self::Musixmatch => "Musixmatch",
            Self::Spotify => "Spotify",
            Self::AppleMusic => "Apple Music",
        }
    }
}

/// Application settings loaded from `config.toml` (with optional CLI overrides).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub lyrics_dir: Option<String>,
    pub player: Option<String>,
    pub kitty_graphics: bool,
    pub font_family: String,
    pub font_path: String,
    pub fallback_fonts: Vec<String>,
    pub font_size_active: f32,
    pub font_size_inactive: f32,
    pub pixel_line_spacing: f32,
    pub active_lyric_buffer_px: f32,
    pub inactive_lyric_buffer_px: f32,
    pub fps: u32,
    pub karaoke: bool,
    pub scroll_speed: f64,
    pub continuous_glide: bool,
    pub line_spacing: usize,
    pub active_lyric_buffer_rows: usize,
    pub offset_ms: i64,
    pub alignment: String,
    pub dim_inactive: bool,
    pub dim_past_only: bool,
    pub active_marker: String,
    pub empty_line_symbol: String,
    pub idle_symbol: String,
    pub mpris_poll_ms: u64,
    pub match_threshold: f64,
    pub fallback_match_threshold: f64,
    pub prefer_uncensored: bool,
    pub fallback_priority: Vec<String>,
    pub enable_lrclib: bool,
    pub enable_musixmatch: bool,
    pub enable_spotify: bool,
    pub enable_apple_music: bool,
    pub musixmatch_token: String,
    pub spotify_sp_dc: String,
    pub spotify_api_url: String,
    pub apple_music_bearer_token: String,
    pub apple_music_media_user_token: String,
    pub apple_music_api_url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            lyrics_dir: None,
            player: None,
            kitty_graphics: true,
            font_family: String::from("Noto Sans"),
            font_path: String::new(),
            fallback_fonts: vec![
                String::from("Noto Sans CJK"),
                String::from("IBM Plex Sans JP"),
                String::from("IBM Plex Sans SC"),
                String::from("IBM Plex Sans TC"),
                String::from("IBM Plex Sans KR"),
                String::from("WenQuanYi Zen Hei"),
                String::from("DejaVu Sans"),
                String::from("Unifont"),
            ],
            font_size_active: 17.5,
            font_size_inactive: 15.0,
            pixel_line_spacing: 35.0,
            active_lyric_buffer_px: 18.0,
            inactive_lyric_buffer_px: 10.0,
            fps: 60,
            karaoke: false,
            scroll_speed: 10.0,
            continuous_glide: false,
            line_spacing: 2,
            active_lyric_buffer_rows: 1,
            offset_ms: 0,
            alignment: String::from("center"),
            dim_inactive: true,
            dim_past_only: false,
            active_marker: String::new(),
            empty_line_symbol: String::from("♪"),
            idle_symbol: String::from("♪"),
            mpris_poll_ms: 250,
            match_threshold: 0.62,
            fallback_match_threshold: 0.72,
            prefer_uncensored: true,
            fallback_priority: vec![
                String::from("lrclib"),
                String::from("musixmatch"),
                String::from("spotify"),
                String::from("apple_music"),
            ],
            enable_lrclib: true,
            enable_musixmatch: true,
            enable_spotify: false,
            enable_apple_music: false,
            musixmatch_token: String::new(),
            spotify_sp_dc: String::new(),
            spotify_api_url: String::new(),
            apple_music_bearer_token: String::new(),
            apple_music_media_user_token: String::new(),
            apple_music_api_url: String::new(),
        }
    }
}

impl Settings {
    pub fn sanitized(mut self) -> Self {
        self.fps = self.fps.clamp(15, 240);
        self.font_size_active = self.font_size_active.clamp(10.0, 120.0);
        self.font_size_inactive = self.font_size_inactive.clamp(8.0, 96.0);
        self.pixel_line_spacing = self.pixel_line_spacing.clamp(12.0, 240.0);
        self.active_lyric_buffer_px = self.active_lyric_buffer_px.clamp(2.0, 160.0);
        self.inactive_lyric_buffer_px = self.inactive_lyric_buffer_px.clamp(2.0, 120.0);
        self.scroll_speed = self.scroll_speed.clamp(0.5, 50.0);
        self.line_spacing = self.line_spacing.clamp(1, 5);
        self.active_lyric_buffer_rows = self.active_lyric_buffer_rows.clamp(0, 4);
        self.mpris_poll_ms = self.mpris_poll_ms.clamp(50, 2000);
        self.match_threshold = self.match_threshold.clamp(0.1, 0.99);
        self.fallback_match_threshold = self.fallback_match_threshold.clamp(0.1, 0.99);
        if let Some(ref p) = self.player {
            if p.trim().is_empty() {
                self.player = None;
            }
        }
        self
    }

    /// Return the ordered list of enabled fallback providers according to `fallback_priority`.
    pub fn enabled_fallback_providers(&self) -> Vec<FallbackProviderKind> {
        let mut out = Vec::new();
        for name in &self.fallback_priority {
            if let Some(kind) = FallbackProviderKind::from_name(name) {
                let enabled = match kind {
                    FallbackProviderKind::LrcLib => self.enable_lrclib,
                    FallbackProviderKind::Musixmatch => self.enable_musixmatch,
                    FallbackProviderKind::Spotify => self.enable_spotify,
                    FallbackProviderKind::AppleMusic => self.enable_apple_music,
                };
                if enabled && !out.contains(&kind) {
                    out.push(kind);
                }
            }
        }
        out
    }

    pub fn text_alignment(&self) -> Alignment {
        match self.alignment.trim().to_ascii_lowercase().as_str() {
            "left" => Alignment::Left,
            "right" => Alignment::Right,
            _ => Alignment::Center,
        }
    }

    pub fn resolved_lyrics_dir(&self, cli: &Cli) -> PathBuf {
        if let Some(ref d) = cli.dir_flag {
            return expand_tilde(d);
        }
        if let Some(ref f) = cli.folder {
            return expand_tilde(f);
        }
        if let Some(ref cfg_dir) = self.lyrics_dir {
            let trimmed = cfg_dir.trim();
            if !trimmed.is_empty() {
                return expand_tilde(&PathBuf::from(trimmed));
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let music = PathBuf::from(home).join("Music");
            if music.is_dir() {
                return music;
            }
        }
        PathBuf::from(".")
    }

    pub fn apply_cli_overrides(&mut self, cli: &Cli) {
        if let Some(ref player) = cli.player {
            self.player = if player.trim().is_empty() {
                None
            } else {
                Some(player.clone())
            };
        }
        if let Some(kitty) = cli.kitty_graphics {
            self.kitty_graphics = kitty;
        }
        if let Some(ref font) = cli.font_path {
            if let Some(s) = font.to_str() {
                self.font_path = s.to_string();
            }
        }
        if let Some(fps) = cli.fps {
            self.fps = fps;
        }
        if let Some(offset) = cli.offset_ms {
            self.offset_ms = offset;
        }
        if let Some(spacing) = cli.line_spacing {
            self.line_spacing = spacing;
        }
        if let Some(speed) = cli.scroll_speed {
            self.scroll_speed = speed;
        }
        if let Some(glide) = cli.continuous_glide {
            self.continuous_glide = glide;
        }
        if let Some(karaoke) = cli.karaoke {
            self.karaoke = karaoke;
        }
        *self = self.clone().sanitized();
    }
}

/// Manages loading and live-reloading `config.toml` while preserving any CLI overrides.
pub struct SettingsManager {
    config_paths: Vec<PathBuf>,
    last_active_path: Option<PathBuf>,
    last_mtime: Option<SystemTime>,
    last_check: Instant,
    cli: Cli,
    current: Settings,
}

impl SettingsManager {
    pub fn new(cli: Cli) -> Self {
        let config_paths = resolve_or_create_config_paths(cli.config.as_ref());
        let (last_active_path, last_mtime, mut current) =
            load_newest_config(&config_paths).unwrap_or_else(|| (None, None, Settings::default()));
        current.apply_cli_overrides(&cli);

        Self {
            config_paths,
            last_active_path,
            last_mtime,
            last_check: Instant::now(),
            cli,
            current,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.current
    }

    /// Periodically check if `config.toml` was modified on disk and reload it if so.
    /// Returns `Some(&Settings)` when the settings have changed.
    pub fn poll_reload(&mut self) -> Option<&Settings> {
        if self.last_check.elapsed() < Duration::from_millis(350) {
            return None;
        }
        self.last_check = Instant::now();

        let (newest_path, newest_mtime) = find_newest_mtime(&self.config_paths)?;
        if self.last_active_path.as_ref() == Some(&newest_path)
            && self.last_mtime == Some(newest_mtime)
        {
            return None;
        }

        // Only advance `last_mtime` once `load_settings_from_file` succeeds so atomic/partial saves retry cleanly
        let mut loaded = load_settings_from_file(&newest_path)?;
        self.last_active_path = Some(newest_path);
        self.last_mtime = Some(newest_mtime);

        loaded.apply_cli_overrides(&self.cli);
        if loaded != self.current {
            self.current = loaded;
            return Some(&self.current);
        }
        None
    }
}

fn load_settings_from_file(path: &Path) -> Option<Settings> {
    let content = fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    let parsed: Settings = toml::from_str(&content).ok()?;
    Some(parsed.sanitized())
}

fn find_newest_mtime(paths: &[PathBuf]) -> Option<(PathBuf, SystemTime)> {
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for p in paths {
        if let Ok(mtime) = fs::metadata(p).and_then(|m| m.modified()) {
            let is_newer = match &best {
                None => true,
                Some((_, prev_t)) => mtime > *prev_t,
            };
            if is_newer {
                best = Some((p.clone(), mtime));
            }
        }
    }
    best
}

fn load_newest_config(paths: &[PathBuf]) -> Option<(Option<PathBuf>, Option<SystemTime>, Settings)> {
    let (newest_path, newest_mtime) = find_newest_mtime(paths)?;
    let settings = load_settings_from_file(&newest_path)?;
    Some((Some(newest_path), Some(newest_mtime), settings))
}

fn resolve_or_create_config_paths(custom: Option<&PathBuf>) -> Vec<PathBuf> {
    if let Some(p) = custom {
        return vec![expand_tilde(p)];
    }

    let mut paths = Vec::new();
    let local = PathBuf::from("config.toml");
    if local.is_file() {
        paths.push(local);
    }

    if let Ok(xdg_dir) = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        let user_cfg = xdg_dir.join("termlrc").join("config.toml");
        if !user_cfg.exists() && paths.is_empty() {
            if let Some(parent) = user_cfg.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&user_cfg, DEFAULT_CONFIG_TEMPLATE);
        }
        if user_cfg.is_file() {
            paths.push(user_cfg);
        }
    }

    paths
}

pub fn expand_tilde(path: &PathBuf) -> PathBuf {
    if let Some(s) = path.to_str() {
        if s == "~" {
            if let Ok(home) = std::env::var("HOME") {
                return PathBuf::from(home);
            }
        } else if let Some(rest) = s.strip_prefix("~/") {
            if let Ok(home) = std::env::var("HOME") {
                return PathBuf::from(home).join(rest);
            }
        }
    }
    path.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_default_config_template() {
        let parsed: Settings =
            toml::from_str(DEFAULT_CONFIG_TEMPLATE).expect("Default config.toml must be valid TOML");
        let s = parsed.sanitized();
        assert!((15..=240).contains(&s.fps));
        assert!((1..=5).contains(&s.line_spacing));

        let mut custom = Settings::default();
        custom.fallback_priority = vec![
            String::from("musixmatch"),
            String::from("lrclib"),
            String::from("spotify"),
        ];
        custom.enable_musixmatch = true;
        custom.enable_lrclib = true;
        custom.enable_spotify = false;
        assert_eq!(
            custom.enabled_fallback_providers(),
            vec![
                FallbackProviderKind::Musixmatch,
                FallbackProviderKind::LrcLib
            ]
        );
    }
}
