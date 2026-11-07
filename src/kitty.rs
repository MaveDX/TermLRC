use crate::lrc::{LyricSyncState, ParsedLrc};
use crate::providers::LookupStatus;
use crate::ui::UiState;
use ab_glyph::{point, Font, FontArc, GlyphId, PxScale, ScaleFont};
use anyhow::Result;
use crossterm::terminal::window_size;
use ratatui::layout::Alignment;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

/// Lazy-loaded, ultra-lightweight font stack.
/// Only the single primary font (~0.5 MB) is loaded at startup; fallback fonts (CJK, symbols)
/// are kept as paths and only loaded into memory on demand when a song actually contains
/// a character missing from the primary font.
pub struct FontStack {
    loaded: Vec<FontArc>,
    pending_paths: Vec<PathBuf>,
    missing_chars: BTreeSet<char>,
}

impl FontStack {
    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty()
    }

    pub fn primary(&self) -> &FontArc {
        &self.loaded[0]
    }

    pub fn font(&self, idx: usize) -> &FontArc {
        &self.loaded[idx]
    }

    /// Resolve the first font in the stack that contains a valid glyph (`GlyphId != 0`) for `ch`.
    /// Lazy-loads pending fallback font files on demand only if `ch` is missing from all currently loaded fonts.
    pub fn resolve_char(&mut self, ch: char) -> (usize, GlyphId) {
        for (idx, font) in self.loaded.iter().enumerate() {
            let id = font.glyph_id(ch);
            if id.0 != 0 {
                return (idx, id);
            }
        }

        if !self.missing_chars.contains(&ch) && !self.pending_paths.is_empty() {
            let mut found_idx = None;
            for i in 0..self.pending_paths.len() {
                let path = &self.pending_paths[i];
                if let Ok(bytes) = fs::read(path) {
                    if let Ok(font) = FontArc::try_from_vec(bytes) {
                        let id = font.glyph_id(ch);
                        if id.0 != 0 {
                            let new_font_idx = self.loaded.len();
                            self.loaded.push(font);
                            found_idx = Some((i, new_font_idx, id));
                            break;
                        }
                        // `font` and `bytes` are dropped immediately here if they don't contain `ch`!
                    }
                }
            }

            if let Some((path_idx, font_idx, glyph_id)) = found_idx {
                self.pending_paths.remove(path_idx);
                return (font_idx, glyph_id);
            }

            self.missing_chars.insert(ch);
        }

        let fallback_id = self.loaded.first().map(|f| f.glyph_id(ch)).unwrap_or(GlyphId(0));
        (0, fallback_id)
    }
}

/// Kitty Graphics Protocol pixel-level smooth scrolling renderer with multi-font CJK fallback
/// and guaranteed non-overlapping bounding-box layout.
pub struct KittyRenderer {
    fonts: FontStack,
    font_config_key: (String, String, Vec<String>),
    rgba_buf: Vec<u8>,
    out_buf: Vec<u8>,
    use_shm: bool,
    last_shm_path: Option<PathBuf>,
    frame_seq: u64,
    last_state_key: Option<FrameStateKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FrameStateKey {
    width: u32,
    height: u32,
    has_lrc: bool,
    line_count: usize,
    active_index: Option<usize>,
    /// Scroll position quantized to 1/32nd of a pixel
    scroll_subpx: i64,
    /// Karaoke progress quantized to 1/2000th
    karaoke_permille: u32,
    karaoke_enabled: bool,
    font_active_tenths: u32,
    font_inactive_tenths: u32,
    spacing_tenths: u32,
    active_buf_tenths: u32,
    inactive_buf_tenths: u32,
    status_tag: u8,
    download_pct: u8,
}

impl KittyRenderer {
    pub fn new(font_path: &str, font_family: &str, fallback_fonts: &[String]) -> Self {
        let fonts = load_font_stack(font_path, font_family, fallback_fonts);
        let use_shm = Path::new("/dev/shm").is_dir();
        Self {
            fonts,
            font_config_key: (
                font_path.to_string(),
                font_family.to_string(),
                fallback_fonts.to_vec(),
            ),
            rgba_buf: Vec::new(),
            out_buf: Vec::with_capacity(4096),
            use_shm,
            last_shm_path: None,
            frame_seq: 0,
            last_state_key: None,
        }
    }

    pub fn update_fonts_if_changed(
        &mut self,
        font_path: &str,
        font_family: &str,
        fallback_fonts: &[String],
    ) {
        if self.font_config_key.0 != font_path
            || self.font_config_key.1 != font_family
            || self.font_config_key.2 != fallback_fonts
        {
            self.fonts = load_font_stack(font_path, font_family, fallback_fonts);
            self.font_config_key = (
                font_path.to_string(),
                font_family.to_string(),
                fallback_fonts.to_vec(),
            );
            self.last_state_key = None;
        }
    }

    pub fn invalidate(&mut self) {
        self.last_state_key = None;
    }

    /// Delete all active Kitty graphics placements, release the pixel framebuffer, and clean up SHM.
    pub fn clear_placements(&mut self) -> Result<()> {
        if let Some(prev) = self.last_shm_path.take() {
            let _ = fs::remove_file(prev);
        }
        self.rgba_buf = Vec::new();
        self.out_buf = Vec::new();
        self.last_state_key = None;
        let mut stdout = io::stdout().lock();
        stdout.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\")?;
        stdout.flush()?;
        Ok(())
    }

