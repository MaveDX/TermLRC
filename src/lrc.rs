use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

fn default_true() -> bool {
    true
}

fn default_match_score() -> f64 {
    1.0
}

/// Metadata extracted from LRC header tags (`[ti:]`, `[ar:]`, `[al:]`, `[src:]`, `[offset:]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LrcMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub source: Option<String>,
    /// Offset in milliseconds from `[offset:+/-ms]` tag.
    pub offset_ms: i64,
}

/// Optional word/segment timestamp inside an Enhanced LRC line (`<mm:ss.xx>text`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedSegment {
    pub timestamp: Duration,
    pub text: String,
}

/// A single timestamped lyric line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub timestamp: Duration,
    pub text: String,
    pub segments: Vec<TimedSegment>,
}

/// Parsed LRC file containing metadata and sorted synced (or plain-text fallback) lyric lines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedLrc {
    pub metadata: LrcMetadata,
    pub lines: Vec<LyricLine>,
    /// Optional banner displayed at the top of the lyric sheet when fetched from a fallback provider
    /// (e.g. `Some("Lyrics from lrcmux")`).
    pub source_label: Option<String>,
    /// Whether these lyrics have real line timestamps (`true`) or are unsynced plain text (`false`).
    #[serde(default = "default_true")]
    pub is_synced: bool,
    /// Confidence score `[0.0, 1.0]` of the remote or local match.
    #[serde(default = "default_match_score")]
    pub match_score: f64,
}

impl Default for ParsedLrc {
    fn default() -> Self {
        Self {
            metadata: LrcMetadata::default(),
            lines: Vec::new(),
            source_label: None,
            is_synced: true,
            match_score: 1.0,
        }
    }
}

/// State of the lyric playback at a specific song position.
#[derive(Debug, Clone, Copy)]
pub struct LyricSyncState {
    /// Index of the currently active lyric line, if playback has reached the first line.
    pub active_index: Option<usize>,
    /// Fractional progress `[0.0, 1.0]` through the active line's duration until the next line.
    #[allow(dead_code)]
    pub line_progress: f64,
    /// Continuous floating-point line index (e.g. `3.42` means 42% between line 3 and line 4).
    pub continuous_index: f64,
    /// Character-level progress `[0.0, 1.0]` for karaoke highlight (uses `<mm:ss.xx>` segments if available).
    pub karaoke_progress: f64,
}

