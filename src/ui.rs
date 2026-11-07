use crate::config::Settings;
use crate::lrc::{LyricSyncState, ParsedLrc};
use crate::providers::LookupStatus;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};
use unicode_width::UnicodeWidthChar;

/// Holds the smooth-scrolling camera state and pure lyric display preferences.
pub struct UiState {
    /// Current continuous floating-point scroll position in lyric line units.
    pub scroll_line: f64,
    /// Current floating-point scroll position in virtual terminal rows.
    pub scroll_y: f64,
    /// Whether Kitty Graphics Protocol pixel-level scrolling is enabled.
    pub kitty_graphics: bool,
    /// Configured primary font family name.
    pub font_family: String,
    /// Configured primary font file path.
    pub font_path: String,
    /// Configured fallback font family/path list.
    pub fallback_fonts: Vec<String>,
    /// Font size (in pixels) for the active lyric line in Kitty mode.
    pub font_size_active: f32,
    /// Font size (in pixels) for inactive lyric lines in Kitty mode.
    pub font_size_inactive: f32,
    /// Base vertical line spacing (in pixels) in Kitty mode.
    pub pixel_line_spacing: f32,
    /// Guaranteed minimum pixel gap between the active lyric block and neighboring lyrics.
    pub active_lyric_buffer_px: f32,
    /// Guaranteed minimum pixel gap between adjacent non-active lyric blocks.
    pub inactive_lyric_buffer_px: f32,
    /// Exponential damping speed for smooth scrolling.
    pub scroll_speed: f64,
    /// Base rows per lyric line in standard text TUI mode.
    pub line_spacing: usize,
    /// Guaranteed minimum blank rows around the active lyric block in standard text TUI mode.
    pub active_lyric_buffer_rows: usize,
    /// Whether the camera glides continuously across each line's duration.
    pub continuous_glide: bool,
    /// Whether karaoke-style horizontal highlight sweep is enabled.
    pub karaoke: bool,
    /// User-adjustable lyric timing offset in milliseconds.
    pub user_offset_ms: i64,
    /// Horizontal text alignment (`Center`, `Left`, or `Right`).
    pub alignment: Alignment,
    /// Whether to dim inactive lines with the terminal's DIM modifier.
    pub dim_inactive: bool,
    /// Whether to only dim already-sung (past) lines while keeping upcoming lines normal.
    pub dim_past_only: bool,
    /// Optional marker placed around the active lyric line.
    pub active_marker: String,
    /// Symbol shown on empty/instrumental timestamped lines.
    pub empty_line_symbol: String,
    /// Symbol shown when waiting for a song or when no matching LRC is loaded.
    pub idle_symbol: String,
    /// Manual scroll override offset (in lines) when user scrolls with Up/Down keys;
    /// resets automatically when the active lyric line advances.
    pub manual_line_offset: isize,
    /// Current status of lyric search/download when no lyrics are loaded yet.
    pub lookup_status: LookupStatus,
    last_active_index: Option<usize>,
}

impl UiState {
    pub fn from_settings(s: &Settings) -> Self {
        Self {
            scroll_line: 0.0,
            scroll_y: 0.0,
            kitty_graphics: s.kitty_graphics,
            font_family: s.font_family.clone(),
            font_path: s.font_path.clone(),
            fallback_fonts: s.fallback_fonts.clone(),
            font_size_active: s.font_size_active,
            font_size_inactive: s.font_size_inactive,
            pixel_line_spacing: s.pixel_line_spacing,
            active_lyric_buffer_px: s.active_lyric_buffer_px,
            inactive_lyric_buffer_px: s.inactive_lyric_buffer_px,
            scroll_speed: s.scroll_speed,
            line_spacing: s.line_spacing,
            active_lyric_buffer_rows: s.active_lyric_buffer_rows,
            continuous_glide: s.continuous_glide,
            karaoke: s.karaoke,
            user_offset_ms: s.offset_ms,
            alignment: s.text_alignment(),
            dim_inactive: s.dim_inactive,
            dim_past_only: s.dim_past_only,
            active_marker: s.active_marker.clone(),
            empty_line_symbol: s.empty_line_symbol.clone(),
            idle_symbol: s.idle_symbol.clone(),
            manual_line_offset: 0,
            lookup_status: LookupStatus::Idle,
            last_active_index: None,
        }
    }

