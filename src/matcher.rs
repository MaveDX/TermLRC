use crate::lrc::{LrcMetadata, ParsedLrc};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;
use walkdir::WalkDir;

/// Media category automatically detected by Smart Identification via embedded file metadata,
/// MPRIS metadata, remote provider flags, or directory/playlist folder names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SmartMediaType {
    Instrumental,
    LoFi,
    Podcast,
    Audiobook,
    Soundtrack,
    Ambient,
    DjMix,
}

impl SmartMediaType {
    pub fn status_label(self) -> &'static str {
        match self {
            Self::Instrumental => "Instrumental",
            Self::LoFi => "Lo-Fi",
            Self::Podcast => "Podcast",
            Self::Audiobook => "Audiobook",
            Self::Soundtrack => "Soundtrack",
            Self::Ambient => "Ambient",
            Self::DjMix => "DJ Mix",
        }
    }

    pub fn tag_code(self) -> u8 {
        match self {
            Self::Instrumental => 10,
            Self::LoFi => 11,
            Self::Podcast => 12,
            Self::Audiobook => 13,
            Self::Soundtrack => 14,
            Self::Ambient => 15,
            Self::DjMix => 16,
        }
    }
}

/// Song information retrieved from MPRIS used to locate the matching LRC file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackQuery {
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub url: Option<String>,
    pub genres: Vec<String>,
    pub comments: Vec<String>,
}

impl TrackQuery {
    pub fn primary_artist(&self) -> Option<&str> {
        self.artists.first().map(|s| s.as_str()).filter(|s| !s.is_empty())
    }

    pub fn joined_artists(&self) -> String {
        self.artists.join(", ")
    }

    pub fn is_empty(&self) -> bool {
        self.title.trim().is_empty() && self.url.is_none()
    }

    /// Check if `other` refers to the same playing song (ignoring small duration or album updates).
    pub fn is_same_track(&self, other: &Self, self_len: Option<Duration>, other_len: Option<Duration>) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        let same_title = normalize(&self.title) == normalize(&other.title);
        let same_artist = normalize(&self.joined_artists()) == normalize(&other.joined_artists());
        if !same_title || !same_artist {
            return false;
        }
        if let (Some(a), Some(b)) = (self_len, other_len) {
            if a.as_secs_f64() > 5.0 && b.as_secs_f64() > 5.0 {
                if (a.as_secs_f64() - b.as_secs_f64()).abs() > 10.0 {
                    return false;
                }
            }
        }
        true
    }
}

/// Cleaned and decomposed metadata extracted from raw MPRIS / video-style track tags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanedTrackMetadata {
    /// Cleaned title (video suffixes like `(Official Video)` stripped, but featuring info may remain).
    pub title: String,
    /// Primary artist (extracted from embedded title or cleaned MPRIS artist).
    pub artist: String,
    /// Core song title with featuring/remix parentheticals stripped (e.g. `"Crazy"` from `"Crazy (feat. Yina)"`).
    pub core_title: String,
    /// Core artist with collaboration guests (`x Guest`, `feat. Guest`) and uploader channel names removed.
    pub core_artist: String,
    /// Album name if present and not identical to a video platform name.
    pub album: Option<String>,
    /// Original uploader / channel name if separated from an embedded artist in the video title.
    pub uploader_channel: Option<String>,
}

impl CleanedTrackMetadata {
    /// Return the stripped `"<song_title> <song_artist>"` search query leaving out the uploader's channel name
    /// (for example `"Crazy chomin"` for `"Crazy (feat. Yina) by chomin"` from `"DJMAX ARCHIVE"`).
    pub fn stripped_title_artist_query(&self) -> String {
        let t = self.core_title.trim();
        let a = self.core_artist.trim();
        if !t.is_empty() && !a.is_empty() {
            format!("{} {}", t, a)
        } else if !t.is_empty() {
            t.to_string()
        } else {
            a.to_string()
        }
    }

    /// Whether the stripped `(core_title, core_artist)` differs meaningfully from `(title, artist)` or raw MPRIS tags.
    pub fn has_distinct_stripped_query(&self, raw: &TrackQuery) -> bool {
        let raw_ar = raw.primary_artist().unwrap_or("");
        self.uploader_channel.is_some()
            || normalize(&self.core_title) != normalize(&self.title)
            || normalize(&self.core_artist) != normalize(&self.artist)
            || (!raw_ar.is_empty() && normalize(&self.core_artist) != normalize(raw_ar))
    }
}

/// Resolve `query.url` to a local filesystem audio path if it points to a local file.
pub fn resolve_local_audio_path(query: &TrackQuery) -> Option<PathBuf> {
    let raw_url = query.url.as_deref()?.trim();
    if raw_url.is_empty()
        || raw_url.starts_with("http://")
        || raw_url.starts_with("https://")
        || raw_url.starts_with("spotify:")
        || raw_url.starts_with("/org/mpris/")
    {
        return None;
    }

    let path_part = raw_url.strip_prefix("file://").unwrap_or(raw_url);
    let decoded = urlencoding::decode(path_part).ok()?;
    let audio_path = Path::new(decoded.as_ref());
    if audio_path.is_file() || audio_path.parent().map_or(false, |p| p.is_dir()) {
        Some(audio_path.to_path_buf())
    } else {
        None
    }
}

/// Check if `query.url` points to a local audio file that has a sibling `.lrc` file with the same stem
/// in the exact same directory (e.g. `/path/to/song.flac` -> `/path/to/song.lrc`).
pub fn find_sibling_lrc(query: &TrackQuery) -> Option<PathBuf> {
    let audio_path = resolve_local_audio_path(query)?;

    // Direct `<stem>.lrc` check
    let direct_lrc = audio_path.with_extension("lrc");
    if direct_lrc.is_file() {
        return Some(direct_lrc);
    }
    let upper_lrc = audio_path.with_extension("LRC");
    if upper_lrc.is_file() {
        return Some(upper_lrc);
    }

    // Case-insensitive stem check in the same directory
    let parent = audio_path.parent()?;
    let stem = audio_path.file_stem()?.to_str()?;
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let is_lrc = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("lrc"))
                .unwrap_or(false);
            if is_lrc {
                if let Some(s) = p.file_stem().and_then(|s| s.to_str()) {
                    if s.eq_ignore_ascii_case(stem) {
                        return Some(p);
                    }
                }
            }
        }
    }

    None
}

/// Clean up video-style metadata (such as `Artist x Guest - Song (Official Video)` titles,
/// `Crazy (feat. Yina) by chomin` titles, and uploader channel names).
pub fn clean_video_metadata(query: &TrackQuery) -> CleanedTrackMetadata {
    let raw_title = query.title.trim();
    let raw_artist = query.primary_artist().unwrap_or("").trim();

    let mut working_title = strip_video_tags(raw_title);
    let mut extracted_artist: Option<String> = None;
    let mut uploader_channel: Option<String> = None;

    // 1. Check for `"<Song> by <Artist>"` pattern (e.g. `"Crazy (feat. Yina) by chomin"`)
    if let Some((song_part, by_artist)) = split_by_artist_suffix(&working_title) {
        let new_title = song_part.to_string();
        let new_artist = by_artist.to_string();
        if !raw_artist.is_empty() && normalize(raw_artist) != normalize(&new_artist) {
            uploader_channel = Some(raw_artist.to_string());
        }
        working_title = new_title;
        extracted_artist = Some(new_artist);
    } else if let Some((left_artist, right_song)) = split_embedded_artist_title(&working_title, raw_artist) {
        // 2. Check for `"<Artist> - <Song>"` embedded in video title
        //    (e.g. `"Artist x Guest - Song (Official Video)"`)
        let new_title = strip_video_tags(right_song);
        let new_artist = left_artist.to_string();
        if !raw_artist.is_empty() && normalize(raw_artist) != normalize(&new_artist) {
            uploader_channel = Some(raw_artist.to_string());
        }
        working_title = new_title;
        extracted_artist = Some(new_artist);
    }

    let full_artist = match extracted_artist {
        Some(ref ar) => clean_channel_suffix(ar),
        None => clean_channel_suffix(raw_artist),
    };

    let core_title = strip_featuring_and_decorations(&working_title);
    let core_artist = extract_primary_collab_artist(&full_artist);

    let album = query
        .album
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty() && !is_generic_video_album(a))
        .map(String::from);

    CleanedTrackMetadata {
        title: if working_title.is_empty() {
            raw_title.to_string()
        } else {
            working_title
        },
        artist: full_artist,
        core_title: if core_title.is_empty() {
            raw_title.to_string()
        } else {
            core_title
        },
        core_artist,
        album,
        uploader_channel,
    }
}