impl ParsedLrc {
    /// Parse an LRC file from disk (supports both synced LRC and plain-text `.lrc` sidecar files).
    pub fn from_file(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)
            .with_context(|| format!("Failed to read LRC file: {}", path.display()))?;
        let content = String::from_utf8_lossy(&bytes);
        let parsed = Self::parse(&content);
        let mut out = if parsed.lines.len() >= 2 {
            parsed
        } else {
            let mut plain = Self::from_plain_text(&content);
            plain.metadata = parsed.metadata;
            plain
        };
        out.source_label = Some(match out.metadata.source.as_deref().map(str::trim) {
            Some(src) if !src.is_empty() => {
                if src.starts_with("Lyrics from ") {
                    src.to_string()
                } else {
                    format!("Lyrics from {}", src)
                }
            }
            _ => String::from("Lyrics from Local LRC"),
        });
        Ok(out)
    }

    /// Read only the metadata headers from an LRC file quickly for indexing/matching
    /// using a fixed stack buffer without allocating the entire file on the heap.
    pub fn read_metadata_only(path: &Path) -> Result<LrcMetadata> {
        let mut file = File::open(path)?;
        let mut buf = [0u8; 1024];
        let n = file.read(&mut buf)?;
        let content = String::from_utf8_lossy(&buf[..n]);
        let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
        let mut meta = LrcMetadata::default();
        for raw_line in content.lines().take(25) {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            parse_metadata_tag(line, &mut meta);
        }
        Ok(meta)
    }

    /// Parse LRC content string into metadata and sorted lyric lines.
    pub fn parse(content: &str) -> Self {
        let content = content.strip_prefix('\u{feff}').unwrap_or(content);
        let mut metadata = LrcMetadata::default();
        let mut lines = Vec::new();
        let mut untimed_text_lines = 0usize;

        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }

            let (timestamps, remainder) = extract_line_timestamps(line);
            if timestamps.is_empty() {
                if line.starts_with('[') && line.ends_with(']') && line.contains(':') {
                    parse_metadata_tag(line, &mut metadata);
                } else {
                    untimed_text_lines += 1;
                }
                continue;
            }

            let (clean_text, segments) = parse_enhanced_segments(remainder);
            for ts in timestamps {
                lines.push(LyricLine {
                    timestamp: ts,
                    text: clean_text.clone(),
                    segments: segments.clone(),
                });
            }
        }

        lines.sort_by_key(|l| l.timestamp);

        // If untimed plain-text lines outnumber timed lines, this is not a properly synced LRC
        let is_synced = !lines.is_empty() && untimed_text_lines <= lines.len() / 2;

        Self {
            metadata,
            lines,
            source_label: None,
            is_synced,
            match_score: 1.0,
        }
    }

    /// Parse unsynced plain-text lyrics (used only when nothing is synced).
    pub fn from_plain_text(content: &str) -> Self {
        let content = content.strip_prefix('\u{feff}').unwrap_or(content);
        let mut metadata = LrcMetadata::default();
        let mut lines = Vec::new();
        for raw_line in content.lines() {
            let trimmed = raw_line.trim();
            if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.contains(':') {
                parse_metadata_tag(trimmed, &mut metadata);
                continue;
            }
            let (_, remainder) = extract_line_timestamps(trimmed);
            let (clean_text, _) = parse_enhanced_segments(remainder);
            let clean_trimmed = clean_text.trim();
            if clean_trimmed.is_empty() {
                continue;
            }
            lines.push(LyricLine {
                timestamp: Duration::ZERO,
                text: clean_trimmed.to_string(),
                segments: Vec::new(),
            });
        }
        Self {
            metadata,
            lines,
            source_label: None,
            is_synced: false,
            match_score: 1.0,
        }
    }

    /// Format this `ParsedLrc` back into standard `.lrc` file text (preserving Enhanced LRC word tags if present).
    pub fn to_lrc_string(&self) -> String {
        let mut out = String::new();
        if let Some(ref ti) = self.metadata.title {
            if !ti.trim().is_empty() {
                out.push_str(&format!("[ti:{}]\n", ti.trim()));
            }
        }
        if let Some(ref ar) = self.metadata.artist {
            if !ar.trim().is_empty() {
                out.push_str(&format!("[ar:{}]\n", ar.trim()));
            }
        }
        if let Some(ref al) = self.metadata.album {
            if !al.trim().is_empty() {
                out.push_str(&format!("[al:{}]\n", al.trim()));
            }
        }
        let persisted_source = self
            .metadata
            .source
            .as_deref()
            .or(self.source_label.as_deref())
            .map(|s| s.trim().strip_prefix("Lyrics from ").unwrap_or(s.trim()))
            .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("Local LRC"));
        if let Some(src) = persisted_source {
            out.push_str(&format!("[src:{}]\n", src));
        }
        if self.metadata.offset_ms != 0 {
            out.push_str(&format!("[offset:{}]\n", self.metadata.offset_ms));
        }

        for line in &self.lines {
            if self.is_synced {
                out.push('[');
                out.push_str(&format_lrc_timestamp(line.timestamp));
                out.push(']');
                if !line.segments.is_empty() {
                    for (idx, seg) in line.segments.iter().enumerate() {
                        out.push('<');
                        out.push_str(&format_lrc_timestamp(seg.timestamp));
                        out.push('>');
                        out.push_str(&seg.text);
                        if idx + 1 < line.segments.len()
                            && !seg.text.ends_with(' ')
                            && !line.segments[idx + 1].text.starts_with(' ')
                        {
                            out.push(' ');
                        }
                    }
                } else {
                    out.push_str(&line.text);
                }
                out.push('\n');
            } else {
                out.push_str(&line.text);
                out.push('\n');
            }
        }
        out
    }

    /// Atomically write this `ParsedLrc` to a `.lrc` sidecar file on disk.
    /// Refuses to write unsynced or invalid lyrics so `.lrc` files on disk are always genuinely synced.
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        if !self.is_strictly_synced() {
            anyhow::bail!("Refusing to save unsynced lyrics as an LRC file");
        }
        let content = self.to_lrc_string();
        let tmp_path = path.with_extension("lrc.tmp");
        if fs::write(&tmp_path, content.as_bytes()).is_ok() {
            if fs::rename(&tmp_path, path).is_ok() {
                return Ok(());
            }
            let _ = fs::remove_file(&tmp_path);
        }
        fs::write(path, content.as_bytes())
            .with_context(|| format!("Failed to write sidecar LRC file: {}", path.display()))
    }

    /// Compute the active line and smooth fractional progress for a given playback position.
    ///
    /// `user_offset_ms`: positive values delay lyrics (shift timestamps later),
    /// negative values advance lyrics (shift timestamps earlier).
    pub fn sync_state(&self, position: Duration, user_offset_ms: i64) -> LyricSyncState {
        if self.lines.is_empty() || !self.is_synced {
            return LyricSyncState {
                active_index: None,
                line_progress: 0.0,
                continuous_index: 0.0,
                karaoke_progress: 0.0,
            };
        }

        // According to LRC spec, [offset:+/-ms] shifts lyric display time.
        // Effective song position relative to lyric timestamps:
        let total_offset_ms = self.metadata.offset_ms.saturating_sub(user_offset_ms);
        let pos_ms = (position.as_millis() as i64 + total_offset_ms).max(0) as u64;
        let effective_pos = Duration::from_millis(pos_ms);

        // Find the last line whose timestamp <= effective_pos
        let idx = match self
            .lines
            .binary_search_by_key(&effective_pos, |l| l.timestamp)
        {
            Ok(mut i) => {
                while i + 1 < self.lines.len() && self.lines[i + 1].timestamp <= effective_pos {
                    i += 1;
                }
                Some(i)
            }
            Err(0) => None,
            Err(insert_point) => Some(insert_point - 1),
        };

        match idx {
            None => {
                // Before the first lyric line
                let first_ts = self.lines[0].timestamp.as_secs_f64();
                let cur = effective_pos.as_secs_f64();
                let pre_progress = if first_ts > 0.0 {
                    (cur / first_ts).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                LyricSyncState {
                    active_index: None,
                    line_progress: pre_progress,
                    continuous_index: (pre_progress - 1.0).clamp(-1.0, 0.0),
                    karaoke_progress: 0.0,
                }
            }
            Some(i) => {
                let current_line = &self.lines[i];
                let start_s = current_line.timestamp.as_secs_f64();
                let end_s = if i + 1 < self.lines.len() {
                    self.lines[i + 1].timestamp.as_secs_f64()
                } else {
                    // Default 5 second tail duration for the final lyric line
                    start_s + 5.0
                };

                let duration = (end_s - start_s).max(0.05);
                let elapsed = (effective_pos.as_secs_f64() - start_s).max(0.0);
                let line_progress = (elapsed / duration).clamp(0.0, 1.0);

                // For karaoke highlight, finish sweeping slightly before the line ends
                // or use enhanced LRC word segments if available.
                let karaoke_progress =
                    compute_karaoke_progress(current_line, effective_pos, end_s, line_progress);

                LyricSyncState {
                    active_index: Some(i),
                    line_progress,
                    continuous_index: i as f64 + line_progress,
                    karaoke_progress,
                }
            }
        }
    }

    /// Validate that this `ParsedLrc` has real, progressing line timestamps across the song
    /// (rejecting plain text, single-timestamp dumps, or fake `[00:00.00]`-only files).
    pub fn is_strictly_synced(&self) -> bool {
        if !self.is_synced || self.lines.len() < 2 {
            return false;
        }

        let mut non_empty_timestamps: Vec<Duration> = self
            .lines
            .iter()
            .filter(|l| !l.text.trim().is_empty())
            .map(|l| l.timestamp)
            .collect();

        if non_empty_timestamps.len() < 2 {
            return false;
        }

        non_empty_timestamps.sort();
        let min_ts = non_empty_timestamps[0];
        let max_ts = *non_empty_timestamps.last().unwrap();

        // Must have timestamps progressing past 00:01.00 and spanning at least 1.5 seconds
        if max_ts <= Duration::from_millis(1000)
            || max_ts.saturating_sub(min_ts) < Duration::from_millis(1500)
        {
            return false;
        }

        let total_non_empty = non_empty_timestamps.len();
        non_empty_timestamps.dedup();
        let distinct_count = non_empty_timestamps.len();

        // Require at least 2 distinct timestamps, and for longer lyrics at least 40% distinct timestamps
        // so files where almost every line is stamped with the same dummy timestamp are rejected.
        if distinct_count < 2 || (total_non_empty >= 6 && distinct_count * 5 < total_non_empty * 2)
        {
            return false;
        }

        true
    }

    /// Verify that this `ParsedLrc` is genuinely time-synced and contains real lyrics rather than
    /// unsynced plain text, Chinese/CJK artist credits, production metadata rosters
    /// (e.g. `作曲 : ...`, `作词 : ...`), or `"纯音乐，请欣赏"` placeholders.
    ///
    /// Strips credit/metadata-only lines in-place and returns `true` only if valid synced lyrics remain.
    pub fn verify_and_clean(&mut self) -> bool {
        for line in &mut self.lines {
            let (_, stripped) = extract_line_timestamps(line.text.trim());
            if stripped.len() != line.text.len() {
                line.text = stripped.to_string();
            }
        }

        self.lines
            .retain(|line| !is_credit_or_non_lyric_line(&line.text));

        if !self.is_strictly_synced() {
            self.is_synced = false;
            return false;
        }

        true
    }

    /// Verify that this `ParsedLrc`'s internal `[ti:...]` and `[ar:...]` headers (if present)
    /// match the playing track's title and artist, preventing a song with the same title
    /// by a different artist from displaying the wrong lyrics.
    pub fn matches_track_identity(&self, query: &crate::matcher::TrackQuery) -> bool {
        if let Some(ref lrc_ti) = self.metadata.title {
            if !lrc_ti.trim().is_empty() && !query.title.trim().is_empty() {
                if crate::matcher::title_match_score(query, lrc_ti) < 0.70 {
                    return false;
                }
            }
        }
        let cleaned = crate::matcher::clean_video_metadata(query);
        if !cleaned.core_artist.trim().is_empty() {
            if let Some(ref lrc_ar) = self.metadata.artist {
                if !lrc_ar.trim().is_empty() {
                    if crate::matcher::artist_match_score(query, lrc_ar) < 0.70 {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// Detect whether a single lyric line is actually a Chinese/CJK/English production credit,
/// artist roster (e.g. `作曲 : Oliver Sykes/Lee Malia/...`, `作词 : ...`), or instrumental placeholder.
pub fn is_credit_or_non_lyric_line(raw_text: &str) -> bool {
    let (_, after_ts) = extract_line_timestamps(raw_text.trim());
    let text = after_ts.trim();
    if text.is_empty() {
        return false;
    }

    let lower = text.to_lowercase();

    // 1. Instrumental / "No lyrics" placeholder sentences common on NetEase / KuGou / LRCLIB
    const INSTRUMENTAL_PLACEHOLDERS: &[&str] = &[
        "纯音乐，请欣赏",
        "纯音乐,请欣赏",
        "纯音乐 请欣赏",
        "此歌曲为没有填词的纯音乐",
        "没有填词的纯音乐",
        "纯音乐无歌词",
        "暂无歌词",
        "没有歌词",
        "无歌词",
        "纯音乐",
    ];
    for &ph in INSTRUMENTAL_PLACEHOLDERS {
        if lower == ph || lower.contains("纯音乐，请欣赏") || lower.contains("没有填词的纯音乐")
        {
            return true;
        }
    }

    // 2. Credit prefix followed by ':' or '：' (e.g. "作曲 : ...", "作词：...", "编曲 : ...", "词/曲：...")
    if let Some((left, right)) = text.split_once([':', '：']) {
        let role = left
            .trim()
            .trim_start_matches(|c: char| matches!(c, '(' | '[' | '【' | '《' | '-' | '*' | ' '))
            .trim_end_matches(|c: char| matches!(c, ')' | ']' | '】' | '》' | ' '));

        if !role.is_empty() && role.chars().count() <= 24 {
            if is_credit_role_label(role) {
                return true;
            }
            // Also check compound roles separated by '/', '&', '、', ',' (e.g. "作词/作曲", "混音/母带")
            let parts: Vec<&str> = role
                .split(['/', '&', '、', ',', '，', '+'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if !parts.is_empty() && parts.iter().all(|p| is_credit_role_label(p)) {
                return true;
            }
        }

        // If the right side after a colon is a slash-separated list of 3+ credit names (e.g. "A/B/C/D")
        let slash_count = right.chars().filter(|&c| c == '/').count();
        if slash_count >= 3 && role.chars().count() <= 16 {
            return true;
        }
    }

    // 3. Standalone slash-separated credit roster with 4+ slashes (e.g. "Oliver Sykes/Lee Malia/Matthew Nicholls/Dan Lancaster/Zakk Cervini")
    let slash_segments: Vec<&str> = text.split('/').map(str::trim).filter(|s| !s.is_empty()).collect();
    if slash_segments.len() >= 5
        && slash_segments
            .iter()
            .all(|seg| seg.chars().count() <= 35 && !seg.contains(['?', '!', '。', '，']))
    {
        return true;
    }

    false
}

fn is_credit_role_label(label: &str) -> bool {
    let clean = label.trim().to_lowercase();
    const CREDIT_ROLES: &[&str] = &[
        "作曲",
        "作词",
        "作詞",
        "编曲",
        "編曲",
        "制作人",
        "製作人",
        "制作",
        "製作",
        "词曲",
        "詞曲",
        "词",
        "詞",
        "曲",
        "编",
        "編",
        "混音",
        "混音师",
        "母带",
        "母帶",
        "母带后期",
        "母带工程师",
        "录音",
        "錄音",
        "录音师",
        "录音室",
        "录音棚",
        "缩混",
        "混缩",
        "后期",
        "吉他",
        "木吉他",
        "电吉他",
        "贝斯",
        "鼓",
        "打击乐",
        "和声",
        "和聲",
        "和音",
        "和声编写",
        "弦乐",
        "弦樂",
        "弦乐编写",
        "管乐",
        "键盘",
        "钢琴",
        "合成器",
        "乐器",
        "配唱",
        "配唱制作人",
        "人声",
        "人聲",
        "人声编辑",
        "音频编辑",
        "统筹",
        "音乐统筹",
        "监制",
        "監製",
        "总策划",
        "策划",
        "企划",
        "企劃",
        "出品",
        "出品人",
        "出品方",
        "出品公司",
        "发行",
        "發行",
        "发行方",
        "发行公司",
        "宣发",
        "宣传",
        "推广",
        "营销",
        "封面",
        "封面设计",
        "美工",
        "视觉",
        "海报",
        "文案",
        "设计",
        "题字",
        "曲绘",
        "调校",
        "视频",
        "映像",
        "原唱",
        "翻唱",
        "演唱",
        "歌手",
        "艺人",
        "合唱",
        "独唱",
        "伴奏",
        "制作团队",
        "音乐制作",
        "版权",
        "版权方",
        "鸣谢",
        "特别鸣谢",
        "op",
        "sp",
        "tw",
        "pv",
        "cv",
        "composer",
        "composed by",
        "lyricist",
        "lyrics",
        "lyrics by",
        "written by",
        "arranger",
        "arranged by",
        "arrangement",
        "producer",
        "produced by",
        "production",
        "mixed by",
        "mixing",
        "mastered by",
        "mastering",
        "recorded by",
        "recording",
        "vocals",
        "vocals by",
        "guitar",
        "bass",
        "drums",
        "strings",
        "published by",
        "publisher",
    ];
    CREDIT_ROLES.contains(&clean.as_str())
}

fn compute_karaoke_progress(
    line: &LyricLine,
    effective_pos: Duration,
    line_end_s: f64,
    fallback_line_progress: f64,
) -> f64 {
    if line.text.is_empty() {
        return 1.0;
    }

    if line.segments.is_empty() {
        // Complete the horizontal sweep over 85% of the interval (capped to 4.5s max sweep)
        // so long instrumental tails after a line don't make the text crawl unnaturally slowly.
        let line_start_s = line.timestamp.as_secs_f64();
        let raw_dur = (line_end_s - line_start_s).max(0.1);
        let sweep_dur = (raw_dur * 0.85).min(4.5).max(0.15);
        let elapsed = (effective_pos.as_secs_f64() - line_start_s).max(0.0);
        return (elapsed / sweep_dur).clamp(0.0, 1.0);
    }

    let total_chars: usize = line.segments.iter().map(|s| s.text.chars().count()).sum();
    if total_chars == 0 {
        return fallback_line_progress;
    }

    let pos_s = effective_pos.as_secs_f64();
    let mut completed_chars = 0usize;

    for (idx, seg) in line.segments.iter().enumerate() {
        let seg_chars = seg.text.chars().count();
        let seg_start = seg.timestamp.as_secs_f64();
        let seg_end = if idx + 1 < line.segments.len() {
            line.segments[idx + 1].timestamp.as_secs_f64()
        } else {
            line_end_s
        };

        if pos_s >= seg_end {
            completed_chars += seg_chars;
        } else if pos_s > seg_start {
            let seg_dur = (seg_end - seg_start).max(0.01);
            let frac = ((pos_s - seg_start) / seg_dur).clamp(0.0, 1.0);
            let total_f = completed_chars as f64 + frac * (seg_chars as f64);
            return (total_f / total_chars as f64).clamp(0.0, 1.0);
        } else {
            break;
        }
    }

    (completed_chars as f64 / total_chars as f64).clamp(0.0, 1.0)
}

/// Extract one or more leading `[mm:ss.xx]` timestamps from a line, returning `(timestamps, rest)`.
fn extract_line_timestamps(mut input: &str) -> (Vec<Duration>, &str) {
    let mut timestamps = Vec::new();
    while let Some(rest_after_open) = input.strip_prefix('[') {
        if let Some(close_idx) = rest_after_open.find(']') {
            let inside = &rest_after_open[..close_idx];
            if let Some(ts) = parse_timestamp(inside) {
                timestamps.push(ts);
                input = &rest_after_open[close_idx + 1..];
                continue;
            }
        }
        break;
    }
    (timestamps, input.trim())
}

/// Format a `Duration` as `mm:ss.xx` for standard LRC output.
fn format_lrc_timestamp(d: Duration) -> String {
    let total_cs = (d.as_millis() + 5) / 10;
    let mins = total_cs / 6000;
    let secs = (total_cs % 6000) / 100;
    let cs = total_cs % 100;
    format!("{:02}:{:02}.{:02}", mins, secs, cs)
}

/// Parse a timestamp string like `01:23.45`, `01:23.456`, `01:23.45-1`, `01:23:45`, or `01:23`.
fn parse_timestamp(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (mins_str, rest) = s.split_once(':')?;
    let mins: u64 = mins_str.trim().parse().ok()?;

    // Strip any trailing NetEase/YRC line-index suffix like `-1` or `-0` (e.g. `00:00.00-1`)
    let rest_clean = match rest.split_once('-') {
        Some((before_dash, after_dash))
            if !after_dash.trim().is_empty()
                && after_dash.trim().chars().all(|c| c.is_ascii_digit()) =>
        {
            before_dash
        }
        _ => rest,
    };

    let (secs, nanos) = if let Some((sec_str, frac_str)) = rest_clean.split_once(['.', ':']) {
        let secs: u64 = sec_str.trim().parse().ok()?;
        let frac_trimmed = frac_str.trim();
        if frac_trimmed.is_empty() || !frac_trimmed.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let frac_val: u64 = frac_trimmed.parse().ok()?;
        let nanos = match frac_trimmed.len() {
            1 => frac_val * 100_000_000,
            2 => frac_val * 10_000_000,
            3 => frac_val * 1_000_000,
            _ => {
                let scale = 10u64.pow(frac_trimmed.len() as u32);
                (frac_val * 1_000_000_000) / scale
            }
        };
        (secs, nanos as u32)
    } else {
        let secs: u64 = rest_clean.trim().parse().ok()?;
        (secs, 0)
    };

    Some(Duration::new(mins * 60 + secs, nanos))
}

/// Parse metadata tags like `[ti:Song Title]`, `[ar:Artist]`, `[al:Album]`, `[src:Provider]`, `[offset:+200]`.
fn parse_metadata_tag(line: &str, meta: &mut LrcMetadata) {
    let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return;
    };
    let Some((key, val)) = inner.split_once(':') else {
        return;
    };
    let key = key.trim().to_ascii_lowercase();
    let val = val.trim();
    if val.is_empty() {
        return;
    }
    match key.as_str() {
        "ti" | "title" => meta.title = Some(val.to_string()),
        "ar" | "artist" => meta.artist = Some(val.to_string()),
        "al" | "album" => meta.album = Some(val.to_string()),
        "src" | "source" => meta.source = Some(val.to_string()),
        "offset" => {
            if let Ok(offset) = val.parse::<i64>() {
                meta.offset_ms = offset;
            }
        }
        _ => {}
    }
}

/// Parse Enhanced LRC inline word timestamps (`<00:12.34>word`) if present.
fn parse_enhanced_segments(text: &str) -> (String, Vec<TimedSegment>) {
    if !text.contains('<') || !text.contains('>') {
        return (text.to_string(), Vec::new());
    }

    let mut clean = String::with_capacity(text.len());
    let mut segments: Vec<TimedSegment> = Vec::new();
    let mut current_ts: Option<Duration> = None;
    let mut current_seg_text = String::new();

    let mut rest = text;
    while let Some(open_pos) = rest.find('<') {
        let before = &rest[..open_pos];
        clean.push_str(before);
        if current_ts.is_some() {
            current_seg_text.push_str(before);
        }

        let after_open = &rest[open_pos + 1..];
        if let Some(close_pos) = after_open.find('>') {
            let tag_content = &after_open[..close_pos];
            if let Some(ts) = parse_timestamp(tag_content) {
                if let Some(prev_ts) = current_ts.take() {
                    if !current_seg_text.is_empty() {
                        segments.push(TimedSegment {
                            timestamp: prev_ts,
                            text: std::mem::take(&mut current_seg_text),
                        });
                    }
                }
                current_ts = Some(ts);
                rest = &after_open[close_pos + 1..];
                continue;
            }
        }

        // Not a valid timestamp tag; keep literal '<'
        clean.push('<');
        if current_ts.is_some() {
            current_seg_text.push('<');
        }
        rest = after_open;
    }

    clean.push_str(rest);
    if let Some(prev_ts) = current_ts.take() {
        current_seg_text.push_str(rest);
        if !current_seg_text.is_empty() {
            segments.push(TimedSegment {
                timestamp: prev_ts,
                text: current_seg_text,
            });
        }
    }

    (clean.trim().to_string(), segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_standard_and_multi_timestamp_lrc() {
        let sample = "\
[ti:Midnight City]
[ar:M83]
[al:Hurry Up, We're Dreaming]
[offset:150]
[00:10.50]Waiting in a car
[00:14.20][00:45.00]Waiting for a ride in the dark
[00:18.750]The night city grows
";
        let parsed = ParsedLrc::parse(sample);
        assert_eq!(parsed.metadata.title.as_deref(), Some("Midnight City"));
        assert_eq!(parsed.metadata.artist.as_deref(), Some("M83"));
        assert_eq!(
            parsed.metadata.album.as_deref(),
            Some("Hurry Up, We're Dreaming")
        );
        assert_eq!(parsed.metadata.offset_ms, 150);
        assert_eq!(parsed.lines.len(), 4);
        assert_eq!(parsed.lines[0].timestamp, Duration::from_millis(10_500));
        assert_eq!(parsed.lines[0].text, "Waiting in a car");
        assert_eq!(parsed.lines[1].timestamp, Duration::from_millis(14_200));
        assert_eq!(parsed.lines[2].timestamp, Duration::from_millis(18_750));
        assert_eq!(parsed.lines[3].timestamp, Duration::from_millis(45_000));
        assert_eq!(parsed.lines[3].text, "Waiting for a ride in the dark");
    }

    #[test]
    fn test_parse_enhanced_lrc_segments() {
        let sample = "[00:05.00]<00:05.00>Hello <00:06.00>world";
        let parsed = ParsedLrc::parse(sample);
        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(parsed.lines[0].text, "Hello world");
        assert_eq!(parsed.lines[0].segments.len(), 2);
        assert_eq!(parsed.lines[0].segments[0].text, "Hello ");
        assert_eq!(parsed.lines[0].segments[1].text, "world");
    }

    #[test]
    fn test_lyrics_verification_rejects_chinese_credits_and_cleans_headers() {
        // 1. Exact case from user screenshot: NetEase suffixed timestamp + Chinese composer credit roster only
        let bogus_credits = "[00:00.00-1] 作曲 : Oliver Sykes/Lee Malia/Matthew Nicholls/Dan Lancaster/Zakk Cervini/Sione Teumohenga/Daisuke Ehara\n";
        let mut parsed_bogus = ParsedLrc::parse(bogus_credits);
        assert!(!parsed_bogus.verify_and_clean());

        let mut plain_bogus = ParsedLrc::from_plain_text(bogus_credits);
        assert!(!plain_bogus.verify_and_clean());

        // 2. Multi-line Chinese credit-only placeholder (composer + lyricist + instrumental notice)
        let multi_credits = "\
[00:00.00] 作词 : 某某
[00:00.50] 作曲 : 某某
[00:01.00] 编曲 : 某某
[00:02.00] 纯音乐，请欣赏
";
        let mut parsed_multi = ParsedLrc::parse(multi_credits);
        assert!(!parsed_multi.verify_and_clean());

        // 3. Valid lyrics that happen to start with Chinese credit lines: credits are stripped, real lyrics pass!
        let mixed_valid = "\
[00:00.00-1] 作曲 : Oliver Sykes/Lee Malia/Matthew Nicholls/Dan Lancaster
[00:01.00] 作词 : Oliver Sykes
[00:12.50] Can you feel my heart?
[00:16.20] Can you hear the silence?
[00:20.00] Can you see the dark?
";
        let mut parsed_valid = ParsedLrc::parse(mixed_valid);
        assert!(parsed_valid.verify_and_clean());
        assert_eq!(parsed_valid.lines.len(), 3);
        assert_eq!(parsed_valid.lines[0].text, "Can you feel my heart?");

        // 4. Unsynced plain text (even with [src:LRCLIB] header) must fail is_strictly_synced() and verify_and_clean()
        let unsynced_lrc = "\
[src:LRCLIB]
Call me by your name feel like
Citizen Kane 2049
Blade run like Im Justine in
RAW
I get Under the Skin and the
Skin I Live in Im the Thing
";
        let mut parsed_unsynced = ParsedLrc::parse(unsynced_lrc);
        assert!(!parsed_unsynced.is_strictly_synced());
        assert!(!parsed_unsynced.verify_and_clean());

        let mut plain_unsynced = ParsedLrc::from_plain_text(unsynced_lrc);
        assert!(!plain_unsynced.is_strictly_synced());
        assert!(!plain_unsynced.verify_and_clean());

        // 5. Fake synced file where all lines share [00:00.00] must also fail is_strictly_synced()
        let fake_synced = "\
[00:00.00] Line one
[00:00.00] Line two
[00:00.00] Line three
";
        let mut parsed_fake = ParsedLrc::parse(fake_synced);
        assert!(!parsed_fake.is_strictly_synced());
        assert!(!parsed_fake.verify_and_clean());
    }

    #[test]
    fn test_sync_state_interpolation() {
        let sample = "\
[00:10.00]First line
[00:20.00]Second line
";
        let parsed = ParsedLrc::parse(sample);

        let before = parsed.sync_state(Duration::from_secs(5), 0);
        assert_eq!(before.active_index, None);
        assert!((before.line_progress - 0.5).abs() < 1e-5);

        let mid = parsed.sync_state(Duration::from_secs(15), 0);
        assert_eq!(mid.active_index, Some(0));
        assert!((mid.line_progress - 0.5).abs() < 1e-5);
        assert!((mid.continuous_index - 0.5).abs() < 1e-5);
    }
}

