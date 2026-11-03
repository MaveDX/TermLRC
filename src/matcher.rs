use crate::lrc::{LrcMetadata, ParsedLrc};
use std::path::{Path, PathBuf};
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

    /// Extract the 22-character base62 Spotify Track ID if `self.url` contains a Spotify
    /// track URL (`https://open.spotify.com/track/<id>`), URI (`spotify:track:<id>`),
    /// or MPRIS D-Bus object path (`/com/spotify/track/<id>`).
    pub fn spotify_track_id(&self) -> Option<String> {
        self.url.as_deref().and_then(extract_spotify_track_id)
    }
}

pub fn extract_spotify_track_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let candidate = if let Some(rest) = trimmed.strip_prefix("spotify:track:") {
        rest
    } else if let Some(idx) = trimmed.find("/track/") {
        &trimmed[idx + "/track/".len()..]
    } else {
        return None;
    };

    let id: String = candidate
        .split(|c: char| c == '?' || c == '/' || c == '#' || c.is_whitespace())
        .next()
        .unwrap_or("")
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();

    if id.len() == 22 {
        Some(id)
    } else {
        None
    }
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
    /// If not found in the current index, rescans the directory once in case new files were added.
    pub fn find_best_match(&mut self, query: &TrackQuery) -> Option<MatchResult> {
        if query.is_empty() {
            return None;
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
    fn test_extract_spotify_track_id() {
        assert_eq!(
            extract_spotify_track_id("https://open.spotify.com/track/4cOdK2wGLETKBW3PvgPWqT?si=123"),
            Some("4cOdK2wGLETKBW3PvgPWqT".to_string())
        );
        assert_eq!(
            extract_spotify_track_id("spotify:track:4cOdK2wGLETKBW3PvgPWqT"),
            Some("4cOdK2wGLETKBW3PvgPWqT".to_string())
        );
        assert_eq!(
            extract_spotify_track_id("/com/spotify/track/4cOdK2wGLETKBW3PvgPWqT"),
            Some("4cOdK2wGLETKBW3PvgPWqT".to_string())
        );
        assert_eq!(extract_spotify_track_id("file:///mnt/music/song.flac"), None);
    }
}