fn strip_video_tags(input: &str) -> String {
    const VIDEO_KEYWORDS: &[&str] = &[
        "official video",
        "official music video",
        "official audio",
        "official lyric video",
        "official lyrics",
        "official visualizer",
        "official mv",
        "music video",
        "lyric video",
        "lyrics video",
        "visualizer",
        "full audio",
        "audio only",
        "m/v",
        "mv",
        "hd",
        "4k",
        "hq",
        "1080p",
        "720p",
    ];

    let mut out = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let open = chars[i];
        let close = match open {
            '(' => Some(')'),
            '[' => Some(']'),
            '【' => Some('】'),
            _ => None,
        };
        if let Some(close_ch) = close {
            if let Some(rel_end) = chars[i + 1..].iter().position(|&c| c == close_ch) {
                let inside: String = chars[i + 1..i + 1 + rel_end].iter().collect();
                let inside_low = inside.trim().to_ascii_lowercase();
                let is_video_noise = VIDEO_KEYWORDS.iter().any(|&kw| {
                    inside_low == kw
                        || inside_low.starts_with(&format!("{} ", kw))
                        || inside_low.ends_with(&format!(" {}", kw))
                        || (kw.len() >= 5 && inside_low.contains(kw))
                });
                if is_video_noise {
                    i += rel_end + 2;
                    continue;
                }
            }
        }
        out.push(open);
        i += 1;
    }

    // Also strip trailing `- Official Video`, `| Official Music Video`, etc.
    let mut cleaned = out.trim().to_string();
    for sep in [" - ", " | ", " // ", " / "] {
        if let Some((left, right)) = cleaned.rsplit_once(sep) {
            let right_low = right.trim().to_ascii_lowercase();
            if VIDEO_KEYWORDS.iter().any(|&kw| right_low == kw || (kw.len() >= 5 && right_low.contains(kw))) {
                cleaned = left.trim().to_string();
            }
        }
    }

    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn split_by_artist_suffix(title: &str) -> Option<(&str, &str)> {
    // Match case-insensitive `" by "` near the end of the title (outside parentheses)
    let lower = title.to_ascii_lowercase();
    let idx = lower.rfind(" by ")?;
    let left = title[..idx].trim();
    let right = title[idx + 4..].trim();
    if left.is_empty()
        || right.is_empty()
        || left.contains('(') && !left.contains(')')
        || right.contains(')')
    {
        return None;
    }
    Some((left, right))
}

fn split_embedded_artist_title<'a>(title: &'a str, raw_artist: &str) -> Option<(&'a str, &'a str)> {
    for sep in [" - ", " – ", " — "] {
        if let Some((left, right)) = title.split_once(sep) {
            let l = left.trim();
            let r = right.trim();
            if !l.is_empty() && !r.is_empty() && !l.chars().all(|c| c.is_ascii_digit()) {
                // Avoid splitting if the right side is just a remix/remaster tag
                let r_low = r.to_ascii_lowercase();
                if r_low.starts_with("remaster")
                    || r_low.starts_with("live")
                    || r_low.starts_with("radio edit")
                    || r_low.starts_with("single")
                {
                    continue;
                }
                let _ = raw_artist;
                return Some((l, r));
            }
        }
    }
    None
}

fn clean_channel_suffix(artist: &str) -> String {
    let mut s = artist.trim().to_string();
    for suffix in [
        " - Topic",
        " - topic",
        " Official Channel",
        " Official",
        "VEVO",
        "vevo",
    ] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            let trimmed = stripped.trim();
            if !trimmed.is_empty() {
                s = trimmed.to_string();
            }
        }
    }
    s
}

fn strip_featuring_and_decorations(title: &str) -> String {
    let base = strip_decorations(title);
    let lower = base.to_ascii_lowercase();
    for marker in [" feat.", " feat ", " ft.", " ft ", " with ", " prod."] {
        if let Some(idx) = lower.find(marker) {
            if idx > 0 {
                return base[..idx].trim().to_string();
            }
        }
    }
    base.trim().to_string()
}