    /// Render the current lyrics frame at pixel precision and transmit via Kitty Graphics Protocol.
    pub fn render_frame(
        &mut self,
        lrc: Option<&ParsedLrc>,
        sync: &LyricSyncState,
        ui: &UiState,
    ) -> Result<()> {
        let (width, height) = query_pixel_dimensions();
        if width < 32 || height < 32 {
            return Ok(());
        }

        let (status_tag, download_pct) = match ui.lookup_status {
            LookupStatus::Idle => (0u8, 0u8),
            LookupStatus::Searching => (1u8, 0u8),
            LookupStatus::Downloading { progress } => {
                (2u8, (progress.clamp(0.0, 1.0) * 100.0).round() as u8)
            }
            LookupStatus::RateLimited => (3u8, 0u8),
            LookupStatus::NotFound => (4u8, 0u8),
        };

        let scroll_px = ui.scroll_line * ui.pixel_line_spacing as f64;
        let state_key = FrameStateKey {
            width,
            height,
            has_lrc: lrc.is_some(),
            line_count: lrc.map(|l| l.lines.len()).unwrap_or(0),
            active_index: sync.active_index,
            scroll_subpx: (scroll_px * 32.0).round() as i64,
            karaoke_permille: (sync.karaoke_progress.clamp(0.0, 1.0) * 2000.0).round() as u32,
            karaoke_enabled: ui.karaoke,
            font_active_tenths: (ui.font_size_active * 10.0).round() as u32,
            font_inactive_tenths: (ui.font_size_inactive * 10.0).round() as u32,
            spacing_tenths: (ui.pixel_line_spacing * 10.0).round() as u32,
            active_buf_tenths: (ui.active_lyric_buffer_px * 10.0).round() as u32,
            inactive_buf_tenths: (ui.inactive_lyric_buffer_px * 10.0).round() as u32,
            status_tag,
            download_pct,
        };

        if self.last_state_key.as_ref() == Some(&state_key) {
            return Ok(());
        }
        self.last_state_key = Some(state_key);

        let total_bytes = (width as usize) * (height as usize) * 4;
        if self.rgba_buf.len() != total_bytes {
            self.rgba_buf.resize(total_bytes, 0);
            self.rgba_buf.shrink_to_fit();
        } else {
            self.rgba_buf.fill(0);
        }

        draw_pixel_lyrics(
            &mut self.fonts,
            &mut self.rgba_buf,
            width,
            height,
            lrc,
            sync,
            ui,
        );

        self.transmit_rgba_frame(width, height)
    }

