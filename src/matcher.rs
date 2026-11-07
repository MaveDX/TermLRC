use crate::lrc::{LrcMetadata, ParsedLrc};
use std::path::{Path, PathBuf};
use std::time::Duration;
use walkdir::WalkDir;

/// Song information retrieved from MPRIS used to locate the matching LRC file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackQuery {
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub url: Option<String>,
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

/// Recursive LRC directory scanner and fuzzy/exact matcher.
#[derive(Debug, Default)]
pub struct LrcMatcher {
    root_dir: PathBuf,
    match_threshold: f64,
    entries: Vec<IndexedLrc>,
}

impl LrcMatcher {
    /// Create a new matcher pointed at `root_dir` and perform an initial recursive scan.
    pub fn new(root_dir: PathBuf, match_threshold: f64) -> Self {
        let mut matcher = Self {
            root_dir,
            match_threshold: match_threshold.clamp(0.1, 0.99),
            entries: Vec::new(),
        };
        matcher.rescan();
        matcher
    }

    pub fn update_config(&mut self, root_dir: PathBuf, match_threshold: f64) -> bool {
        self.match_threshold = match_threshold.clamp(0.1, 0.99);
        if self.root_dir != root_dir {
            self.root_dir = root_dir;
            self.rescan();
            return true;
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
            if !path.is_file() {
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
            if !path.is_file() {
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

        best_audio.map(|(p, _)| p)
    }

    /// Save fetched remote lyrics as a `.lrc` sidecar file next to the matching local audio file
    /// when its confidence score meets `save_lrc_threshold` (without overwriting an existing non-empty `.lrc` file).
    pub fn save_fetched_sidecar(
        &mut self,
        query: &TrackQuery,
        lrc: &ParsedLrc,
        save_lrc_threshold: f64,
    ) -> Option<PathBuf> {
        if lrc.lines.is_empty() || lrc.match_score < save_lrc_threshold {
            return None;
        }
        let target_path = self.resolve_sidecar_lrc_target(query, save_lrc_threshold)?;
        if let Ok(meta) = std::fs::metadata(&target_path) {
            if meta.is_file() && meta.len() > 0 {
                return Some(target_path);
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
}

fn score_candidate(entry: &IndexedLrc, query: &TrackQuery, url_stem: Option<&str>) -> f64 {
    let mut max_score: f64 = 0.0;

    // 1. Direct URL filename stem match inside the target lyrics folder
    if let Some(u_stem) = url_stem {
        if !u_stem.is_empty() && entry.norm_stem == u_stem {
            max_score = max_score.max(0.99);
        }
    }

    let raw_title = query.title.trim();
    if raw_title.is_empty() {
        return max_score;
    }

    let norm_title = normalize(raw_title);
    let clean_title = normalize(&strip_decorations(raw_title));
    let norm_artist = query
        .primary_artist()
        .map(normalize)
        .unwrap_or_default();
    let clean_artist = query
        .primary_artist()
        .map(|a| normalize(&strip_decorations(a)))
        .unwrap_or_default();
    let norm_album = query
        .album
        .as_deref()
        .map(normalize)
        .unwrap_or_default();

    // 2. Check LRC internal metadata tags ([ti:...], [ar:...]) if present
    if let Some(ref lrc_ti) = entry.metadata.title {
        let norm_lrc_ti = normalize(lrc_ti);
        let clean_lrc_ti = normalize(&strip_decorations(lrc_ti));
        let title_sim = similarity(&norm_title, &norm_lrc_ti)
            .max(similarity(&clean_title, &clean_lrc_ti));

        if title_sim > 0.80 {
            if !norm_artist.is_empty() {
                if let Some(ref lrc_ar) = entry.metadata.artist {
                    let norm_lrc_ar = normalize(lrc_ar);
                    let ar_sim = similarity(&norm_artist, &norm_lrc_ar)
                        .max(similarity(&clean_artist, &norm_lrc_ar));
                    if ar_sim > 0.75 {
                        max_score = max_score.max(title_sim * 0.7 + ar_sim * 0.3);
                    }
                } else {
                    max_score = max_score.max(title_sim * 0.85);
                }
            } else {
                max_score = max_score.max(title_sim * 0.90);
            }
        }
    }

    // 3. Check combined "<artist> - <title>" or "<title> - <artist>" against filename stem
    let stem_without_track = normalize(&strip_leading_track_num(&entry.file_stem));

    if !norm_artist.is_empty() {
        let artist_title = format!("{} {}", norm_artist, norm_title);
        let clean_artist_title = format!("{} {}", clean_artist, clean_title);
        let title_artist = format!("{} {}", norm_title, norm_artist);

        let combo_sim = similarity(&stem_without_track, &artist_title)
            .max(similarity(&entry.stripped_stem, &clean_artist_title))
            .max(similarity(&stem_without_track, &title_artist));

        // Exact normalized match on "artist - title"
        if stem_without_track == artist_title || entry.stripped_stem == clean_artist_title {
            max_score = max_score.max(0.98);
        } else if combo_sim > 0.82 {
            // Ensure the title portion itself actually matches well
            if stem_contains_tokens(&entry.stripped_stem, &clean_title) {
                max_score = max_score.max(combo_sim * 0.95);
            } else {
                max_score = max_score.max(combo_sim * 0.80);
            }
        }
    }

    // 4. Check when filename is just the song title (e.g. `Artist/Album/01 - Title.lrc` or `Title.lrc`)
    let stem_title_sim = similarity(&stem_without_track, &norm_title)
        .max(similarity(&entry.stripped_stem, &clean_title));

    if stem_title_sim > 0.78 {
        let mut dir_bonus = 0.0;
        if !norm_artist.is_empty() {
            let parent_ar = similarity(&entry.parent_dir, &norm_artist)
                .max(similarity(&entry.grandparent_dir, &norm_artist));
            if parent_ar > 0.80 {
                dir_bonus += 0.12 * parent_ar;
            }
        }
        if !norm_album.is_empty() {
            let parent_al = similarity(&entry.parent_dir, &norm_album);
            if parent_al > 0.80 {
                dir_bonus += 0.06 * parent_al;
            }
        }

        // If there is no artist in the filename, scale base title match slightly below a full artist+title match
        // unless parent folders confirm the artist/album.
        let base = if norm_artist.is_empty() {
            stem_title_sim * 0.92
        } else {
            stem_title_sim * 0.82
        };
        max_score = max_score.max((base + dir_bonus).min(0.97));
    }

    // 5. Split filename on ` - ` to test explicit (artist, title) parts
    if let Some((left, right)) = split_artist_title(&entry.file_stem) {
        let norm_left = normalize(&strip_decorations(left));
        let norm_right = normalize(&strip_decorations(right));

        let right_title_sim = similarity(&norm_right, &clean_title).max(similarity(&normalize(right), &norm_title));
        let left_artist_sim = if !clean_artist.is_empty() {
            similarity(&norm_left, &clean_artist).max(similarity(&normalize(left), &norm_artist))
        } else {
            0.8
        };

        if right_title_sim > 0.75 && left_artist_sim > 0.70 {
            max_score = max_score.max(right_title_sim * 0.68 + left_artist_sim * 0.32);
        }

        // Also check reversed `Title - Artist.lrc`
        let left_title_sim = similarity(&norm_left, &clean_title);
        let right_artist_sim = if !clean_artist.is_empty() {
            similarity(&norm_right, &clean_artist)
        } else {
            0.0
        };
        if left_title_sim > 0.75 && right_artist_sim > 0.75 {
            max_score = max_score.max(left_title_sim * 0.65 + right_artist_sim * 0.30);
        }
    }

    max_score
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
        };
        let m1 = matcher.find_best_match(&q1).expect("Should match Queen LRC");
        assert_eq!(m1.path, lrc_queen);

        // 2. Artist - Title filename match
        let q2 = TrackQuery {
            title: "Midnight City".to_string(),
            artists: vec!["M83".to_string()],
            album: None,
            url: None,
        };
        let m2 = matcher.find_best_match(&q2).expect("Should match M83 LRC");
        assert_eq!(m2.path, lrc_m83);

        // 3. Metadata [ti:] / [ar:] match even when filename is opaque
        let q3 = TrackQuery {
            title: "Starlight".to_string(),
            artists: vec!["Muse".to_string()],
            album: None,
            url: None,
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
        };
        assert_eq!(find_sibling_lrc(&q_sib), Some(sibling));
        let _ = fs::remove_dir_all(&temp_dir);

        // Video metadata cleanup 1: "Crazy (feat. Yina) by chomin" from "DJMAX ARCHIVE"
        let q_djmax = TrackQuery {
            title: "Crazy (feat. Yina) by chomin".to_string(),
            artists: vec!["DJMAX ARCHIVE".to_string()],
            album: None,
            url: None,
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
        };
        let mut fetched = ParsedLrc::parse("[00:12.34]Last night I had a dream about you\n[00:16.50]In this dream I'm dancing right beside you\n");
        fetched.match_score = 0.78;
        assert!(
            sidecar_matcher
                .save_fetched_sidecar(&q_save, &fetched, 0.85)
                .is_none(),
            "Should not save sidecar when match_score (0.78) < save_lrc_threshold (0.85)"
        );

        fetched.match_score = 0.92;
        let saved_path = sidecar_matcher
            .save_fetched_sidecar(&q_save, &fetched, 0.85)
            .expect("Should save sidecar .lrc next to matching audio file when >= threshold");
        assert_eq!(saved_path, sidecar_dir.join("Daft Punk - Digital Love.lrc"));
        assert!(saved_path.is_file());
        let reloaded = ParsedLrc::from_file(&saved_path).unwrap();
        assert_eq!(reloaded.lines.len(), 2);
        let _ = fs::remove_dir_all(&sidecar_dir);
    }
}