fn extract_primary_collab_artist(artist: &str) -> String {
    let base = strip_decorations(artist);
    let lower = base.to_ascii_lowercase();
    for sep in [
        " feat.",
        " feat ",
        " ft.",
        " ft ",
        " x ",
        " × ",
        " vs.",
        " vs ",
        " with ",
        ", ",
        " & ",
    ] {
        if let Some(idx) = lower.find(sep) {
            let first = base[..idx].trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    base.trim().to_string()
}

fn is_generic_video_album(album: &str) -> bool {
    let low = album.trim().to_ascii_lowercase();
    matches!(
        low.as_str(),
        "youtube" | "youtube music" | "soundcloud" | "bilibili" | "nicovideo" | "vimeo"
    )
}

/// An indexed `.lrc` file discovered during recursive directory traversal.
#[derive(Debug, Clone)]
pub struct IndexedLrc {
    pub path: PathBuf,
    pub file_stem: String,
    pub norm_stem: String,
    pub stripped_stem: String,
    pub parent_dir: String,
    pub grandparent_dir: String,
    pub metadata: LrcMetadata,
}

/// Result of a successful LRC match.
#[derive(Debug, Clone)]
pub struct MatchResult {
    pub path: PathBuf,
    pub score: f64,
}

/// Check if `path` is inside any of the configured `blacklist_dirs`.
pub fn is_path_blacklisted(path: &Path, blacklist_dirs: &[PathBuf]) -> bool {
    if blacklist_dirs.is_empty() {
        return false;
    }
    let canon_path = path.canonicalize().ok();
    let path_str_low = path.to_string_lossy().to_lowercase();

    for b_dir in blacklist_dirs {
        let b_str = b_dir.to_string_lossy();
        let b_trimmed = b_str.trim().trim_end_matches(['/', '\\']);
        if b_trimmed.is_empty() {
            continue;
        }
        if path.starts_with(b_dir) || path.starts_with(b_trimmed) {
            return true;
        }
        if let (Some(ref cp), Ok(cb)) = (&canon_path, b_dir.canonicalize()) {
            if cp.starts_with(&cb) {
                return true;
            }
        }
        let b_low = b_trimmed.to_lowercase();
        if b_low.starts_with('/') || b_low.contains('/') {
            if path_str_low == b_low || path_str_low.starts_with(&format!("{}/", b_low)) {
                return true;
            }
        } else {
            // Relative directory name match against any path component
            if path.components().any(|comp| {
                comp.as_os_str()
                    .to_str()
                    .map(|s| s.eq_ignore_ascii_case(b_trimmed))
                    .unwrap_or(false)
            }) {
                return true;
            }
        }
    }
    false
}

/// Recursive LRC directory scanner and fuzzy/exact matcher.
#[derive(Debug, Default)]
pub struct LrcMatcher {
    root_dir: PathBuf,
    match_threshold: f64,
    blacklist_dirs: Vec<PathBuf>,
    entries: Vec<IndexedLrc>,
}

impl LrcMatcher {
    /// Create a new matcher pointed at `root_dir` and perform an initial recursive scan.
    #[cfg(test)]
    pub fn new(root_dir: PathBuf, match_threshold: f64) -> Self {
        Self::with_blacklist(root_dir, match_threshold, Vec::new())
    }

    /// Create a new matcher with an explicit list of blacklisted directories.
    pub fn with_blacklist(
        root_dir: PathBuf,
        match_threshold: f64,
        blacklist_dirs: Vec<PathBuf>,
    ) -> Self {
        let mut matcher = Self {
            root_dir,
            match_threshold: match_threshold.clamp(0.1, 0.99),
            blacklist_dirs,
            entries: Vec::new(),
        };
        matcher.rescan();
        matcher
    }

    pub fn update_config(
        &mut self,
        root_dir: PathBuf,
        match_threshold: f64,
        blacklist_dirs: Vec<PathBuf>,
    ) -> bool {
        self.match_threshold = match_threshold.clamp(0.1, 0.99);
        if self.root_dir != root_dir || self.blacklist_dirs != blacklist_dirs {
            self.root_dir = root_dir;
            self.blacklist_dirs = blacklist_dirs;
            self.rescan();
            return true;
        }
        false
    }

    /// Check whether the currently playing track originates from a blacklisted directory.
    pub fn is_track_blacklisted(&self, query: &TrackQuery) -> bool {
        if self.blacklist_dirs.is_empty() || query.is_empty() {
            return false;
        }
        if let Some(audio_path) = resolve_local_audio_path(query) {
            if is_path_blacklisted(&audio_path, &self.blacklist_dirs) {
                return true;
            }
        }
        if let Some(ref raw_url) = query.url {
            let decoded = urlencoding::decode(raw_url)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| raw_url.clone());
            let clean = decoded.strip_prefix("file://").unwrap_or(&decoded);
            if is_path_blacklisted(Path::new(clean), &self.blacklist_dirs) {
                return true;
            }
        }
        // If MPRIS did not provide a file:// URL, check if the track matches an audio file inside any blacklisted directory
        for b_dir in &self.blacklist_dirs {
            let resolved_b = if b_dir.is_relative() && self.root_dir.join(b_dir).is_dir() {
                self.root_dir.join(b_dir)
            } else {
                b_dir.clone()
            };
            if !resolved_b.is_dir() {
                continue;
            }
            for entry in WalkDir::new(&resolved_b)
                .follow_links(true)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let file_stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                if file_stem.is_empty() {
                    continue;
                }
                let candidate = IndexedLrc {
                    path: path.to_path_buf(),
                    norm_stem: normalize(&file_stem),
                    stripped_stem: normalize(&strip_leading_track_num(&strip_decorations(&file_stem))),
                    file_stem,
                    parent_dir: path
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|s| s.to_str())
                        .map(normalize)
                        .unwrap_or_default(),
                    grandparent_dir: path
                        .parent()
                        .and_then(|p| p.parent())
                        .and_then(|p| p.file_name())
                        .and_then(|s| s.to_str())
                        .map(normalize)
                        .unwrap_or_default(),
                    metadata: LrcMetadata::default(),
                };
                if score_candidate(&candidate, query, None) >= self.match_threshold {
                    return true;
                }
            }
        }
        false
    }

    #[cfg(test)]
    pub fn indexed_count(&self) -> usize {
        self.entries.len()
    }

    /// Recursively scan `root_dir` for all `.lrc` files.
    pub fn rescan(&mut self) {
        self.entries.clear();
        if !self.root_dir.exists() {
            return;
        }

        for entry in WalkDir::new(&self.root_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() || is_path_blacklisted(path, &self.blacklist_dirs) {
                continue;
            }
            let is_lrc = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("lrc"))
                .unwrap_or(false);
            if !is_lrc {
                continue;
            }

            let file_stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let norm_stem = normalize(&file_stem);
            let stripped_stem = normalize(&strip_leading_track_num(& strip_decorations(&file_stem)));

            let parent_dir = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .map(normalize)
                .unwrap_or_default();

            let grandparent_dir = path
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .map(normalize)
                .unwrap_or_default();

            let metadata = ParsedLrc::read_metadata_only(path).unwrap_or_default();

            self.entries.push(IndexedLrc {
                path: path.to_path_buf(),
                file_stem,
                norm_stem,
                stripped_stem,
                parent_dir,
                grandparent_dir,
                metadata,
            });
        }
        self.entries.shrink_to_fit();
    }

    /// Find the best matching `.lrc` file for the given track query.
    /// First checks if the local audio file has a direct sibling `.lrc` file with the same name.
    /// Otherwise searches the indexed directory (rescanning once if needed).
    pub fn find_best_match(&mut self, query: &TrackQuery) -> Option<MatchResult> {
        if query.is_empty() {
            return None;
        }

        if let Some(sibling) = find_sibling_lrc(query) {
            return Some(MatchResult {
                path: sibling,
                score: 1.0,
            });
        }

        if let Some(res) = self.search_indexed(query) {
            return Some(res);
        }

        // Rescan once in case LRC files were recently added to the folder
        self.rescan();
        self.search_indexed(query)
    }

    /// Search already-indexed LRC files without triggering a recursive filesystem rescan.
    pub fn find_best_match_no_rescan(&self, query: &TrackQuery) -> Option<MatchResult> {
        if query.is_empty() {
            return None;
        }
        if let Some(sibling) = find_sibling_lrc(query) {
            return Some(MatchResult {
                path: sibling,
                score: 1.0,
            });
        }
        self.search_indexed(query)
    }

    fn search_indexed(&self, query: &TrackQuery) -> Option<MatchResult> {
        // 1. Check if xesam:url points to a local file whose stem matches an LRC file
        let url_stem = query
            .url
            .as_deref()
            .and_then(extract_url_file_stem)
            .map(|s| normalize(&s));

        let mut best: Option<MatchResult> = None;

        for entry in &self.entries {
            let score = score_candidate(entry, query, url_stem.as_deref());
            if score >= self.match_threshold {
                let is_better = match &best {
                    None => true,
                    Some(prev) => score > prev.score,
                };
                if is_better {
                    best = Some(MatchResult {
                        path: entry.path.clone(),
                        score,
                    });
                }
            }
        }

        best
    }

    /// Resolve the destination `.lrc` sidecar file path right next to the playing song's audio file.
    /// 1. Uses `query.url` directly if it points to a local audio file on disk.
    /// 2. Otherwise searches `self.root_dir` for the matching local audio file (`.flac`, `.mp3`, `.m4a`, etc.)
    ///    whose match confidence meets `min_threshold`.
    pub fn resolve_sidecar_lrc_target(
        &self,
        query: &TrackQuery,
        min_threshold: f64,
    ) -> Option<PathBuf> {
        if let Some(audio_path) = resolve_local_audio_path(query) {
            if is_path_blacklisted(&audio_path, &self.blacklist_dirs) {
                return None;
            }
            if audio_path.file_stem().is_some() {
                return Some(audio_path.with_extension("lrc"));
            }
        }

        if !self.root_dir.exists() || query.is_empty() {
            return None;
        }

        let threshold = min_threshold.max(self.match_threshold);
        let url_stem = query
            .url
            .as_deref()
            .and_then(extract_url_file_stem)
            .map(|s| normalize(&s));

        let mut best_audio: Option<(PathBuf, f64)> = None;
        const AUDIO_EXTENSIONS: &[&str] = &[
            "flac", "mp3", "m4a", "ogg", "opus", "wav", "aac", "wv", "ape", "alac", "aiff", "dsf",
        ];

        for entry in WalkDir::new(&self.root_dir)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() || is_path_blacklisted(path, &self.blacklist_dirs) {
                continue;
            }
            let is_audio = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|ext| AUDIO_EXTENSIONS.iter().any(|&a| ext.eq_ignore_ascii_case(a)))
                .unwrap_or(false);
            if !is_audio {
                continue;
            }

            let file_stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let norm_stem = normalize(&file_stem);
            let stripped_stem = normalize(&strip_leading_track_num(&strip_decorations(&file_stem)));
            let parent_dir = path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .map(normalize)
                .unwrap_or_default();
            let grandparent_dir = path
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .map(normalize)
                .unwrap_or_default();

            let candidate = IndexedLrc {
                path: path.to_path_buf(),
                file_stem,
                norm_stem,
                stripped_stem,
                parent_dir,
                grandparent_dir,
                metadata: LrcMetadata::default(),
            };
            let score = score_candidate(&candidate, query, url_stem.as_deref());
            if score >= threshold {
                if best_audio.as_ref().map_or(true, |(_, prev_s)| score > *prev_s) {
                    best_audio = Some((path.with_extension("lrc"), score));
                }
            }
        }

        if let Some((p, _)) = best_audio {
            return Some(p);
        }

        None
    }

    /// Fallback path inside `self.root_dir` (`<Artist> - <Title>.lrc`) when `local_only_lyrics` is enabled
    /// and no matching local audio file was found on disk.
    pub fn fallback_root_lrc_path(&self, query: &TrackQuery) -> Option<PathBuf> {
        if !self.root_dir.is_dir() || query.title.trim().is_empty() {
            return None;
        }
        let cleaned = clean_video_metadata(query);
        let raw_name = if !cleaned.core_artist.is_empty() {
            format!("{} - {}", cleaned.core_artist, cleaned.core_title)
        } else {
            cleaned.core_title
        };
        let safe_stem: String = raw_name
            .chars()
            .map(|c| {
                if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let trimmed = safe_stem.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(self.root_dir.join(format!("{}.lrc", trimmed)))
    }

    /// Save fetched remote lyrics as a `.lrc` sidecar file next to the matching local audio file
    /// when its confidence score meets `save_lrc_threshold`.
    /// Will not overwrite an existing non-empty `.lrc` file unless `verify_lyrics` is true and the existing file fails verification.
    pub fn save_fetched_sidecar(
        &mut self,
        query: &TrackQuery,
        lrc: &ParsedLrc,
        save_lrc_threshold: f64,
        fallback_to_root_dir: bool,
        verify_lyrics: bool,
    ) -> Option<PathBuf> {
        if !lrc.is_strictly_synced() || lrc.match_score < save_lrc_threshold {
            return None;
        }
        let target_path = self
            .resolve_sidecar_lrc_target(query, save_lrc_threshold)
            .or_else(|| {
                if fallback_to_root_dir {
                    self.fallback_root_lrc_path(query)
                } else {
                    None
                }
            })?;
        if let Ok(meta) = std::fs::metadata(&target_path) {
            if meta.is_file() && meta.len() > 0 {
                let existing_is_valid = ParsedLrc::from_file(&target_path)
                    .map(|mut p| {
                        p.matches_track_identity(query)
                            && if verify_lyrics {
                                p.verify_and_clean()
                            } else {
                                p.is_strictly_synced()
                            }
                    })
                    .unwrap_or(false);
                if existing_is_valid {
                    return Some(target_path);
                }
            }
        }

        lrc.save_to_file(&target_path).ok()?;

        // Immediately index the newly saved sidecar `.lrc` file
        let file_stem = target_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let norm_stem = normalize(&file_stem);
        let stripped_stem = normalize(&strip_leading_track_num(&strip_decorations(&file_stem)));
        let parent_dir = target_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(normalize)
            .unwrap_or_default();
        let grandparent_dir = target_path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(normalize)
            .unwrap_or_default();

        self.entries.push(IndexedLrc {
            path: target_path.clone(),
            file_stem,
            norm_stem,
            stripped_stem,
            parent_dir,
            grandparent_dir,
            metadata: lrc.metadata.clone(),
        });

        Some(target_path)
    }

    /// Smart Identification: automatically detect whether the playing track is instrumental
    /// or non-vocal media (such as `"Instrumental"`, `"Lo-Fi"`, `"Podcast"`, `"Audiobook"`,
    /// `"Soundtrack"`, `"Ambient"`, `"DJ Mix"`).
    ///
    /// Priority order (most specific/reliable first):
    /// 1. Embedded audio file metadata tags (FLAC/OGG/OPUS Vorbis comments, MP3 ID3v2/ID3v1 tags, M4A atoms).
    /// 2. MPRIS D-Bus metadata (`xesam:genre`, `xesam:comment`, `xesam:title`, `xesam:album`).
    /// 3. Directory / playlist folder hierarchy names (e.g. `"Lo-Fi"`, `"Podcasts"`, `"Instrumental"`),
    ///    while ambiguous or generic directory names return `None` so lyrics are searched normally.
    pub fn smart_identify_track(&self, query: &TrackQuery) -> Option<SmartMediaType> {
        if query.is_empty() {
            return None;
        }

        let audio_path = resolve_local_audio_path(query)
            .or_else(|| self.find_matching_local_audio_file(query));

        // 1. Embedded audio file metadata tags (most reliable per-file signal)
        if let Some(ref path) = audio_path {
            if let Some(media_type) = inspect_audio_file_smart_media(path) {
                return Some(media_type);
            }
        }

        // 2. MPRIS genre and comment metadata
        for g in &query.genres {
            if let Some(media_type) = classify_media_metadata_tag(g) {
                return Some(media_type);
            }
        }
        for c in &query.comments {
            if let Some(media_type) = classify_media_metadata_tag(c) {
                return Some(media_type);
            }
        }

        // 3. Explicit markers in track title or album name (e.g. "(Instrumental)", "(Off Vocal)", "Podcast")
        if let Some(media_type) = classify_title_or_album_markers(&query.title, query.album.as_deref()) {
            return Some(media_type);
        }

        // 4. Directory / playlist folder names from the audio file's path
        if let Some(ref path) = audio_path {
            if let Some(media_type) = classify_path_directories(path, Some(&self.root_dir)) {
                return Some(media_type);
            }
        } else if let Some(ref raw_url) = query.url {
            let decoded = urlencoding::decode(raw_url)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| raw_url.clone());
            let clean = decoded.strip_prefix("file://").unwrap_or(&decoded);
            if let Some(media_type) = classify_path_directories(Path::new(clean), Some(&self.root_dir)) {
                return Some(media_type);
            }
        }

        None
    }

    fn find_matching_local_audio_file(&self, query: &TrackQuery) -> Option<PathBuf> {
        let sidecar = self.resolve_sidecar_lrc_target(query, self.match_threshold)?;
        let parent = sidecar.parent()?;
        let stem = sidecar.file_stem()?.to_str()?;
        const AUDIO_EXTENSIONS: &[&str] = &[
            "flac", "mp3", "m4a", "ogg", "opus", "wav", "aac", "wv", "ape", "alac", "aiff", "dsf",
        ];
        for ext in AUDIO_EXTENSIONS {
            let candidate = parent.join(format!("{}.{}", stem, ext));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
}

/// Inspect embedded audio file tags (FLAC/OGG/OPUS Vorbis comments, MP3 ID3v2/ID3v1, M4A atoms)
/// to determine if the file's metadata identifies it as instrumental or non-vocal media.
pub fn inspect_audio_file_smart_media(path: &Path) -> Option<SmartMediaType> {
    if !path.is_file() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let mut header_buf = vec![0u8; 65536];
    let n = file.read(&mut header_buf).ok()?;
    header_buf.truncate(n);

    // 1. Check Vorbis comment key=value strings (FLAC, OGG, OPUS)
    for tag_key in [
        "INSTRUMENTAL=",
        "PODCAST=",
        "GENRE=",
        "GROUPING=",
        "CONTENTGROUP=",
        "SUBTITLE=",
        "COMMENT=",
        "DESCRIPTION=",
        "MOOD=",
        "STYLE=",
    ] {
        for val in extract_ascii_key_values(&header_buf, tag_key) {
            if tag_key == "INSTRUMENTAL=" {
                let v = val.trim().to_ascii_lowercase();
                if matches!(v.as_str(), "1" | "true" | "yes" | "y") {
                    return Some(SmartMediaType::Instrumental);
                }
            } else if tag_key == "PODCAST=" {
                let v = val.trim().to_ascii_lowercase();
                if matches!(v.as_str(), "1" | "true" | "yes" | "y") {
                    return Some(SmartMediaType::Podcast);
                }
            } else if let Some(media) = classify_media_metadata_tag(&val) {
                return Some(media);
            }
        }
    }

    // 2. Check ID3v2 frames (MP3 / FLAC with ID3 wrapper / WAV)
    if header_buf.starts_with(b"ID3") && header_buf.len() >= 10 {
        for frame_id in [b"TCON", b"TIT1", b"TIT3", b"GRP1", b"TFLT", b"COMM", b"TXXX"] {
            for text in extract_id3v2_frame_texts(&header_buf, frame_id) {
                if let Some(media) = classify_media_metadata_tag(&text) {
                    return Some(media);
                }
            }
        }
    }

    // 3. Check MP4 / M4A atoms (`©gen` genre, `pcst` podcast flag)
    let ext_low = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(ext_low.as_str(), "m4a" | "mp4" | "aac" | "alac") {
        if let Some(media) = inspect_mp4_atoms(&header_buf) {
            return Some(media);
        }
    }

    // 4. Check ID3v1 tail (last 128 bytes of MP3)
    if let Ok(len) = file.metadata().map(|m| m.len()) {
        if len >= 128 && file.seek(SeekFrom::End(-128)).is_ok() {
            let mut tail = [0u8; 128];
            if file.read_exact(&mut tail).is_ok() && &tail[0..3] == b"TAG" {
                let genre_byte = tail[127];
                match genre_byte {
                    33 | 46 | 47 => return Some(SmartMediaType::Instrumental),
                    26 => return Some(SmartMediaType::Ambient),
                    183 => return Some(SmartMediaType::Audiobook),
                    186 => return Some(SmartMediaType::Podcast),
                    _ => {}
                }
            }
        }
    }

    None
}

fn extract_ascii_key_values(buf: &[u8], key_upper: &str) -> Vec<String> {
    let mut results = Vec::new();
    let key_bytes = key_upper.as_bytes();
    let klen = key_bytes.len();
    if buf.len() <= klen + 4 {
        return results;
    }

    let mut i = 4;
    while i + klen <= buf.len() {
        if buf[i..i + klen].eq_ignore_ascii_case(key_bytes) {
            // In Vorbis comments, the 4 bytes right before the key store the little-endian u32 comment length
            let comment_len = u32::from_le_bytes([buf[i - 4], buf[i - 3], buf[i - 2], buf[i - 1]]) as usize;
            let val_start = i + klen;
            if comment_len >= klen && comment_len <= 512 && i + comment_len <= buf.len() {
                let val_end = i + comment_len;
                let s = String::from_utf8_lossy(&buf[val_start..val_end]).trim().to_string();
                if !s.is_empty() {
                    results.push(s);
                }
                i = val_end;
                continue;
            }
        }
        i += 1;
    }
    results
}

fn extract_id3v2_frame_texts(buf: &[u8], frame_id: &[u8; 4]) -> Vec<String> {
    let mut out = Vec::new();
    if buf.len() < 14 {
        return out;
    }
    let mut i = 10;
    while i + 10 < buf.len() {
        if &buf[i..i + 4] == frame_id {
            let size_be = u32::from_be_bytes([buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]]) as usize;
            let syncsafe = (((buf[i + 4] as usize) & 0x7F) << 21)
                | (((buf[i + 5] as usize) & 0x7F) << 14)
                | (((buf[i + 6] as usize) & 0x7F) << 7)
                | ((buf[i + 7] as usize) & 0x7F);
            let frame_len = if size_be > 0 && i + 10 + size_be <= buf.len() {
                size_be
            } else {
                syncsafe
            };
            if frame_len > 1 && frame_len <= 1024 && i + 10 + frame_len <= buf.len() {
                let payload = &buf[i + 10..i + 10 + frame_len];
                let cleaned: String = payload
                    .iter()
                    .copied()
                    .filter(|&b| b >= 0x20 && b < 0x7F)
                    .map(char::from)
                    .collect();
                if !cleaned.trim().is_empty() {
                    out.push(cleaned.trim().to_string());
                }
            }
            i += 10 + frame_len.max(1);
        } else {
            i += 1;
        }
    }
    out
}

fn inspect_mp4_atoms(buf: &[u8]) -> Option<SmartMediaType> {
    // Look for `©gen` data atom text
    if let Some(pos) = buf.windows(4).position(|w| w == b"\xa9gen") {
        let slice = &buf[pos.. (pos + 128).min(buf.len())];
        let text: String = slice
            .iter()
            .copied()
            .filter(|&b| b >= 0x20 && b < 0x7F)
            .map(char::from)
            .collect();
        if let Some(media) = classify_media_metadata_tag(&text) {
            return Some(media);
        }
    }
    None
}

/// Classify a genre, comment, or style metadata string into a `SmartMediaType` if it unambiguously
/// represents instrumental or non-vocal media.
pub fn classify_media_metadata_tag(raw: &str) -> Option<SmartMediaType> {
    let norm = normalize(raw);
    if norm.is_empty() {
        return None;
    }
    let lower = raw.trim().to_lowercase();

    // 1. Podcast
    if matches!(
        norm.as_str(),
        "podcast" | "podcasts" | "talk show" | "talk radio" | "radio podcast"
    ) || lower.contains("podcast")
    {
        return Some(SmartMediaType::Podcast);
    }

    // 2. Audiobook / Spoken Word
    if matches!(
        norm.as_str(),
        "audiobook"
            | "audiobooks"
            | "audio book"
            | "audio books"
            | "spoken word"
            | "speech"
            | "lecture"
            | "lectures"
    ) || lower.contains("audiobook")
        || lower.contains("spoken word")
    {
        return Some(SmartMediaType::Audiobook);
    }

    // 3. Lo-Fi
    if matches!(
        norm.as_str(),
        "lo fi"
            | "lofi"
            | "chillhop"
            | "lo fi beats"
            | "lofi beats"
            | "lo fi hip hop"
            | "lofi hip hop"
            | "lofi chill"
    ) || lower.contains("lo-fi")
        || lower.contains("lofi")
        || lower.contains("chillhop")
    {
        return Some(SmartMediaType::LoFi);
    }

    // 4. Ambient / Meditation / White Noise / ASMR
    if matches!(
        norm.as_str(),
        "ambient"
            | "dark ambient"
            | "drone"
            | "soundscape"
            | "soundscapes"
            | "meditation"
            | "white noise"
            | "asmr"
            | "nature sounds"
            | "sleep music"
            | "binaural"
    ) {
        return Some(SmartMediaType::Ambient);
    }

    // 5. DJ Mix / Continuous Set
    if matches!(
        norm.as_str(),
        "dj mix" | "dj mixes" | "continuous mix" | "live set" | "boiler room"
    ) {
        return Some(SmartMediaType::DjMix);
    }

    // 6. Instrumental
    if matches!(
        norm.as_str(),
        "instrumental"
            | "instrumentals"
            | "instrumental pop"
            | "instrumental rock"
            | "instrumental hip hop"
            | "off vocal"
            | "karaoke"
            | "backing track"
            | "classical"
            | "piano solo"
            | "orchestral"
            | "symphony"
            | "chamber music"
            | "baroque"
            | "sonata"
            | "concerto"
    ) || lower.contains("instrumental")
        || lower.contains("off vocal")
        || lower.contains("纯音乐")
    {
        return Some(SmartMediaType::Instrumental);
    }

    None
}

fn classify_title_or_album_markers(title: &str, album: Option<&str>) -> Option<SmartMediaType> {
    for text in [Some(title), album].into_iter().flatten() {
        let low = text.to_lowercase();
        if low.contains("(instrumental)")
            || low.contains("[instrumental]")
            || low.contains("- instrumental")
            || low.contains("instrumental version")
            || low.contains("(off vocal)")
            || low.contains("[off vocal]")
            || low.contains("(off-vocal)")
            || low.contains("(karaoke)")
            || low.contains("[karaoke]")
            || low.contains("(inst.)")
            || low.contains("[inst.]")
            || low.contains("纯音乐")
        {
            return Some(SmartMediaType::Instrumental);
        }
        if low.contains("(podcast)") || low.contains("[podcast]") {
            return Some(SmartMediaType::Podcast);
        }
        if low.contains("(audiobook)") || low.contains("[audiobook]") {
            return Some(SmartMediaType::Audiobook);
        }
        if low.contains("(lo-fi)") || low.contains("[lo-fi]") || low.contains("(lofi)") {
            return Some(SmartMediaType::LoFi);
        }
    }
    None
}

/// Inspect directory/playlist folder names along `audio_path` (from immediate parent folder up to `root_dir`).
/// Recognizes non-lyric/instrumental folder names like `"Lo-Fi"`, `"Podcasts"`, `"Instrumental"`,
/// `"Audiobooks"`, `"Ambient"`, `"OST"`, `"DJ Mixes"`, while ignoring ambiguous or generic folder names.
pub fn classify_path_directories(audio_path: &Path, root_dir: Option<&Path>) -> Option<SmartMediaType> {
    let mut cur = audio_path.parent();
    while let Some(dir) = cur {
        let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
            break;
        };
        if let Some(media) = classify_directory_or_playlist_name(name) {
            return Some(media);
        }
        if let Some(root) = root_dir {
            if dir == root {
                break;
            }
        }
        cur = dir.parent();
    }
    None
}

