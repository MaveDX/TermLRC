# TermLRC

**TermLRC** is a lightweight, synced-lyrics terminal showcase written in Rust. It listens to **MPRIS2** over D-Bus to track your currently playing song and playback position, and resolves lyrics through a multi-stage local and remote pipeline.

## Lyric Sourcing Pipeline

1. **Direct Sibling `.lrc` & Local Folder Search**
   - When a local audio file has a sibling `.lrc` file with the same name (for example `song.flac` and `song.lrc`), those lyrics are used immediately without any network request.
   - Otherwise, TermLRC searches your configured local `lyrics_dir` recursively (`match_threshold = 0.62`).
2. **Video-Style Metadata Cleanup & Recording Identification**
   - Cleans up video-style metadata (such as `Artist x Guest - Song (Official Video)` titles and uploader channel names) and identifies the canonical recording through [Deezer](https://www.deezer.com/) and [MusicBrainz](https://musicbrainz.org/).
3. **Ordered Remote Providers & Line-by-Line Consensus**
   - Queries enabled providers in order:
     1. [lrcmux](https://lrcmux.dev/)
     2. [LyricsPlus](https://github.com/ibratabian17/lyricsplus)
     3. [LRCLIB](https://lrclib.net/)
     4. [KuGou](https://www.kugou.com/) and [NetEase Cloud Music](https://music.163.com/), queried **only** when none of the above has synced lyrics timed for a recording of the playing length.
   - When LRCLIB, KuGou, or NetEase finds nothing for the full metadata, it is searched once more with just the song title and its artist, leaving out the uploader's channel name (for example `Crazy chomin` for a `Crazy (feat. Yina) by chomin` video from the `DJMAX ARCHIVE` channel).
   - **Length Match & Cross-Source Line Consensus**: Lyrics timed for a recording whose length matches the playing track are preferred. When none match, the synced lyrics from every source that has them are compared line by line, and the version whose line timings the other sources confirm wins (ties go to the earlier source in the list). Plain text is used only when nothing is synced. Recordings up to 30 seconds longer or shorter than the playing track are still matched; ambiguous or unrelated recordings are rejected.
4. **Persistent Caching, Stability & Backoff**
   - Results are cached in memory and in `~/.local/share/termlrc/lyrics_cache.json` (`$XDG_DATA_HOME/termlrc/lyrics_cache.json`), so restarts do not repeat lookups.
   - Small duration or album updates do not invalidate found lyrics, and temporary failures keep the last lyrics for the unchanged track visible.
   - Providers that fail or rate-limit are skipped for a cooldown period, and failed lookups are retried after 30 seconds with doubling delays up to 10 minutes.

## Usage

### Nix Flake

```bash
nix run github:MaveDX/TermLRC -- /path/to/music
```

### Cargo

```bash
cargo build --release
./target/release/termlrc /path/to/music
```

## Configuration (`config.toml`)

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
alignment = "left"

# Matching & Remote Fallbacks
match_threshold = 0.62
fallback_match_threshold = 0.72
prefer_uncensored = true
enable_metadata_identify = true
save_lrc_file = true
save_lrc_threshold = 0.85
fallback_priority = ["lrcmux", "lyricsplus", "lrclib", "kugou", "netease"]

enable_lrcmux = true
enable_lyricsplus = true
enable_lrclib = true
enable_kugou = true
enable_netease = true
```