    fn transmit_rgba_frame(&mut self, width: u32, height: u32) -> Result<()> {
        if let Some(prev_path) = self.last_shm_path.take() {
            if prev_path.exists() {
                let _ = fs::remove_file(&prev_path);
                self.use_shm = false;
            }
        }

        self.out_buf.clear();
        self.out_buf.extend_from_slice(b"\x1b[?2026h\x1b[H");

        if self.use_shm {
            self.frame_seq = self.frame_seq.wrapping_add(1);
            let shm_name = format!("/termlrc_{}_{}", std::process::id(), self.frame_seq);
            let shm_path = PathBuf::from(format!("/dev/shm{}", shm_name));
            if fs::write(&shm_path, &self.rgba_buf).is_ok() {
                write!(
                    self.out_buf,
                    "\x1b_Ga=T,f=32,t=s,s={},v={},i=1,p=1,C=1,q=2;",
                    width, height
                )?;
                encode_base64_append(shm_name.as_bytes(), &mut self.out_buf);
                self.out_buf.extend_from_slice(b"\x1b\\\x1b[?2026l");

                let mut stdout = io::stdout().lock();
                stdout.write_all(&self.out_buf)?;
                stdout.flush()?;
                self.last_shm_path = Some(shm_path);
                return Ok(());
            } else {
                self.use_shm = false;
            }
        }

        // Stream base64 directly in 3072-byte raw chunks (4096 base64 chars per chunk)
        // without allocating a second full-screen buffer in heap memory.
        let mut stdout = io::stdout().lock();
        stdout.write_all(&self.out_buf)?;

        let raw_chunks: Vec<&[u8]> = self.rgba_buf.chunks(3072).collect();
        let num_chunks = raw_chunks.len();
        for (i, raw_chunk) in raw_chunks.into_iter().enumerate() {
            self.out_buf.clear();
            let more = if i + 1 < num_chunks { 1 } else { 0 };
            if i == 0 {
                write!(
                    self.out_buf,
                    "\x1b_Ga=T,f=32,t=d,s={},v={},i=1,p=1,C=1,q=2,m={};",
                    width, height, more
                )?;
            } else {
                write!(self.out_buf, "\x1b_Gm={};", more)?;
            }
            encode_base64_append(raw_chunk, &mut self.out_buf);
            self.out_buf.extend_from_slice(b"\x1b\\");
            stdout.write_all(&self.out_buf)?;
        }

        stdout.write_all(b"\x1b[?2026l")?;
        stdout.flush()?;
        if self.out_buf.capacity() > 16384 {
            self.out_buf.shrink_to(8192);
        }
        Ok(())
    }
}

impl Drop for KittyRenderer {
    fn drop(&mut self) {
        if let Some(prev) = self.last_shm_path.take() {
            let _ = fs::remove_file(prev);
        }
    }
}

fn query_pixel_dimensions() -> (u32, u32) {
    if let Ok(ws) = window_size() {
        if ws.width > 0 && ws.height > 0 {
            return (
                (ws.width as u32).min(2560),
                (ws.height as u32).min(1600),
            );
        }
        if ws.columns > 0 && ws.rows > 0 {
            return (
                (ws.columns as u32 * 10).min(2560),
                (ws.rows as u32 * 20).min(1600),
            );
        }
    }
    (960, 540)
}

fn char_advance_px(fonts: &mut FontStack, scale: PxScale, ch: char) -> f32 {
    if fonts.is_empty() {
        return scale.x * 0.5;
    }
    let (font_idx, glyph_id) = fonts.resolve_char(ch);
    fonts.font(font_idx).as_scaled(scale).h_advance(glyph_id)
}

/// Precomputed layout measurement for a single lyric entry (potentially wrapped onto multiple sublines).
pub struct MeasuredLyricBlock {
    pub wrapped_lines: Vec<String>,
    pub font_size: f32,
    pub subline_height: f32,
    pub total_height: f32,
    pub reserved_height: f32,
    pub center_y: f32,
}

/// Compute cumulative vertical layout for all lyric lines, guaranteeing that:
/// 1. Line wrapping is computed at the reference maximum font size so subline counts NEVER
///    jump or re-wrap as lines focus and unfocus.
/// 2. Inter-line spacing (`cum_y`) is completely rigid and invariant across focus changes so
///    lyrics never warp or shift relative to each other.
/// 3. Adjacent lines NEVER overlap and always maintain at least `active_lyric_buffer_px` of separation.
pub fn compute_non_overlapping_layout(
    fonts: &mut FontStack,
    parsed: &ParsedLrc,
    sync: &LyricSyncState,
    ui: &UiState,
    max_text_width: f32,
    viewport_center_y: f32,
) -> Vec<MeasuredLyricBlock> {
    let n = parsed.lines.len();
    if n == 0 || fonts.is_empty() {
        return Vec::new();
    }

    // Wrap every line at the reference maximum font size so line breaks never change when focusing/unfocusing
    let ref_font_size = ui.font_size_active.max(ui.font_size_inactive);
    let ref_scale = PxScale::from(ref_font_size);
    let ref_subline_height = fonts.primary().as_scaled(ref_scale).height() * 1.18;

    let mut blocks: Vec<MeasuredLyricBlock> = Vec::with_capacity(n);

    for (idx, line) in parsed.lines.iter().enumerate() {
        let is_active = sync.active_index == Some(idx);
        let dist_lines = (idx as f32 - ui.scroll_line as f32).abs();
        let focus = (1.0 - (dist_lines / 1.5).clamp(0.0, 1.0)).powi(2);
        let font_size =
            ui.font_size_inactive + (ui.font_size_active - ui.font_size_inactive) * focus;

        let display_text = if line.text.trim().is_empty() {
            ui.empty_line_symbol.as_str()
        } else {
            line.text.as_str()
        };

        let ref_text = if !ui.active_marker.is_empty() && !display_text.is_empty() {
            format!("{}{}", ui.active_marker, display_text)
        } else {
            display_text.to_string()
        };

        let mut wrapped_lines = if ref_text.is_empty() {
            Vec::new()
        } else {
            wrap_text_by_pixel_width(fonts, ref_scale, &ref_text, max_text_width)
        };

        if !is_active && !ui.active_marker.is_empty() {
            if let Some(first) = wrapped_lines.first_mut() {
                if let Some(stripped) = first.strip_prefix(&ui.active_marker) {
                    *first = stripped.to_string();
                }
            }
        }

        let scale = PxScale::from(font_size);
        let subline_height = fonts.primary().as_scaled(scale).height() * 1.18;
        let num_sublines = wrapped_lines.len().max(1);
        let total_height = num_sublines as f32 * subline_height;
        let reserved_height = num_sublines as f32 * ref_subline_height;

        blocks.push(MeasuredLyricBlock {
            wrapped_lines,
            font_size,
            subline_height,
            total_height,
            reserved_height,
            center_y: 0.0,
        });
    }

    // Build static, focus-invariant cumulative centers so lines never warp or shift relative to each other
    let mut cum_y: Vec<f32> = Vec::with_capacity(n);
    cum_y.push(0.0);

    let guaranteed_gap = ui
        .active_lyric_buffer_px
        .max(ui.inactive_lyric_buffer_px)
        .max(4.0);
    let base_step = ui.pixel_line_spacing.max(12.0);

    for i in 1..n {
        let prev_h = blocks[i - 1].reserved_height;
        let curr_h = blocks[i].reserved_height;
        let min_non_overlapping_step = 0.5 * prev_h + 0.5 * curr_h + guaranteed_gap;
        let step_i = min_non_overlapping_step.max(base_step);
        cum_y.push(cum_y[i - 1] + step_i);
    }

    // Smoothly interpolate camera Y in cumulative layout space
    let clamped_scroll = ui.scroll_line.clamp(0.0, (n - 1) as f64) as f32;
    let i0 = clamped_scroll.floor() as usize;
    let i1 = (i0 + 1).min(n - 1);
    let frac = clamped_scroll - i0 as f32;
    let camera_y = cum_y[i0] + (cum_y[i1] - cum_y[i0]) * frac;

    for (idx, block) in blocks.iter_mut().enumerate() {
        block.center_y = viewport_center_y + (cum_y[idx] - camera_y);
    }

    blocks
}

fn draw_pixel_lyrics(
    fonts: &mut FontStack,
    buf: &mut [u8],
    width: u32,
    height: u32,
    lrc: Option<&ParsedLrc>,
    sync: &LyricSyncState,
    ui: &UiState,
) {
    if fonts.is_empty() {
        return;
    }

    let w_f = width as f32;
    let h_f = height as f32;
    let center_y = h_f * 0.5;
    let half_h = (h_f * 0.5).max(1.0);
    let step_px = ui.pixel_line_spacing.max(20.0);
    let margin_x = (w_f * 0.06).clamp(24.0, 80.0);
    let max_text_width = (w_f - margin_x * 2.0).max(120.0);

    let Some(parsed) = lrc.filter(|l| !l.lines.is_empty()) else {
        let scale = PxScale::from(ui.font_size_inactive);
        let subline_h = fonts.primary().as_scaled(scale).height() * 1.18;
        match ui.lookup_status {
            LookupStatus::Downloading { progress } => {
                let pct = (progress.clamp(0.0, 1.0) * 100.0).round() as u8;
                let label = format!("Downloading ({}%)", pct);
                let wrapped = wrap_text_by_pixel_width(fonts, scale, &label, max_text_width);
                draw_prewrapped_block(
                    fonts,
                    buf,
                    width,
                    height,
                    &wrapped,
                    center_y - subline_h * 0.75,
                    ui.font_size_inactive,
                    subline_h,
                    margin_x,
                    ui.alignment,
                    false,
                    0.0,
                    160,
                );
                draw_pixel_progress_bar(
                    buf,
                    width,
                    height,
                    center_y + subline_h * 0.35,
                    margin_x,
                    max_text_width,
                    ui.alignment,
                    progress.clamp(0.0, 1.0),
                );
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
                    let wrapped = wrap_text_by_pixel_width(fonts, scale, msg, max_text_width);
                    draw_prewrapped_block(
                        fonts,
                        buf,
                        width,
                        height,
                        &wrapped,
                        center_y,
                        ui.font_size_inactive,
                        subline_h,
                        margin_x,
                        ui.alignment,
                        false,
                        0.0,
                        140,
                    );
                }
            }
        }
        return;
    };

