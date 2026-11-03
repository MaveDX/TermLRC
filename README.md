# TermLRC

**TermLRC** is a lightweight, synced-lyrics terminal showcase written in Rust. It listens to **MPRIS2** over D-Bus to track your currently playing song and playback position, searches a local music directory recursively for matching `.lrc` files, and optionally falls back to remote synced lyric providers (**Spotify**, **Apple Music**, **Musixmatch**, **LRCLIB**).

## Features

- **Pixel-Level Smooth Scrolling (Kitty Graphics Protocol)**
  - Renders vector fonts (`ab_glyph`) onto a transparent RGBA canvas (`f=32`) sized to your terminal's exact pixel geometry (`TIOCGWINSZ`), preserving your terminal's native background color, opacity, and blur.
  - Focus-invariant line wrapping and guaranteed non-overlapping vertical buffers (`active_lyric_buffer_px`, `inactive_lyric_buffer_px`) so lyrics never warp or collide when focused.
  - Press `x` at any time to toggle between Kitty pixel-scrolling mode and standard text TUI mode.
- **Configurable Fonts & Lazy CJK Fallback**
  - Supports any installed proportional or monospace font (`font_family = "Inter"`, `"Maple Mono NF"`, `"Noto Sans"`, etc.) or direct `font_path`, with on-demand fallback (`fallback_fonts`) for Japanese, Chinese, Korean, and Unicode symbols while keeping idle RAM usage around ~1–2 MB.
- **Local Recursive `.lrc` Search + Remote Synced Fallbacks**
  - Searches your local music folder first (`match_threshold = 0.62`), then queries enabled fallback providers in your configured `fallback_priority` order (`fallback_match_threshold = 0.72`).
  - **Synced-Only & Uncensored Priority**: Rejects unsynced/plain-text lyrics and prioritizes explicit/uncensored lyrics over censored (`f**k`, `***`, `Clean`, `Radio Edit`) versions (`prefer_uncensored = true`).
  - Displays a scrollable `"Lyrics from <Provider>"` attribution header above the first line when a remote fallback provider is used.
  - Built-in per-provider rate limiting, token caching, and session LRU lyric caching.

## Usage

### Nix Flake

Run directly without installing:

```bash
nix run github:MaveDX/TermLRC -- /path/to/music
```

Or build locally:

```bash
nix build
./result/bin/termlrc /path/to/music
```

### Cargo

```bash
cargo build --release
./target/release/termlrc /path/to/music
```

## Configuration (`config.toml`)

TermLRC automatically creates and live-reloads `config.toml` while running. It checks:
1. `--config <PATH>` (or `TERMLRC_CONFIG`)
2. `./config.toml`
3. `~/.config/termlrc/config.toml`

Key settings in [`config.toml`](config.toml):

```toml
# Default folder to search recursively for .lrc files
lyrics_dir = "/path/to/music"

# Display & Fonts
kitty_graphics = true
font_family = "Inter"
font_size_active = 25.0
font_size_inactive = 17.5
pixel_line_spacing = 25.0
active_lyric_buffer_px = 18.0
inactive_lyric_buffer_px = 10.0
alignment = "left" # "left", "center", or "right"

# Matching & Remote Fallbacks
match_threshold = 0.62
fallback_match_threshold = 0.72
prefer_uncensored = true
fallback_priority = ["spotify", "apple_music", "musixmatch", "lrclib"]

enable_lrclib = true
enable_musixmatch = true
enable_spotify = true
enable_apple_music = true

# Optional provider credentials (e.g. Spotify web cookie `sp_dc` from open.spotify.com)
spotify_sp_dc = ""
```

## Keybindings

| Key | Action |
| --- | --- |
| `q` / `Esc` | Quit |
| `x` | Toggle Kitty Graphics pixel-scrolling / Text TUI mode |
| `k` | Toggle karaoke word/line sweep highlight |
| `g` | Toggle continuous vertical glide vs. smooth step scroll |
| `c` | Cycle alignment (`left` / `center` / `right`) |
| `[` / `]` | Adjust lyric timing offset (`-50ms` / `+50ms`, `0` to reset) |
| `-` / `+` | Adjust scroll damping speed |
| `r` | Re-scan local `.lrc` directory and remote fallbacks |