    pub fn apply_settings(&mut self, s: &Settings) {
        self.kitty_graphics = s.kitty_graphics;
        self.font_family = s.font_family.clone();
        self.font_path = s.font_path.clone();
        self.fallback_fonts = s.fallback_fonts.clone();
        self.font_size_active = s.font_size_active;
        self.font_size_inactive = s.font_size_inactive;
        self.pixel_line_spacing = s.pixel_line_spacing;
        self.active_lyric_buffer_px = s.active_lyric_buffer_px;
        self.inactive_lyric_buffer_px = s.inactive_lyric_buffer_px;
        self.scroll_speed = s.scroll_speed;
        self.line_spacing = s.line_spacing;
        self.active_lyric_buffer_rows = s.active_lyric_buffer_rows;
        self.continuous_glide = s.continuous_glide;
        self.karaoke = s.karaoke;
        self.user_offset_ms = s.offset_ms;
        self.alignment = s.text_alignment();
        self.dim_inactive = s.dim_inactive;
        self.dim_past_only = s.dim_past_only;
        self.active_marker = s.active_marker.clone();
        self.empty_line_symbol = s.empty_line_symbol.clone();
        self.idle_symbol = s.idle_symbol.clone();
    }

    pub fn reset_for_new_track(&mut self) {
        self.scroll_line = 0.0;
        self.scroll_y = 0.0;
        self.manual_line_offset = 0;
        self.last_active_index = None;
    }

    /// Advance the smooth-scrolling camera by `dt_secs` seconds.
    pub fn step_animation(&mut self, sync: &LyricSyncState, total_lines: usize, dt_secs: f64) {
        if sync.active_index != self.last_active_index {
            self.last_active_index = sync.active_index;
            self.manual_line_offset = 0;
        }

        if total_lines == 0 {
            self.scroll_line = 0.0;
            self.scroll_y = 0.0;
            return;
        }

        let step = self.line_spacing.max(1) as f64;
        let base_line = if self.continuous_glide {
            sync.continuous_index.max(0.0)
        } else {
            sync.active_index.unwrap_or(0) as f64
        };

        let target_line = (base_line + self.manual_line_offset as f64)
            .clamp(0.0, (total_lines.saturating_sub(1)) as f64);

        let alpha = 1.0 - (-self.scroll_speed * dt_secs.clamp(0.001, 0.25)).exp();
        self.scroll_line += (target_line - self.scroll_line) * alpha;
        if (target_line - self.scroll_line).abs() < 0.0002 {
            self.scroll_line = target_line;
        }
        self.scroll_y = self.scroll_line * step;
    }
}