pub fn classify_directory_or_playlist_name(folder_name: &str) -> Option<SmartMediaType> {
    let norm = normalize(folder_name);
    if norm.is_empty() {
        return None;
    }

    let tokens: Vec<&str> = norm.split_whitespace().collect();

    // 1. Podcast folders/playlists
    if matches!(
        norm.as_str(),
        "podcast" | "podcasts" | "talk show" | "talk shows" | "interviews"
    ) || tokens.contains(&"podcast")
        || tokens.contains(&"podcasts")
    {
        return Some(SmartMediaType::Podcast);
    }

    // 2. Audiobook / Spoken Word folders/playlists
    if matches!(
        norm.as_str(),
        "audiobook"
            | "audiobooks"
            | "audio book"
            | "audio books"
            | "spoken word"
            | "lectures"
    ) || tokens.contains(&"audiobook")
        || tokens.contains(&"audiobooks")
    {
        return Some(SmartMediaType::Audiobook);
    }

    // 3. Lo-Fi folders/playlists
    if matches!(
        norm.as_str(),
        "lo fi"
            | "lofi"
            | "chillhop"
            | "lo fi beats"
            | "lofi beats"
            | "lo fi hip hop"
            | "lofi hip hop"
            | "lofi study"
            | "lo fi study"
            | "lofi chill"
            | "lo fi chill"
    ) || tokens.contains(&"lofi")
        || tokens.contains(&"chillhop")
        || norm.starts_with("lo fi ")
        || norm.ends_with(" lo fi")
    {
        return Some(SmartMediaType::LoFi);
    }

    // 4. Instrumental folders/playlists
    if matches!(
        norm.as_str(),
        "instrumental"
            | "instrumentals"
            | "off vocal"
            | "karaoke"
            | "beats"
            | "type beats"
            | "backing tracks"
            | "classical"
            | "piano"
            | "piano solo"
            | "orchestral"
    ) || tokens.contains(&"instrumental")
        || tokens.contains(&"instrumentals")
        || tokens.contains(&"karaoke")
    {
        return Some(SmartMediaType::Instrumental);
    }

    // 5. Ambient / Sleep / White Noise / ASMR folders/playlists
    if matches!(
        norm.as_str(),
        "ambient"
            | "soundscape"
            | "soundscapes"
            | "meditation"
            | "white noise"
            | "asmr"
            | "nature sounds"
            | "sleep"
            | "sleep music"
            | "drone"
    ) || tokens.contains(&"ambient")
        || tokens.contains(&"asmr")
        || tokens.contains(&"soundscapes")
    {
        return Some(SmartMediaType::Ambient);
    }

    // 6. Soundtrack / OST / Score / BGM folders/playlists
    if matches!(
        norm.as_str(),
        "ost"
            | "osts"
            | "soundtrack"
            | "soundtracks"
            | "original soundtrack"
            | "game ost"
            | "anime ost"
            | "film score"
            | "score"
            | "bgm"
    ) || tokens.contains(&"bgm")
    {
        return Some(SmartMediaType::Soundtrack);
    }

    // 7. DJ Mix / Live Set folders/playlists
    if matches!(
        norm.as_str(),
        "dj mix" | "dj mixes" | "mixes" | "live set" | "live sets" | "boiler room"
    ) {
        return Some(SmartMediaType::DjMix);
    }

    None
}

