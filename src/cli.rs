use clap::Parser;
use std::path::PathBuf;

/// Pure synced-lyrics terminal showcase with Kitty pixel-level scrolling, MPRIS song detection, and recursive LRC search.
#[derive(Parser, Debug, Clone)]
#[command(name = "termlrc", version, about, long_about = None)]
pub struct Cli {
    /// Path to the folder containing LRC files (searched recursively).
    /// Overrides `lyrics_dir` in config.toml.
    #[arg(value_name = "LYRICS_DIR", env = "TERMLRC_DIR")]
    pub folder: Option<PathBuf>,

    /// Alternative flag to specify the folder containing LRC files.
    #[arg(short = 'd', long = "dir", value_name = "DIR")]
    pub dir_flag: Option<PathBuf>,

    /// Path to a custom config.toml settings file.
    #[arg(short = 'c', long = "config", value_name = "FILE", env = "TERMLRC_CONFIG")]
    pub config: Option<PathBuf>,

    /// Preferred MPRIS media player bus name fragment (e.g., "spotify", "mpd", "vlc").
    #[arg(short = 'p', long = "player", value_name = "PLAYER")]
    pub player: Option<String>,

    /// Enable or disable Kitty Graphics Protocol pixel-scrolling mode (overrides config.toml).
    #[arg(long = "kitty")]
    pub kitty_graphics: Option<bool>,

    /// Path to a custom .ttf or .otf font file for Kitty graphics mode (overrides config.toml).
    #[arg(long = "font", value_name = "FONT_PATH")]
    pub font_path: Option<PathBuf>,

    /// Target frame rate for smooth scrolling and animations (overrides config.toml).
    #[arg(long = "fps")]
    pub fps: Option<u32>,

    /// Initial lyric timing offset in milliseconds (overrides config.toml).
    #[arg(short = 'o', long = "offset")]
    pub offset_ms: Option<i64>,

    /// Vertical line spacing (number of rows per lyric entry, 1..=5, overrides config.toml).
    #[arg(short = 's', long = "spacing")]
    pub line_spacing: Option<usize>,

    /// Smooth scroll damping speed (higher = snappier, lower = softer glide, overrides config.toml).
    #[arg(long = "scroll-speed")]
    pub scroll_speed: Option<f64>,

    /// Enable or disable continuous vertical glide (overrides config.toml).
    #[arg(short = 'g', long = "glide")]
    pub continuous_glide: Option<bool>,

    /// Enable or disable karaoke-style highlight sweep (overrides config.toml).
    #[arg(short = 'k', long = "karaoke")]
    pub karaoke: Option<bool>,
}