pub fn draw(frame: &mut Frame, lrc: Option<&ParsedLrc>, sync: &LyricSyncState, ui: &UiState) {
    let area = frame.area();
    if area.width < 4 || area.height < 1 {
        return;
    }

    let Some(parsed) = lrc.filter(|l| !l.lines.is_empty()) else {
        let mid_y = area.y + area.height / 2;
        match ui.lookup_status {
            LookupStatus::Downloading { progress } => {
                let pct = (progress.clamp(0.0, 1.0) * 100.0).round() as usize;
                let bar_w = 20usize;
                let filled = ((progress.clamp(0.0, 1.0) * bar_w as f32).round() as usize).min(bar_w);
                let empty = bar_w.saturating_sub(filled);
                let bar_str = format!(
                    "[{}{}] {}%",
                    "█".repeat(filled),
                    "░".repeat(empty),
                    pct
                );
                let row1 = Rect {
                    x: area.x,
                    y: mid_y.saturating_sub(1),
                    width: area.width,
                    height: 1,
                };
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        "Downloading",
                        Style::default().add_modifier(Modifier::DIM),
                    ))
                    .alignment(ui.alignment),
                    row1,
                );
                if mid_y < area.y + area.height {
                    let row2 = Rect {
                        x: area.x,
                        y: mid_y,
                        width: area.width,
                        height: 1,
                    };
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            bar_str,
                            Style::default().add_modifier(Modifier::DIM),
                        ))
                        .alignment(ui.alignment),
                        row2,
                    );
                }
            }
            _ => {
                let msg = match ui.lookup_status {
                    LookupStatus::Idle => ui.idle_symbol.as_str(),
                    LookupStatus::Searching => "Searching",
                    LookupStatus::RateLimited => "Rate limited",
                    LookupStatus::NotFound => "Lyrics not found",
                    LookupStatus::Downloading { .. } => "Downloading",
                };
                if !msg.is_empty() {
                    let row = Rect {
                        x: area.x,
                        y: mid_y,
                        width: area.width,
                        height: 1,
                    };
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            msg,
                            Style::default().add_modifier(Modifier::DIM),
                        ))
                        .alignment(ui.alignment),
                        row,
                    );
                }
            }
        }
        return;
    };

    let viewport_h = area.height as isize;
    let center_row = viewport_h / 2;
    let base_step = ui.line_spacing.max(1) as f64;
    let max_text_width = area.width.saturating_sub(4) as usize;
    let n = parsed.lines.len();

    // Pre-wrap lines and compute cumulative non-overlapping row centers
    let mut wrapped_per_line: Vec<Vec<String>> = Vec::with_capacity(n);
    let mut height_rows: Vec<f64> = Vec::with_capacity(n);

    for line in &parsed.lines {
        let display_text = if line.text.trim().is_empty() {
            ui.empty_line_symbol.as_str()
        } else {
            line.text.as_str()
        };
        if display_text.is_empty() {
            wrapped_per_line.push(Vec::new());
            height_rows.push(1.0);
        } else {
            let w = wrap_text(display_text, max_text_width.max(10));
            let h = w.len().max(1) as f64;
            wrapped_per_line.push(w);
            height_rows.push(h);
        }
    }

    let mut cum_y: Vec<f64> = Vec::with_capacity(n);
    cum_y.push(0.0);
    let buffer_rows = ui.active_lyric_buffer_rows as f64;
    for i in 1..n {
        let min_non_overlap = 0.5 * height_rows[i - 1] + 0.5 * height_rows[i] + buffer_rows;
        let step_i = min_non_overlap.max(base_step);
        cum_y.push(cum_y[i - 1] + step_i);
    }

    let clamped_scroll = ui.scroll_line.clamp(0.0, (n.saturating_sub(1)) as f64);
    let i0 = clamped_scroll.floor() as usize;
    let i1 = (i0 + 1).min(n.saturating_sub(1));
    let frac = clamped_scroll - i0 as f64;
    let camera_y = cum_y[i0] + (cum_y[i1] - cum_y[i0]) * frac;

    let first_half_h = height_rows.first().copied().unwrap_or(1.0) * 0.5;
    let top_step = (first_half_h + buffer_rows + 1.0).max(base_step);

    if sync.active_index.is_none() && !ui.empty_line_symbol.is_empty() {
        let intro_y = -top_step;
        let rel_y = (intro_y - camera_y).round() as isize;
        let screen_y = center_row + rel_y;
        if screen_y >= 0 && screen_y < viewport_h {
            let row_rect = Rect {
                x: area.x,
                y: area.y + screen_y as u16,
                width: area.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    ui.empty_line_symbol.as_str(),
                    Style::default().add_modifier(Modifier::DIM),
                ))
                .alignment(ui.alignment),
                row_rect,
            );
        }
    }

    if let Some(ref source_label) = parsed.source_label {
        let label_y = if !ui.empty_line_symbol.is_empty() {
            -top_step - base_step.max(2.0)
        } else {
            -top_step
        };
        let rel_y = (label_y - camera_y).round() as isize;
        let screen_y = center_row + rel_y;
        if screen_y >= 0 && screen_y < viewport_h {
            let row_rect = Rect {
                x: area.x,
                y: area.y + screen_y as u16,
                width: area.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    source_label.as_str(),
                    Style::default().add_modifier(Modifier::DIM),
                ))
                .alignment(ui.alignment),
                row_rect,
            );
        }
    }

    for (idx, wrapped) in wrapped_per_line.iter().enumerate() {
        if wrapped.is_empty() {
            continue;
        }
        let exact_offset_rows = cum_y[idx] - camera_y;
        let block_h = wrapped.len() as isize;
        let top_row = center_row + exact_offset_rows.round() as isize - (block_h / 2);

        if top_row + block_h < 0 || top_row >= viewport_h {
            continue;
        }

        let is_active = sync.active_index == Some(idx);
        let dist_lines = (idx as f64 - ui.scroll_line).abs();

        for (wrap_idx, sub_line) in wrapped.iter().enumerate() {
            let row_y = top_row + wrap_idx as isize;
            if row_y < 0 || row_y >= viewport_h {
                continue;
            }

            let row_rect = Rect {
                x: area.x,
                y: area.y + row_y as u16,
                width: area.width,
                height: 1,
            };

            let rendered_line = if is_active {
                let marker = if wrap_idx == 0 {
                    ui.active_marker.as_str()
                } else {
                    ""
                };
                build_active_lyric_line(sub_line, marker, ui.karaoke, sync.karaoke_progress)
            } else {
                let is_past = idx < sync.active_index.unwrap_or(0);
                let should_dim = if !ui.dim_inactive {
                    false
                } else if ui.dim_past_only {
                    is_past
                } else {
                    is_past || dist_lines > 1.5
                };
                let mut style = Style::default();
                if should_dim {
                    style = style.add_modifier(Modifier::DIM);
                }
                Line::from(Span::styled(sub_line.clone(), style))
            };

            frame.render_widget(
                Paragraph::new(rendered_line).alignment(ui.alignment),
                row_rect,
            );
        }
    }
}