fn score_candidate(entry: &IndexedLrc, query: &TrackQuery, url_stem: Option<&str>) -> f64 {
    let raw_title = query.title.trim();
    if raw_title.is_empty() {
        if let Some(u_stem) = url_stem {
            if !u_stem.is_empty() && entry.norm_stem == u_stem {
                return 0.99;
            }
        }
        return 0.0;
    }

    let cleaned_q = clean_video_metadata(query);
    let has_query_artist = !cleaned_q.core_artist.trim().is_empty();

    // Hard gate 1: If the LRC file has an internal [ti:...] tag, it must match the playing track's title
    if let Some(ref lrc_ti) = entry.metadata.title {
        if !lrc_ti.trim().is_empty() && title_match_score(query, lrc_ti) < 0.70 {
            return 0.0;
        }
    }

    // Hard gate 2: If the LRC file has an internal [ar:...] tag and the playing track has an artist,
    // they must match — never show another artist's lyrics for a song with the same title.
    let mut has_verified_tag_artist = false;
    if has_query_artist {
        if let Some(ref lrc_ar) = entry.metadata.artist {
            if !lrc_ar.trim().is_empty() {
                if artist_match_score(query, lrc_ar) < 0.70 {
                    return 0.0;
                }
                has_verified_tag_artist = true;
            }
        }
    }

    // Check if the LRC file lives in the exact same directory as the playing local audio file
    let in_same_audio_dir = resolve_local_audio_path(query)
        .and_then(|ap| ap.parent().map(|p| p.to_path_buf()))
        .and_then(|audio_dir| {
            entry
                .path
                .parent()
                .map(|lrc_dir| lrc_dir == audio_dir)
        })
        .unwrap_or(false);

    let mut max_score: f64 = 0.0;

    // 1. Direct URL filename stem match ONLY if in the same directory or artist is not contradicted
    if let Some(u_stem) = url_stem {
        if !u_stem.is_empty() && entry.norm_stem == u_stem && in_same_audio_dir {
            max_score = max_score.max(0.99);
        }
    }

    let norm_title = normalize(raw_title);
    let clean_title = normalize(&cleaned_q.core_title);
    let norm_artist = normalize(&cleaned_q.artist);
    let clean_artist = normalize(&cleaned_q.core_artist);
    let norm_album = query
        .album
        .as_deref()
        .map(normalize)
        .unwrap_or_default();

    // 2. Check LRC internal metadata tags ([ti:...], [ar:...]) if present
    if let Some(ref lrc_ti) = entry.metadata.title {
        let title_sim = title_match_score(query, lrc_ti);
        if title_sim >= 0.75 {
            if has_query_artist {
                if let Some(ref lrc_ar) = entry.metadata.artist {
                    let ar_sim = artist_match_score(query, lrc_ar);
                    if ar_sim >= 0.70 {
                        max_score = max_score.max(title_sim * 0.65 + ar_sim * 0.35);
                    }
                }
            } else {
                max_score = max_score.max(title_sim * 0.90);
            }
        }
    }

    // 3. Split filename (after stripping any leading track number like "01 - ") on ` - `
    //    to test explicit (artist, title) or (title, artist) parts
    let raw_stem_without_track = strip_leading_track_num(&entry.file_stem);
    let mut filename_has_artist_split = false;
    if let Some((left, right)) = split_artist_title(&raw_stem_without_track) {
        let left_clean = strip_leading_track_num(left);
        if !left_clean.is_empty() && !left_clean.chars().all(|c| c.is_ascii_digit()) {
            filename_has_artist_split = true;
            let right_title_sim = title_match_score(query, right);
            let left_artist_sim = if has_query_artist {
                artist_match_score(query, &left_clean)
            } else {
                0.80
            };

            let left_title_sim = title_match_score(query, &left_clean);
            let right_artist_sim = if has_query_artist {
                artist_match_score(query, right)
            } else {
                0.0
            };

            if right_title_sim >= 0.72 && left_artist_sim >= 0.70 {
                max_score = max_score.max(right_title_sim * 0.65 + left_artist_sim * 0.35);
            } else if left_title_sim >= 0.72 && right_artist_sim >= 0.70 {
                max_score = max_score.max(left_title_sim * 0.65 + right_artist_sim * 0.35);
            } else if has_query_artist && (right_title_sim >= 0.72 || left_title_sim >= 0.72) {
                // Filename explicitly specifies `<Other Artist> - <Same Title>.lrc` -> reject!
                return 0.0;
            }
        }
    }

    // 4. Check combined "<artist> <title>" or "<title> <artist>" against filename stem
    let stem_without_track = normalize(&raw_stem_without_track);
    if has_query_artist && !filename_has_artist_split {
        let artist_title = format!("{} {}", norm_artist, norm_title);
        let clean_artist_title = format!("{} {}", clean_artist, clean_title);
        let title_artist = format!("{} {}", norm_title, norm_artist);

        if stem_without_track == artist_title
            || entry.stripped_stem == clean_artist_title
            || stem_without_track == title_artist
        {
            max_score = max_score.max(0.98);
        } else if stem_contains_tokens(&entry.stripped_stem, &clean_artist)
            && stem_contains_tokens(&entry.stripped_stem, &clean_title)
        {
            let combo_sim = similarity(&stem_without_track, &artist_title)
                .max(similarity(&entry.stripped_stem, &clean_artist_title))
                .max(similarity(&stem_without_track, &title_artist));
            if combo_sim >= 0.82 {
                max_score = max_score.max(combo_sim * 0.95);
            }
        }
    }

    // 5. Check when filename is just the song title (e.g. `Artist/Album/01 - Title.lrc` or `Title.lrc`)
    let stem_title_sim = title_match_score(query, &strip_leading_track_num(&entry.file_stem));
    if stem_title_sim >= 0.78 && !filename_has_artist_split {
        if !has_query_artist {
            max_score = max_score.max(stem_title_sim * 0.92);
        } else {
            // When the playing track has an artist and the filename only has the song title,
            // require artist confirmation from [ar:...], directory hierarchy, or same audio folder
            let dir_artist_sim = artist_match_score(query, &entry.parent_dir)
                .max(artist_match_score(query, &entry.grandparent_dir));
            let dir_album_sim = if !norm_album.is_empty() {
                similarity(&entry.parent_dir, &norm_album)
            } else {
                0.0
            };

            // If grandparent/parent directory is named after a different artist (and not the album), reject
            if dir_artist_sim >= 0.70 {
                let score = (stem_title_sim * 0.84 + 0.14 * dir_artist_sim).min(0.97);
                max_score = max_score.max(score);
            } else if has_verified_tag_artist || in_same_audio_dir {
                max_score = max_score.max(stem_title_sim * 0.92);
            } else if dir_album_sim >= 0.85 && entry.grandparent_dir.is_empty() {
                max_score = max_score.max(stem_title_sim * 0.85);
            }
        }
    }

    max_score
}