    let layout_blocks =
        compute_non_overlapping_layout(fonts, parsed, sync, ui, max_text_width, center_y);

    let first_top = layout_blocks
        .first()
        .map(|b| b.center_y - 0.5 * b.reserved_height)
        .unwrap_or(center_y);
    let top_gap = ui.active_lyric_buffer_px.max(step_px * 0.6);

    if sync.active_index.is_none() && !ui.empty_line_symbol.is_empty() {
        let intro_y = first_top - top_gap;
        if intro_y > -step_px && intro_y < h_f + step_px {
            let scale = PxScale::from(ui.font_size_inactive);
            let subline_h = fonts.primary().as_scaled(scale).height() * 1.18;
            let wrapped =
                wrap_text_by_pixel_width(fonts, scale, &ui.empty_line_symbol, max_text_width);
            draw_prewrapped_block(
                fonts,
                buf,
                width,
                height,
                &wrapped,
                intro_y,
                ui.font_size_inactive,
                subline_h,
                margin_x,
                ui.alignment,
                false,
                0.0,
                110,
            );
        }
    }

    // If lyrics came from a remote fallback provider, render "Lyrics from <Provider>"
    // anchored above line 0 so it naturally scrolls away with the lyrics.
    if let Some(ref source_label) = parsed.source_label {
        let scale = PxScale::from(ui.font_size_inactive);
        let subline_h = fonts.primary().as_scaled(scale).height() * 1.18;
        let extra_intro_offset = if !ui.empty_line_symbol.is_empty() {
            top_gap + subline_h * 0.8
        } else {
            0.0
        };
        let label_y = first_top - top_gap - extra_intro_offset;
        if label_y > -step_px && label_y < h_f + step_px {
            let edge_norm = ((label_y - center_y).abs() / half_h).clamp(0.0, 1.0);
            let edge_fade = (1.0 - edge_norm.powi(2)).clamp(0.0, 1.0);
            let label_alpha = (130.0 * edge_fade).round() as u8;
            if label_alpha >= 4 {
                let wrapped =
                    wrap_text_by_pixel_width(fonts, scale, source_label, max_text_width);
                draw_prewrapped_block(
                    fonts,
                    buf,
                    width,
                    height,
                    &wrapped,
                    label_y,
                    ui.font_size_inactive,
                    subline_h,
                    margin_x,
                    ui.alignment,
                    false,
                    0.0,
                    label_alpha,
                );
            }
        }
    }