fn build_active_lyric_line(
    text: &str,
    marker: &str,
    karaoke: bool,
    karaoke_progress: f64,
) -> Line<'static> {
    let active_style = Style::default().add_modifier(Modifier::BOLD);
    let unsung_style = Style::default().add_modifier(Modifier::DIM);

    let mut spans = Vec::new();
    if !marker.is_empty() {
        spans.push(Span::styled(marker.to_string(), active_style));
    }

    if !karaoke {
        spans.push(Span::styled(text.to_string(), active_style));
        return Line::from(spans);
    }

    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    if total == 0 {
        return Line::from(spans);
    }

    let lit_count = (karaoke_progress.clamp(0.0, 1.0) * total as f64).round() as usize;
    let lit_count = lit_count.min(total);

    if lit_count > 0 {
        let lit_str: String = chars[..lit_count].iter().collect();
        spans.push(Span::styled(lit_str, active_style));
    }
    if lit_count < total {
        let rest_str: String = chars[lit_count..].iter().collect();
        spans.push(Span::styled(rest_str, unsung_style));
    }

    Line::from(spans)
}

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;

    let mut word_buf = String::new();
    let mut word_w = 0usize;

    let flush_word = |word_buf: &mut String,
                      word_w: &mut usize,
                      current: &mut String,
                      current_w: &mut usize,
                      lines: &mut Vec<String>| {
        if word_buf.is_empty() {
            return;
        }
        if *current_w + *word_w > max_width && !current.is_empty() {
            lines.push(std::mem::take(current));
            *current_w = 0;
        }
        current.push_str(word_buf);
        *current_w += *word_w;
        word_buf.clear();
        *word_w = 0;
    };

    for ch in text.chars() {
        let ch_w = ch.width().unwrap_or(1);
        if ch.is_whitespace() {
            flush_word(
                &mut word_buf,
                &mut word_w,
                &mut current,
                &mut current_w,
                &mut lines,
            );
            if !current.is_empty() && current_w + 1 <= max_width {
                current.push(' ');
                current_w += 1;
            }
        } else if crate::kitty::is_cjk_breakable(ch) {
            flush_word(
                &mut word_buf,
                &mut word_w,
                &mut current,
                &mut current_w,
                &mut lines,
            );
            if current_w + ch_w > max_width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_w = 0;
            }
            current.push(ch);
            current_w += ch_w;
        } else {
            if word_w + ch_w > max_width {
                flush_word(
                    &mut word_buf,
                    &mut word_w,
                    &mut current,
                    &mut current_w,
                    &mut lines,
                );
            }
            word_buf.push(ch);
            word_w += ch_w;
        }
    }

    flush_word(
        &mut word_buf,
        &mut word_w,
        &mut current,
        &mut current_w,
        &mut lines,
    );

    let trimmed = current.trim_end().to_string();
    if !trimmed.is_empty() || lines.is_empty() {
        lines.push(trimmed);
    }

    lines
}