/// Compute a strict artist match score `[0.0, 1.0]` between the playing `TrackQuery`
/// and a candidate artist string, preventing two different artists with similar names
/// (e.g. `"Dave"` vs `"Drake"`, `"Muse"` vs `"Moby"`, `"Three Days Grace"` vs `"Three Doors Down"`)
/// from matching each other's lyrics.
pub fn artist_match_score(query: &TrackQuery, cand_artist: &str) -> f64 {
    let q_artists = collect_query_artists(query);
    let c_artists = split_artist_entities(cand_artist);
    if q_artists.is_empty() || c_artists.is_empty() {
        return 0.0;
    }

    let mut best = 0.0f64;
    for (q_idx, qa) in q_artists.iter().enumerate() {
        for (c_idx, ca) in c_artists.iter().enumerate() {
            let sim = strict_entity_similarity(qa, ca);
            let weight = if q_idx == 0 && c_idx == 0 {
                1.0
            } else {
                0.94
            };
            best = best.max(sim * weight);
        }
    }
    best
}

/// Compute a strict song title match score `[0.0, 1.0]` between the playing `TrackQuery`
/// and a candidate title string.
pub fn title_match_score(query: &TrackQuery, cand_title: &str) -> f64 {
    let q_clean = clean_video_metadata(query);
    let c_stripped = strip_featuring_and_decorations(&strip_video_tags(cand_title));

    let q_core = normalize(&q_clean.core_title);
    let q_full = normalize(&q_clean.title);
    let c_core = normalize(&c_stripped);
    let c_full = normalize(cand_title);

    if (q_core.is_empty() && q_full.is_empty()) || (c_core.is_empty() && c_full.is_empty()) {
        return 0.0;
    }

    let s1 = strict_title_similarity(&q_core, &c_core);
    let s2 = strict_title_similarity(&q_full, &c_full);
    let s3 = strict_title_similarity(&q_core, &c_full) * 0.96;
    s1.max(s2).max(s3)
}

fn collect_query_artists(query: &TrackQuery) -> Vec<String> {
    let mut out = Vec::new();
    let cleaned = clean_video_metadata(query);
    for raw in [&cleaned.core_artist, &cleaned.artist] {
        for ent in split_artist_entities(raw) {
            if !ent.is_empty() && !out.contains(&ent) {
                out.push(ent);
            }
        }
    }
    for ar in &query.artists {
        if Some(ar.as_str()) == cleaned.uploader_channel.as_deref() {
            continue;
        }
        for ent in split_artist_entities(ar) {
            if !ent.is_empty() && !out.contains(&ent) {
                out.push(ent);
            }
        }
    }
    out
}

fn split_artist_entities(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let cleaned = clean_channel_suffix(&strip_decorations(raw));
    let full_norm = strip_leading_the(&normalize(&cleaned));
    if !full_norm.is_empty() {
        out.push(full_norm);
    }

    let mut working = cleaned.to_lowercase();
    for sep in [
        " feat. ", " feat ", " ft. ", " ft ", " with ", " vs. ", " vs ", " x ", " × ",
    ] {
        working = working.replace(sep, ",");
    }
    for part in working.split([',', '&', '/', ';', '、']) {
        let n = strip_leading_the(&normalize(part));
        if !n.is_empty() && !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

fn strip_leading_the(norm: &str) -> String {
    norm.strip_prefix("the ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(norm)
        .to_string()
}

fn strict_entity_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    // Check compact form without spaces (e.g. "jay z" vs "jayz")
    let a_compact: String = a.chars().filter(|c| !c.is_whitespace()).collect();
    let b_compact: String = b.chars().filter(|c| !c.is_whitespace()).collect();
    if !a_compact.is_empty() && a_compact == b_compact {
        return 0.98;
    }

    let a_tokens: Vec<&str> = a.split_whitespace().collect();
    let b_tokens: Vec<&str> = b.split_whitespace().collect();

    if a_tokens.len() == 1 && b_tokens.len() == 1 {
        return single_word_strict_similarity(a_tokens[0], b_tokens[0]);
    }

    // Multi-word artist comparison: require high token overlap
    let mut matched_a = 0usize;
    for &at in &a_tokens {
        if b_tokens
            .iter()
            .any(|&bt| single_word_strict_similarity(at, bt) >= 0.82)
        {
            matched_a += 1;
        }
    }
    let mut matched_b = 0usize;
    for &bt in &b_tokens {
        if a_tokens
            .iter()
            .any(|&at| single_word_strict_similarity(at, bt) >= 0.82)
        {
            matched_b += 1;
        }
    }

    let recall_a = matched_a as f64 / a_tokens.len() as f64;
    let recall_b = matched_b as f64 / b_tokens.len() as f64;
    let token_f1 = if recall_a + recall_b > 0.0 {
        2.0 * recall_a * recall_b / (recall_a + recall_b)
    } else {
        0.0
    };

    if token_f1 < 0.66 {
        return 0.0;
    }
    (token_f1 * 0.75 + similarity(a, b) * 0.25).clamp(0.0, 1.0)
}

fn strict_title_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    // Purely numeric titles (e.g. "200" vs "2000" or "1999") must match digits exactly
    if a.chars().all(|c| c.is_ascii_digit()) || b.chars().all(|c| c.is_ascii_digit()) {
        return if a == b { 1.0 } else { 0.0 };
    }

    let a_tokens: Vec<&str> = a.split_whitespace().collect();
    let b_tokens: Vec<&str> = b.split_whitespace().collect();

    if a_tokens.len() == 1 && b_tokens.len() == 1 {
        return single_word_strict_similarity(a_tokens[0], b_tokens[0]);
    }

    let mut matched_a = 0usize;
    for &at in &a_tokens {
        if b_tokens
            .iter()
            .any(|&bt| single_word_strict_similarity(at, bt) >= 0.80)
        {
            matched_a += 1;
        }
    }
    let mut matched_b = 0usize;
    for &bt in &b_tokens {
        if a_tokens
            .iter()
            .any(|&at| single_word_strict_similarity(at, bt) >= 0.80)
        {
            matched_b += 1;
        }
    }

    let recall_a = matched_a as f64 / a_tokens.len() as f64;
    let recall_b = matched_b as f64 / b_tokens.len() as f64;
    let token_f1 = if recall_a + recall_b > 0.0 {
        2.0 * recall_a * recall_b / (recall_a + recall_b)
    } else {
        0.0
    };

    if token_f1 < 0.60 {
        return 0.0;
    }
    (token_f1 * 0.65 + similarity(a, b) * 0.35).clamp(0.0, 1.0)
}

fn single_word_strict_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let len_a = a.chars().count();
    let len_b = b.chars().count();
    let max_len = len_a.max(len_b);
    let min_len = len_a.min(len_b);
    if min_len == 0 || max_len.saturating_sub(min_len) > 2 {
        return 0.0;
    }
    let dist = strsim::levenshtein(a, b);
    if max_len >= 5 && dist == 1 {
        0.88
    } else if max_len >= 8 && dist == 2 {
        0.80
    } else {
        0.0
    }
}

