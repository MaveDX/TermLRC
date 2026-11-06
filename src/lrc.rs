use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

fn default_true() -> bool {
    true
}

/// Metadata extracted from LRC header tags (`[ti:]`, `[ar:]`, `[al:]`, `[offset:]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LrcMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
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
}

impl Default for ParsedLrc {
    fn default() -> Self {
        Self {
            metadata: LrcMetadata::default(),
            lines: Vec::new(),
            source_label: None,
            is_synced: true,
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
        if parsed.lines.len() >= 2 {
            Ok(parsed)
        } else {
            let mut plain = Self::from_plain_text(&content);
            plain.metadata = parsed.metadata;
            Ok(plain)
        }
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

        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }

            let (timestamps, remainder) = extract_line_timestamps(line);
            if timestamps.is_empty() {
                parse_metadata_tag(line, &mut metadata);
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

        Self {
            metadata,
            lines,
            source_label: None,
            is_synced: true,
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
            if trimmed.is_empty() {
                continue;
            }
            lines.push(LyricLine {
                timestamp: Duration::ZERO,
                text: trimmed.to_string(),
                segments: Vec::new(),
            });
        }
        Self {
            metadata,
            lines,
            source_label: None,
            is_synced: false,
        }
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

/// Parse a timestamp string like `01:23.45`, `01:23.456`, `01:23:45`, or `01:23`.
fn parse_timestamp(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (mins_str, rest) = s.split_once(':')?;
    let mins: u64 = mins_str.trim().parse().ok()?;

    let (secs, nanos) = if let Some((sec_str, frac_str)) = rest.split_once(['.', ':']) {
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
        let secs: u64 = rest.trim().parse().ok()?;
        (secs, 0)
    };

    Some(Duration::new(mins * 60 + secs, nanos))
}

/// Parse metadata tags like `[ti:Song Title]`, `[ar:Artist]`, `[al:Album]`, `[offset:+200]`.
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