    for (idx, block) in layout_blocks.iter().enumerate() {
        if block.wrapped_lines.is_empty() {
            continue;
        }

        let top_edge = block.center_y - 0.5 * block.total_height;
        let bottom_edge = block.center_y + 0.5 * block.total_height;
        if bottom_edge < -16.0 || top_edge > h_f + 16.0 {
            continue;
        }

        let is_active = sync.active_index == Some(idx);
        let dist_lines = (idx as f32 - ui.scroll_line as f32).abs();

        let edge_norm = ((block.center_y - center_y).abs() / half_h).clamp(0.0, 1.0);
        let edge_fade = (1.0 - edge_norm.powi(2)).clamp(0.0, 1.0);

        let base_alpha = if is_active {
            (255.0 * edge_fade).round() as u8
        } else {
            let is_past = idx < sync.active_index.unwrap_or(0);
            let should_dim = if !ui.dim_inactive {
                false
            } else if ui.dim_past_only {
                is_past
            } else {
                true
            };
            let dist_t = (dist_lines / 5.0).clamp(0.0, 1.0);
            let raw_alpha = if should_dim {
                145.0 * (1.0 - dist_t * 0.65)
            } else {
                220.0 * (1.0 - dist_t * 0.45)
            };
            (raw_alpha * edge_fade).round() as u8
        };

        if base_alpha < 4 {
            continue;
        }

        draw_prewrapped_block(
            fonts,
            buf,
            width,
            height,
            &block.wrapped_lines,
            block.center_y,
            block.font_size,
            block.subline_height,
            margin_x,
            ui.alignment,
            is_active && ui.karaoke,
            sync.karaoke_progress as f32,
            base_alpha,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_pixel_progress_bar(
    buf: &mut [u8],
    width: u32,
    height: u32,
    center_y: f32,
    margin_x: f32,
    max_text_width: f32,
    alignment: Alignment,
    progress: f32,
) {
    let bar_w = (max_text_width * 0.42).clamp(140.0, 320.0);
    let bar_h = 6.0f32;
    let start_x = match alignment {
        Alignment::Left => margin_x,
        Alignment::Right => (width as f32 - margin_x - bar_w).max(margin_x),
        Alignment::Center => ((width as f32 - bar_w) * 0.5).max(margin_x),
    };
    let x0 = (start_x.round() as i32).clamp(0, width as i32);
    let x1 = ((start_x + bar_w).round() as i32).clamp(0, width as i32);
    let fill_x1 = ((start_x + bar_w * progress.clamp(0.0, 1.0)).round() as i32).clamp(x0, x1);
    let y0 = ((center_y - bar_h * 0.5).round() as i32).clamp(0, height as i32);
    let y1 = ((center_y + bar_h * 0.5).round() as i32).clamp(0, height as i32);

    for py in y0..y1 {
        for px in x0..x1 {
            let alpha = if px < fill_x1 { 220u8 } else { 55u8 };
            let idx = ((py as u32 * width + px as u32) * 4) as usize;
            if idx + 3 < buf.len() {
                buf[idx] = 255;
                buf[idx + 1] = 255;
                buf[idx + 2] = 255;
                buf[idx + 3] = buf[idx + 3].max(alpha);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_prewrapped_block(
    fonts: &mut FontStack,
    buf: &mut [u8],
    width: u32,
    height: u32,
    wrapped_lines: &[String],
    center_y: f32,
    font_size: f32,
    line_height: f32,
    margin_x: f32,
    alignment: Alignment,
    karaoke: bool,
    karaoke_progress: f32,
    alpha: u8,
) {
    let scale = PxScale::from(font_size);
    let num_sublines = wrapped_lines.len();
    let total_block_h = (num_sublines.saturating_sub(1)) as f32 * line_height;
    let start_y = center_y - total_block_h * 0.5;

    let total_chars: usize = wrapped_lines.iter().map(|s| s.chars().count()).sum();
    let mut chars_before_subline = 0usize;

    for (sub_idx, sub_text) in wrapped_lines.iter().enumerate() {
        let sub_chars = sub_text.chars().count();
        let sub_y = start_y + sub_idx as f32 * line_height;

        let sub_karaoke = if karaoke && total_chars > 0 && sub_chars > 0 {
            let global_char_pos = karaoke_progress.clamp(0.0, 1.0) * total_chars as f32;
            ((global_char_pos - chars_before_subline as f32) / sub_chars as f32).clamp(0.0, 1.0)
        } else {
            1.0
        };
        chars_before_subline += sub_chars;

        draw_single_subline(
            fonts,
            scale,
            buf,
            width,
            height,
            sub_text,
            sub_y,
            margin_x,
            alignment,
            karaoke,
            sub_karaoke,
            alpha,
        );
    }
}

struct PositionedCharGlyph {
    font_idx: usize,
    glyph_id: GlyphId,
    x_offset: f32,
}

#[allow(clippy::too_many_arguments)]
fn draw_single_subline(
    fonts: &mut FontStack,
    scale: PxScale,
    buf: &mut [u8],
    width: u32,
    height: u32,
    text: &str,
    center_y: f32,
    margin_x: f32,
    alignment: Alignment,
    karaoke: bool,
    sub_karaoke_progress: f32,
    alpha: u8,
) {
    let mut placed: Vec<PositionedCharGlyph> = Vec::with_capacity(text.len());
    let mut caret_x = 0.0f32;
    let mut prev: Option<(usize, GlyphId)> = None;

    for ch in text.chars() {
        let (font_idx, glyph_id) = fonts.resolve_char(ch);
        let scaled = fonts.font(font_idx).as_scaled(scale);
        if let Some((prev_font_idx, prev_id)) = prev {
            if prev_font_idx == font_idx {
                caret_x += scaled.kern(prev_id, glyph_id);
            }
        }
        placed.push(PositionedCharGlyph {
            font_idx,
            glyph_id,
            x_offset: caret_x,
        });
        caret_x += scaled.h_advance(glyph_id);
        prev = Some((font_idx, glyph_id));
    }

    let line_w = caret_x;
    let start_x = match alignment {
        Alignment::Left => margin_x,
        Alignment::Right => (width as f32 - margin_x - line_w).max(margin_x),
        Alignment::Center => (width as f32 - line_w) * 0.5,
    };

    let primary_scaled = fonts.primary().as_scaled(scale);
    let baseline_y = center_y + (primary_scaled.ascent() + primary_scaled.descent()) * 0.5;
    let karaoke_x = start_x + line_w * sub_karaoke_progress.clamp(0.0, 1.0);
    let unlit_alpha = ((alpha as f32) * 0.42).round() as u8;

    let w_i32 = width as i32;
    let h_i32 = height as i32;

    for item in placed {
        let scaled = fonts.font(item.font_idx).as_scaled(scale);
        let glyph = item
            .glyph_id
            .with_scale_and_position(scale, point(start_x + item.x_offset, baseline_y));

        if let Some(outlined) = scaled.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            let min_x = bounds.min.x as i32;
            let min_y = bounds.min.y as i32;

            outlined.draw(|gx, gy, coverage| {
                let px = min_x + gx as i32;
                let py = min_y + gy as i32;
                if px < 0 || px >= w_i32 || py < 0 || py >= h_i32 {
                    return;
                }

                let target_alpha = if !karaoke {
                    alpha
                } else {
                    let px_f = px as f32;
                    let edge_t = ((karaoke_x - px_f + 3.0) / 6.0).clamp(0.0, 1.0);
                    (unlit_alpha as f32 + (alpha as f32 - unlit_alpha as f32) * edge_t).round()
                        as u8
                };

                let cov_alpha = ((target_alpha as f32) * coverage).round() as u8;
                if cov_alpha == 0 {
                    return;
                }

                let idx = ((py as usize) * (width as usize) + (px as usize)) * 4;
                buf[idx] = 255;
                buf[idx + 1] = 255;
                buf[idx + 2] = 255;
                if cov_alpha > buf[idx + 3] {
                    buf[idx + 3] = cov_alpha;
                }
            });
        }
    }
}

/// Wrap text by pixel width, handling both space-delimited words and unspaced CJK text.
fn wrap_text_by_pixel_width(
    fonts: &mut FontStack,
    scale: PxScale,
    text: &str,
    max_width: f32,
) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_w = 0.0f32;

    let mut word_buf = String::new();
    let mut word_w = 0.0f32;

    let flush_word = |word_buf: &mut String,
                      word_w: &mut f32,
                      current: &mut String,
                      current_w: &mut f32,
                      lines: &mut Vec<String>| {
        if word_buf.is_empty() {
            return;
        }
        if *current_w + *word_w > max_width && !current.is_empty() {
            lines.push(std::mem::take(current));
            *current_w = 0.0;
        }
        current.push_str(word_buf);
        *current_w += *word_w;
        word_buf.clear();
        *word_w = 0.0;
    };

    for ch in text.chars() {
        let ch_w = char_advance_px(fonts, scale, ch);
        if ch.is_whitespace() {
            flush_word(
                &mut word_buf,
                &mut word_w,
                &mut current,
                &mut current_w,
                &mut lines,
            );
            if !current.is_empty() && current_w + ch_w <= max_width {
                current.push(' ');
                current_w += ch_w;
            }
        } else if is_cjk_breakable(ch) {
            flush_word(
                &mut word_buf,
                &mut word_w,
                &mut current,
                &mut current_w,
                &mut lines,
            );
            if current_w + ch_w > max_width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_w = 0.0;
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

pub fn is_cjk_breakable(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3000..=0x30FF // CJK Symbols, Hiragana, Katakana
        | 0x31F0..=0x31FF // Katakana Phonetic Extensions
        | 0x3400..=0x4DBF // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xAC00..=0xD7AF // Hangul Syllables
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0xFF00..=0xFFEF // Halfwidth and Fullwidth Forms
        | 0x20000..=0x2FA1F // CJK Unified Ideographs Extensions B..F
    )
}

/// Build a lightweight, lazy-loaded `FontStack`:
/// - Only the **single primary font** (plus the zero-allocation static `UBUNTU_LIGHT` slice)
///   is loaded at startup (~0.5 MB).
/// - All fallback fonts (CJK, symbols) are kept as `PathBuf`s in `pending_paths` (0 heap bytes
///   until needed) and prefer single-face `.otf`/`.ttf` files (`IBMPlexSansJP-Regular.otf` 2.5 MB,
///   `IBMPlexSansKR-Regular.otf` 1.1 MB, `IBMPlexSansSC-Regular.otf` 5 MB) over 120+ MB `.ttc` bundles.
fn load_font_stack(
    custom_path: &str,
    font_family: &str,
    fallback_fonts: &[String],
) -> FontStack {
    let mut loaded: Vec<FontArc> = Vec::with_capacity(2);
    let mut pending_paths: Vec<PathBuf> = Vec::new();
    let mut seen_paths: BTreeSet<PathBuf> = BTreeSet::new();

    let all_font_files = discover_system_font_files();

    // 1. Resolve and load ONLY the single primary font immediately
    let mut primary_candidates: Vec<PathBuf> = Vec::new();
    let trimmed_path = custom_path.trim();
    if !trimmed_path.is_empty() {
        let expanded = crate::config::expand_tilde(&PathBuf::from(trimmed_path));
        if expanded.is_file() {
            primary_candidates.push(expanded);
        }
    }
    let trimmed_family = font_family.trim();
    if !trimmed_family.is_empty() {
        if let Some(best) = pick_best_font_path(trimmed_family, &all_font_files, &seen_paths) {
            primary_candidates.push(best);
        }
    }
    if let Some(best_default) =
        pick_best_font_path("notosans-regular", &all_font_files, &seen_paths)
    {
        primary_candidates.push(best_default);
    }

    for cand in primary_candidates {
        if seen_paths.contains(&cand) {
            continue;
        }
        if let Ok(bytes) = fs::read(&cand) {
            if let Ok(font) = FontArc::try_from_vec(bytes) {
                seen_paths.insert(cand);
                loaded.push(font);
                break; // Load ONLY ONE primary font at startup!
            }
        }
    }

    // Always include the zero-heap-allocation static embedded Latin fallback
    if let Ok(embedded) = FontArc::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT) {
        loaded.push(embedded);
    }

    // 2. Collect single best PathBuf per user-configured fallback font (lazy-loaded on demand)
    for fb in fallback_fonts {
        let fb_trimmed = fb.trim();
        if !fb_trimmed.is_empty() {
            if let Some(best) = pick_best_font_path(fb_trimmed, &all_font_files, &seen_paths) {
                seen_paths.insert(best.clone());
                pending_paths.push(best);
            }
        }
    }

    // 3. Collect single best PathBuf per lightweight CJK & Symbol safety pattern (lazy-loaded on demand).
    //    Ordered from smallest standalone .otf/.ttf files (1-5 MB) to larger collections.
    let safety_patterns: &[&str] = &[
        "ibmplexsansjp-regular",
        "ibmplexsanskr-regular",
        "ibmplexsanssc-regular",
        "ibmplexsanstc-regular",
        "notosanssymbols-regular",
        "notosanssymbols2-regular",
        "dejavusans",
        "wqy-zenhei",
        "unifont",
        "notosanscjk-regular",
    ];

    for pattern in safety_patterns {
        if let Some(best) = pick_best_font_path(pattern, &all_font_files, &seen_paths) {
            seen_paths.insert(best.clone());
            pending_paths.push(best);
        }
    }

    // Ensure compact standalone font files (<= 25 MB, e.g. IBMPlexSansJP 2.5 MB, IBMPlexSansKR 1.1 MB)
    // are always tried before giant multi-hundred-MB .ttc bundles.
    pending_paths.sort_by_key(|p| {
        let size = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        if size <= 8 * 1024 * 1024 {
            0u8
        } else if size <= 25 * 1024 * 1024 {
            1u8
        } else {
            2u8
        }
    });

    FontStack {
        loaded,
        pending_paths,
        missing_chars: BTreeSet::new(),
    }
}

/// Select the SINGLE best font file for `spec`, strongly preferring:
/// 1. Exact `<spec>-regular.ttf`/`.otf` matches over broad substring matches (so `"Noto Sans"`
///    picks `NotoSans-Regular.ttf` ~500 KB and NEVER matches `NotoSansCJK-Regular.ttc` 120 MB).
/// 2. Standalone `.ttf`/`.otf` files (`< 20 MB`) over huge multi-language `.ttc` bundles.
fn pick_best_font_path(
    spec: &str,
    all_font_files: &[PathBuf],
    seen_paths: &BTreeSet<PathBuf>,
) -> Option<PathBuf> {
    let as_path = crate::config::expand_tilde(&PathBuf::from(spec));
    if as_path.is_file() && !seen_paths.contains(&as_path) {
        return Some(as_path);
    }

    let needle = normalize_font_token(spec);
    if needle.is_empty() {
        return None;
    }

    // Also try querying `fc-match` for exact fontconfig family / monospace alias resolution
    if let Some(fc_path) = query_fc_match(spec, &needle, seen_paths) {
        return Some(fc_path);
    }

    let needle_wants_cjk = needle.contains("cjk")
        || needle.contains("jp")
        || needle.contains("kr")
        || needle.contains("sc")
        || needle.contains("tc")
        || needle.contains("zenhei");

    let mut best_score = i32::MIN;
    let mut best_path: Option<PathBuf> = None;

    for file_path in all_font_files {
        if seen_paths.contains(file_path) {
            continue;
        }
        let Some(fname) = file_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let norm_fname = normalize_font_token(fname);
        let norm_full = normalize_font_token(&file_path.to_string_lossy());

        if norm_fname.contains("italic") || norm_fname.contains("oblique") {
            continue;
        }

        let matches_fname = norm_fname.contains(&needle);
        let matches_dir = !matches_fname && norm_full.contains(&needle);
        if !matches_fname && !matches_dir {
            continue;
        }

        // Prevent a generic Latin request like "Noto Sans" from accidentally matching "NotoSansCJK"
        if !needle_wants_cjk && norm_fname.contains("cjk") {
            continue;
        }

        let mut score: i32 = 0;
        if matches_fname {
            score += 100;
        } else {
            score += 30;
        }

        let stem = Path::new(fname)
            .file_stem()
            .and_then(|s| s.to_str())
            .map(normalize_font_token)
            .unwrap_or_default();

        if stem == needle
            || stem == format!("{}regular", needle)
            || stem == format!("{}nfregular", needle)
            || stem == format!("{}nerdfontregular", needle)
        {
            score += 200;
        } else if stem.starts_with(&needle) && norm_fname.contains("regular") {
            score += 120;
        } else if norm_fname.contains("regular") {
            score += 60;
        } else if norm_fname.contains("text") || norm_fname.contains("medium") {
            score += 30;
        } else if norm_fname.contains("bold")
            || norm_fname.contains("black")
            || norm_fname.contains("thin")
            || norm_fname.contains("light")
            || norm_fname.contains("condensed")
        {
            score -= 80;
        }

        // Strongly prefer compact .ttf/.otf files over giant multi-hundred-MB .ttc collections
        let is_ttc = fname.to_ascii_lowercase().ends_with(".ttc");
        if is_ttc {
            score -= 90;
        }
        if let Ok(meta) = fs::metadata(file_path) {
            let size_mb = meta.len() / (1024 * 1024);
            if size_mb > 30 {
                score -= 150;
            } else if size_mb <= 8 {
                score += 40;
            }
        }

        if score > best_score {
            best_score = score;
            best_path = Some(file_path.clone());
        }
    }

    best_path
}

fn query_fc_match(
    spec: &str,
    needle: &str,
    seen_paths: &BTreeSet<PathBuf>,
) -> Option<PathBuf> {
    let pattern = format!("{}:style=Regular:slant=0:weight=80", spec.trim());
    let output = std::process::Command::new("fc-match")
        .args(["-f", "%{family}\n%{file}\n", &pattern])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let matched_family = lines.next()?.trim();
    let matched_file = lines.next()?.trim();
    if matched_file.is_empty() {
        return None;
    }

    let path = PathBuf::from(matched_file);
    if !path.is_file() || seen_paths.contains(&path) {
        return None;
    }

    let is_generic_alias = matches!(
        needle,
        "monospace" | "mono" | "sansserif" | "sans" | "serif" | "systemui"
    );
    let norm_matched_fam = normalize_font_token(matched_family);
    let norm_matched_file = normalize_font_token(matched_file);

    if is_generic_alias
        || norm_matched_fam.contains(needle)
        || needle.contains(&norm_matched_fam)
        || norm_matched_file.contains(needle)
    {
        // Avoid returning a >30 MB .ttc bundle from fc-match unless explicitly requested
        let needle_wants_cjk = needle.contains("cjk")
            || needle.contains("jp")
            || needle.contains("kr")
            || needle.contains("sc")
            || needle.contains("tc");
        if !needle_wants_cjk {
            if let Ok(meta) = fs::metadata(&path) {
                if meta.len() > 30 * 1024 * 1024 {
                    return None;
                }
            }
        }
        return Some(path);
    }

    None
}

fn normalize_font_token(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn discover_system_font_files() -> Vec<PathBuf> {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();

    for conf_path in ["/etc/fonts/fonts.conf", "/etc/fonts/local.conf"] {
        collect_fontconfig_dirs(Path::new(conf_path), &mut dirs);
    }
    if let Ok(entries) = fs::read_dir("/etc/fonts/conf.d") {
        for entry in entries.filter_map(|e| e.ok()) {
            collect_fontconfig_dirs(&entry.path(), &mut dirs);
        }
    }

    for std_dir in [
        "/run/current-system/sw/share/X11/fonts",
        "/usr/share/fonts",
        "/usr/local/share/fonts",
    ] {
        let p = Path::new(std_dir);
        if p.is_dir() {
            dirs.insert(p.to_path_buf());
        }
    }

    if let Ok(home) = std::env::var("HOME") {
        let user_fonts = PathBuf::from(&home).join(".local/share/fonts");
        if user_fonts.is_dir() {
            dirs.insert(user_fonts);
        }
        let nix_profile_fonts = PathBuf::from(&home).join(".nix-profile/share/fonts");
        if nix_profile_fonts.is_dir() {
            dirs.insert(nix_profile_fonts);
        }
    }

    let mut files = Vec::new();
    for dir in dirs {
        for entry in WalkDir::new(dir)
            .follow_links(true)
            .max_depth(5)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ext.eq_ignore_ascii_case("ttf")
                    || ext.eq_ignore_ascii_case("otf")
                    || ext.eq_ignore_ascii_case("ttc")
                {
                    files.push(path.to_path_buf());
                }
            }
        }
    }
    files
}

fn collect_fontconfig_dirs(conf_file: &Path, dirs: &mut BTreeSet<PathBuf>) {
    let Ok(content) = fs::read_to_string(conf_file) else {
        return;
    };
    let mut rest = content.as_str();
    while let Some(start) = rest.find("<dir") {
        let after_dir = &rest[start..];
        let Some(gt) = after_dir.find('>') else {
            break;
        };
        let after_gt = &after_dir[gt + 1..];
        let Some(end) = after_gt.find("</dir>") else {
            break;
        };
        let dir_str = after_gt[..end].trim();
        if dir_str.starts_with('/') {
            let p = PathBuf::from(dir_str);
            if p.is_dir() {
                dirs.insert(p);
            }
        }
        rest = &after_gt[end + 6..];
    }
}

fn encode_base64_append(input: &[u8], out: &mut Vec<u8>) {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(input.len().div_ceil(3) * 4);

    let mut chunks = input.chunks_exact(3);
    for c in &mut chunks {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        out.push(TABLE[((n >> 18) & 0x3F) as usize]);
        out.push(TABLE[((n >> 12) & 0x3F) as usize]);
        out.push(TABLE[((n >> 6) & 0x3F) as usize]);
        out.push(TABLE[(n & 0x3F) as usize]);
    }

    let rem = chunks.remainder();
    if rem.len() == 1 {
        let n = (rem[0] as u32) << 16;
        out.push(TABLE[((n >> 18) & 0x3F) as usize]);
        out.push(TABLE[((n >> 12) & 0x3F) as usize]);
        out.push(b'=');
        out.push(b'=');
    } else if rem.len() == 2 {
        let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 0x3F) as usize]);
        out.push(TABLE[((n >> 12) & 0x3F) as usize]);
        out.push(TABLE[((n >> 6) & 0x3F) as usize]);
        out.push(b'=');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use std::time::Duration;

    #[test]
    fn test_cjk_and_multilingual_glyph_resolution() {
        let s = Settings::default();
        let mut fonts = load_font_stack(&s.font_path, &s.font_family, &s.fallback_fonts);
        assert!(!fonts.is_empty(), "Font stack must not be empty");
        assert!(
            fonts.loaded.len() <= 2,
            "At startup, only primary font + embedded fallback should be loaded in RAM"
        );

        let has_system_cjk_candidates = !fonts.pending_paths.is_empty();
        let samples: &[&str] = if has_system_cjk_candidates {
            &["夜に駆ける", "アイネクライネ", "你好世界", "안녕하세요", "Hello ♪"]
        } else {
            &["Hello Café"]
        };

        for sample in samples {
            for ch in sample.chars() {
                if ch.is_whitespace() {
                    continue;
                }
                let (_idx, glyph_id) = fonts.resolve_char(ch);
                assert_ne!(
                    glyph_id.0, 0,
                    "Character '{}' (U+{:04X}) should resolve to a real glyph, not .notdef tofu",
                    ch, ch as u32
                );
            }
        }
    }

    #[test]
    fn test_guaranteed_buffer_prevents_overlap_on_large_wrapped_lines() {
        let mut s = Settings::default();
        s.font_size_active = 48.0;
        s.font_size_inactive = 22.0;
        s.pixel_line_spacing = 30.0;
        s.active_lyric_buffer_px = 20.0;
        s.inactive_lyric_buffer_px = 10.0;

        let mut fonts = load_font_stack(&s.font_path, &s.font_family, &s.fallback_fonts);
        let parsed = ParsedLrc::parse(
            "[00:01.00]I might get a 'Rari, 200 I floor it\n\
             [00:05.00]Y'all get the message, this is a very long active lyric line that wraps onto multiple sublines at 48px font size\n\
             [00:10.00]Better keep up, yeah, that's a foreign",
        );
        let sync = parsed.sync_state(Duration::from_secs(6), 0);
        let mut ui = UiState::from_settings(&s);
        ui.scroll_line = 1.0;

        let blocks = compute_non_overlapping_layout(&mut fonts, &parsed, &sync, &ui, 360.0, 300.0);
        assert_eq!(blocks.len(), 3);
        assert!(
            blocks[1].wrapped_lines.len() >= 2,
            "Active line should wrap onto multiple sublines"
        );

        // Verify that when line 1 is unfocused (active_index = Some(0), scroll_line = 0.0),
        // its wrapped subline count and relative inter-line spacing do NOT warp or change!
        let sync_unfocused = parsed.sync_state(Duration::from_secs(2), 0);
        let mut ui_unfocused = UiState::from_settings(&s);
        ui_unfocused.scroll_line = 0.0;
        let blocks_unfocused = compute_non_overlapping_layout(
            &mut fonts,
            &parsed,
            &sync_unfocused,
            &ui_unfocused,
            360.0,
            300.0,
        );

        assert_eq!(
            blocks[1].wrapped_lines,
            blocks_unfocused[1].wrapped_lines,
            "Wrapped line breaks must remain identical when focused vs unfocused"
        );
        let dist_focused = blocks[1].center_y - blocks[0].center_y;
        let dist_unfocused = blocks_unfocused[1].center_y - blocks_unfocused[0].center_y;
        assert!(
            (dist_focused - dist_unfocused).abs() < 1e-4,
            "Inter-line distance must remain rigid and invariant across focus changes"
        );

        let bottom_0 = blocks[0].center_y + 0.5 * blocks[0].total_height;
        let top_1 = blocks[1].center_y - 0.5 * blocks[1].total_height;
        let bottom_1 = blocks[1].center_y + 0.5 * blocks[1].total_height;
        let top_2 = blocks[2].center_y - 0.5 * blocks[2].total_height;

        assert!(
            top_1 - bottom_0 >= s.active_lyric_buffer_px - 0.01,
            "Gap above active lyric ({}) must be >= active_lyric_buffer_px ({})",
            top_1 - bottom_0,
            s.active_lyric_buffer_px
        );
        assert!(
            top_2 - bottom_1 >= s.active_lyric_buffer_px - 0.01,
            "Gap below active lyric ({}) must be >= active_lyric_buffer_px ({})",
            top_2 - bottom_1,
            s.active_lyric_buffer_px
        );
    }

    #[test]
    fn test_monospace_font_loading_maple_mono() {
        let s = Settings::default();
        let mut all_files = discover_system_font_files();
        if all_files.is_empty() {
            all_files = vec![
                PathBuf::from("/nix/store/fake/share/fonts/truetype/MapleMono-NF-Bold.ttf"),
                PathBuf::from("/nix/store/fake/share/fonts/truetype/MapleMono-NF-Italic.ttf"),
                PathBuf::from("/nix/store/fake/share/fonts/truetype/MapleMono-NF-Regular.ttf"),
            ];
        }
        let seen = BTreeSet::new();
        if let Some(picked) = pick_best_font_path("Maple Mono NF", &all_files, &seen) {
            let fname = picked.file_name().unwrap().to_str().unwrap();
            assert_eq!(fname, "MapleMono-NF-Regular.ttf");
        }

        let mut fonts = load_font_stack("", "Maple Mono NF", &s.fallback_fonts);
        assert!(!fonts.is_empty());
        let (idx, gid) = fonts.resolve_char('A');
        assert_eq!(idx, 0);
        assert_ne!(gid.0, 0);
    }
}