/// Normalize a string: lowercase, alphanumeric tokens separated by single spaces.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            last_was_space = false;
        } else if ch == '\'' || ch == '’' {
            // Skip apostrophes so "don't" and "dont" match identically
            continue;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Strip common track number prefixes such as `"01 - "`, `"01. "`, `"1-02 "`.
fn strip_leading_track_num(s: &str) -> String {
    let trimmed = s.trim();
    let bytes = trimmed.as_bytes();
    let mut i = 0;

    // Optional disc number e.g. `1-01`
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i <= 2 && i < bytes.len() && bytes[i] == b'-' {
        let after_dash = i + 1;
        let mut j = after_dash;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > after_dash && j - after_dash <= 3 {
            i = j;
        }
    }

    if i > 0 && i <= 5 {
        let rest = &trimmed[i..];
        let stripped = rest.trim_start_matches(['.', '-', '_', ' ', '\t']);
        if !stripped.is_empty() && stripped.len() < rest.len() {
            return stripped.to_string();
        }
    }
    trimmed.to_string()
}

/// Strip parenthesized/bracketed suffixes like `(Remastered 2011)`, `[feat. X]`, `- Remastered`.
fn strip_decorations(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;

    for ch in s.chars() {
        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            _ if paren_depth == 0 && bracket_depth == 0 => out.push(ch),
            _ => {}
        }
    }

    let base = out.trim();
    // Strip trailing ` - Remastered...` or ` feat. ...`
    let lower = base.to_ascii_lowercase();
    for marker in [
        " - remaster",
        " - single",
        " - live",
        " - radio edit",
        " feat.",
        " feat ",
        " ft.",
        " ft ",
    ] {
        if let Some(idx) = lower.find(marker) {
            if idx > 0 {
                return base[..idx].trim().to_string();
            }
        }
    }

    base.to_string()
}

fn split_artist_title(stem: &str) -> Option<(&str, &str)> {
    let s = strip_leading_track_num(stem);
    // Find ` - ` separator
    if let Some((a, b)) = stem.split_once(" - ") {
        let a_clean = strip_leading_track_num(a);
        if !a_clean.is_empty() && !b.trim().is_empty() {
            // Return slices from original stem if possible
            return Some((a.trim(), b.trim()));
        }
    }
    let _ = s;
    stem.split_once('-').and_then(|(a, b)| {
        let a = a.trim();
        let b = b.trim();
        if !a.is_empty() && !b.is_empty() && !a.chars().all(|c| c.is_ascii_digit()) {
            Some((a, b))
        } else {
            None
        }
    })
}

fn stem_contains_tokens(haystack: &str, needle: &str) -> bool {
    let h_tokens: Vec<&str> = haystack.split_whitespace().collect();
    let n_tokens: Vec<&str> = needle.split_whitespace().collect();
    if n_tokens.is_empty() {
        return false;
    }
    let matched = n_tokens.iter().filter(|t| h_tokens.contains(t)).count();
    matched * 2 >= n_tokens.len()
}

fn similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    let jw = strsim::jaro_winkler(a, b);
    let sd = strsim::sorensen_dice(a, b);
    jw * 0.55 + sd * 0.45
}

fn extract_url_file_stem(url: &str) -> Option<String> {
    let path_part = url.strip_prefix("file://").unwrap_or(url);
    let decoded = urlencoding::decode(path_part).ok()?;
    Path::new(decoded.as_ref())
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_recursive_matching_strategies() {
        let temp_dir = std::env::temp_dir().join(format!("termlrc_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let nested_a = temp_dir.join("Rock/Queen/A Night at the Opera");
        let nested_b = temp_dir.join("Synthwave");
        let nested_c = temp_dir.join("Misc");
        fs::create_dir_all(&nested_a).unwrap();
        fs::create_dir_all(&nested_b).unwrap();
        fs::create_dir_all(&nested_c).unwrap();

        let lrc_queen = nested_a.join("01 - Bohemian Rhapsody.lrc");
        fs::write(&lrc_queen, "[00:01.00]Is this the real life?").unwrap();

        let lrc_m83 = nested_b.join("M83 - Midnight City.lrc");
        fs::write(&lrc_m83, "[00:10.00]Waiting in a car").unwrap();

        let lrc_tagged = nested_c.join("track_999.lrc");
        fs::write(
            &lrc_tagged,
            "[ti:Starlight]\n[ar:Muse]\n[00:05.00]Far away\n",
        )
        .unwrap();

        let mut matcher = LrcMatcher::new(temp_dir.clone(), 0.62);
        assert_eq!(matcher.indexed_count(), 3);

        // 1. Nested Artist/Album/Track - Title match with Remastered suffix in MPRIS
        let q1 = TrackQuery {
            title: "Bohemian Rhapsody (Remastered 2011)".to_string(),
            artists: vec!["Queen".to_string()],
            album: Some("A Night at the Opera".to_string()),
            url: None,
            ..Default::default()
        };
        let m1 = matcher.find_best_match(&q1).expect("Should match Queen LRC");
        assert_eq!(m1.path, lrc_queen);

        // 2. Artist - Title filename match
        let q2 = TrackQuery {
            title: "Midnight City".to_string(),
            artists: vec!["M83".to_string()],
            album: None,
            url: None,
            ..Default::default()
        };
        let m2 = matcher.find_best_match(&q2).expect("Should match M83 LRC");
        assert_eq!(m2.path, lrc_m83);

        // 3. Metadata [ti:] / [ar:] match even when filename is opaque
        let q3 = TrackQuery {
            title: "Starlight".to_string(),
            artists: vec!["Muse".to_string()],
            album: None,
            url: None,
            ..Default::default()
        };
        let m3 = matcher
            .find_best_match(&q3)
            .expect("Should match Muse LRC by internal tags");
        assert_eq!(m3.path, lrc_tagged);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_sibling_lrc_and_video_metadata_cleanup() {
        let temp_dir =
            std::env::temp_dir().join(format!("termlrc_sibling_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).unwrap();

        let audio = temp_dir.join("song.flac");
        let sibling = temp_dir.join("song.lrc");
        fs::write(&audio, b"fake audio").unwrap();
        fs::write(&sibling, "[00:01.00]Sibling line 1\n[00:03.00]Sibling line 2").unwrap();

        let q_sib = TrackQuery {
            title: "Unrelated Title".to_string(),
            artists: vec![],
            album: None,
            url: Some(format!("file://{}", audio.display())),
            ..Default::default()
        };
        assert_eq!(find_sibling_lrc(&q_sib), Some(sibling));
        let _ = fs::remove_dir_all(&temp_dir);

        // Video metadata cleanup 1: "Crazy (feat. Yina) by chomin" from "DJMAX ARCHIVE"
        let q_djmax = TrackQuery {
            title: "Crazy (feat. Yina) by chomin".to_string(),
            artists: vec!["DJMAX ARCHIVE".to_string()],
            album: None,
            url: None,
            ..Default::default()
        };
        let cleaned = clean_video_metadata(&q_djmax);
        assert_eq!(cleaned.title, "Crazy (feat. Yina)");
        assert_eq!(cleaned.artist, "chomin");
        assert_eq!(cleaned.core_title, "Crazy");
        assert_eq!(cleaned.core_artist, "chomin");
        assert_eq!(cleaned.uploader_channel.as_deref(), Some("DJMAX ARCHIVE"));
        assert_eq!(cleaned.stripped_title_artist_query(), "Crazy chomin");

        // Video metadata cleanup 2: "Artist x Guest - Song (Official Video)"
        let q_collab = TrackQuery {
            title: "Artist x Guest - Song (Official Video)".to_string(),
            artists: vec!["Some Uploader Channel".to_string()],
            album: Some("YouTube".to_string()),
            url: None,
            ..Default::default()
        };
        let cleaned2 = clean_video_metadata(&q_collab);
        assert_eq!(cleaned2.title, "Song");
        assert_eq!(cleaned2.artist, "Artist x Guest");
        assert_eq!(cleaned2.core_title, "Song");
        assert_eq!(cleaned2.core_artist, "Artist");
        assert_eq!(cleaned2.album, None);
        assert_eq!(cleaned2.stripped_title_artist_query(), "Song Artist");

        // Sidecar saving next to local audio file
        let sidecar_dir =
            std::env::temp_dir().join(format!("termlrc_sidecar_save_{}", std::process::id()));
        let _ = fs::remove_dir_all(&sidecar_dir);
        fs::create_dir_all(&sidecar_dir).unwrap();
        let audio_file = sidecar_dir.join("Daft Punk - Digital Love.flac");
        fs::write(&audio_file, b"fake flac").unwrap();

        let mut sidecar_matcher = LrcMatcher::new(sidecar_dir.clone(), 0.62);
        let q_save = TrackQuery {
            title: "Digital Love".to_string(),
            artists: vec!["Daft Punk".to_string()],
            album: None,
            url: None,
            ..Default::default()
        };
        let mut fetched = ParsedLrc::parse("[00:12.34]Last night I had a dream about you\n[00:16.50]In this dream I'm dancing right beside you\n");
        fetched.match_score = 0.78;
        assert!(
            sidecar_matcher
                .save_fetched_sidecar(&q_save, &fetched, 0.85, false, true)
                .is_none(),
            "Should not save sidecar when match_score (0.78) < save_lrc_threshold (0.85)"
        );

        fetched.match_score = 0.92;
        fetched.metadata.source = Some("lrcmux".to_string());
        let saved_path = sidecar_matcher
            .save_fetched_sidecar(&q_save, &fetched, 0.85, false, true)
            .expect("Should save sidecar .lrc next to matching audio file when >= threshold");
        assert_eq!(saved_path, sidecar_dir.join("Daft Punk - Digital Love.lrc"));
        assert!(saved_path.is_file());
        let reloaded = ParsedLrc::from_file(&saved_path).unwrap();
        assert_eq!(reloaded.lines.len(), 2);
        assert_eq!(
            reloaded.source_label.as_deref(),
            Some("Lyrics from lrcmux")
        );

        // Directory blacklist verification
        let blacklisted_sub = sidecar_dir.join("Podcasts");
        fs::create_dir_all(&blacklisted_sub).unwrap();
        let bl_audio = blacklisted_sub.join("Episode 1.mp3");
        fs::write(&bl_audio, b"fake mp3").unwrap();
        let bl_matcher = LrcMatcher::with_blacklist(
            sidecar_dir.clone(),
            0.62,
            vec![blacklisted_sub.clone()],
        );
        let q_bl = TrackQuery {
            title: "Episode 1".to_string(),
            artists: vec!["Host".to_string()],
            album: None,
            url: Some(format!("file://{}", bl_audio.display())),
            ..Default::default()
        };
        assert!(bl_matcher.is_track_blacklisted(&q_bl));
        assert!(!bl_matcher.is_track_blacklisted(&q_save));

        let _ = fs::remove_dir_all(&sidecar_dir);
    }

    #[test]
    fn test_same_title_different_artists_never_cross_match() {
        let temp_dir =
            std::env::temp_dir().join(format!("termlrc_same_title_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let dir_muse = temp_dir.join("Muse/Black Holes and Revelations");
        let dir_dave = temp_dir.join("Dave");
        fs::create_dir_all(&dir_muse).unwrap();
        fs::create_dir_all(&dir_dave).unwrap();

        // Muse has "01 - Starlight.lrc" in Muse folder, and "Muse - Starlight.lrc" in root
        let lrc_muse_folder = dir_muse.join("01 - Starlight.lrc");
        fs::write(
            &lrc_muse_folder,
            "[ti:Starlight]\n[ar:Muse]\n[00:05.00]Far away\n[00:10.00]This ship has taken me far away\n",
        )
        .unwrap();

        let lrc_tdg = temp_dir.join("Three Days Grace - Home.lrc");
        fs::write(
            &lrc_tdg,
            "[ti:Home]\n[ar:Three Days Grace]\n[00:12.00]This house is not a home\n[00:16.00]I'd rather be painful\n",
        )
        .unwrap();

        let mut matcher = LrcMatcher::new(temp_dir.clone(), 0.62);

        // 1. Playing "Starlight" by Dave (even with a local file URL "01 - Starlight.flac" in Dave's folder!)
        // must NEVER match Muse's "01 - Starlight.lrc"
        let q_dave = TrackQuery {
            title: "Starlight".to_string(),
            artists: vec!["Dave".to_string()],
            album: None,
            url: Some(format!(
                "file://{}",
                dir_dave.join("01 - Starlight.flac").display()
            )),
            ..Default::default()
        };
        assert!(
            matcher.find_best_match(&q_dave).is_none(),
            "Dave - Starlight must not match Muse - Starlight"
        );

        // 2. Playing "Starlight" by Drake (similar 1-word name starting with D) must also not match Muse or Dave
        let q_drake = TrackQuery {
            title: "Starlight".to_string(),
            artists: vec!["Drake".to_string()],
            album: None,
            url: None,
            ..Default::default()
        };
        assert!(matcher.find_best_match(&q_drake).is_none());
        assert_eq!(artist_match_score(&q_dave, "Drake"), 0.0);

        // 3. Playing "Home" by Three Doors Down must NEVER match "Three Days Grace - Home.lrc"
        let q_tdd = TrackQuery {
            title: "Home".to_string(),
            artists: vec!["Three Doors Down".to_string()],
            album: None,
            url: None,
            ..Default::default()
        };
        assert!(
            matcher.find_best_match(&q_tdd).is_none(),
            "Three Doors Down - Home must not match Three Days Grace - Home"
        );

        // 4. Playing "Starlight" by Muse DOES match Muse's LRC
        let q_muse = TrackQuery {
            title: "Starlight".to_string(),
            artists: vec!["Muse".to_string()],
            album: None,
            url: None,
            ..Default::default()
        };
        assert_eq!(
            matcher.find_best_match(&q_muse).map(|m| m.path),
            Some(lrc_muse_folder)
        );

        // 5. Playing "Home" by Three Days Grace DOES match Three Days Grace's LRC
        let q_tdg = TrackQuery {
            title: "Home".to_string(),
            artists: vec!["Three Days Grace".to_string()],
            album: None,
            url: None,
            ..Default::default()
        };
        assert_eq!(
            matcher.find_best_match(&q_tdg).map(|m| m.path),
            Some(lrc_tdg)
        );

        // 6. Smart Identification via metadata and directory names
        let lofi_dir = temp_dir.join("Lo-Fi");
        let pod_dir = temp_dir.join("Podcasts");
        let generic_dir = temp_dir.join("My Playlist");
        fs::create_dir_all(&lofi_dir).unwrap();
        fs::create_dir_all(&pod_dir).unwrap();
        fs::create_dir_all(&generic_dir).unwrap();
        let lofi_file = lofi_dir.join("rainy_beats.flac");
        let pod_file = pod_dir.join("ep42.mp3");
        let generic_file = generic_dir.join("regular_song.flac");
        fs::write(&lofi_file, b"fake").unwrap();
        fs::write(&pod_file, b"fake").unwrap();
        fs::write(&generic_file, b"fake").unwrap();

        let q_lofi = TrackQuery {
            title: "Rainy Study".to_string(),
            artists: vec!["ChilledCow".to_string()],
            url: Some(format!("file://{}", lofi_file.display())),
            ..Default::default()
        };
        assert_eq!(
            matcher.smart_identify_track(&q_lofi),
            Some(SmartMediaType::LoFi)
        );

        let q_pod = TrackQuery {
            title: "Episode 42".to_string(),
            artists: vec!["Host".to_string()],
            url: Some(format!("file://{}", pod_file.display())),
            ..Default::default()
        };
        assert_eq!(
            matcher.smart_identify_track(&q_pod),
            Some(SmartMediaType::Podcast)
        );

        // Metadata takes priority over ambiguous directory name
        let q_meta_inst = TrackQuery {
            title: "Mystery Track".to_string(),
            artists: vec!["Artist".to_string()],
            url: Some(format!("file://{}", generic_file.display())),
            genres: vec!["Instrumental".to_string()],
            ..Default::default()
        };
        assert_eq!(
            matcher.smart_identify_track(&q_meta_inst),
            Some(SmartMediaType::Instrumental)
        );

        // Ambiguous folder + regular genre returns None so lyrics search runs normally
        let q_regular = TrackQuery {
            title: "Regular Song".to_string(),
            artists: vec!["Artist".to_string()],
            url: Some(format!("file://{}", generic_file.display())),
            genres: vec!["Rock".to_string()],
            ..Default::default()
        };
        assert_eq!(matcher.smart_identify_track(&q_regular), None);

        let _ = fs::remove_dir_all(&temp_dir);
    }
}

