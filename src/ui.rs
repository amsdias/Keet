use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;
use std::sync::atomic::Ordering;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;

use crate::state::{
    PlayerState, VizMode, VizStyle,
    C_RESET, C_BOLD, C_DIM, C_CYAN, C_GREEN, C_YELLOW, C_MAGENTA, C_RED,
    ViewMode, InputMode, UiState,
};
use crate::viz::{
    StatsMonitor, VizAnalyser, render_vu_meter, render_spectrum_horizontal,
    render_spectrum_vertical, render_oscilloscope, render_lissajous,
    render_spectrogram, render_spectrogram_analysis, get_viz_line_count,
    analysis_needs_raw_lines, analysis_rows_for_window,
};

pub fn format_time(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    if total >= 3600 {
        // Audiobooks and long mixes: h:mm:ss instead of rolling minutes past 60.
        format!("{}:{:02}:{:02}", total / 3600, (total % 3600) / 60, total % 60)
    } else {
        format!("{:02}:{:02}", total / 60, total % 60)
    }
}

fn icon_color_for_ext(ext: &str) -> &'static str {
    match ext {
        "mp3"          => C_GREEN,
        "ogg"          => C_MAGENTA,
        "aac" | "m4a"  => C_RED,
        "flac"         => C_CYAN,
        "alac"         => C_CYAN,
        "aiff" | "aif" => C_CYAN,
        "wav"          => C_YELLOW,
        _              => C_GREEN,
    }
}

use crate::ansi::{truncate_ansi, truncate_plain, visible_len};

/// Counts the frame lines actually emitted below the first (anchor) row, so
/// the caller's cursor-up math is derived from what was printed instead of
/// predicted by hand — predicted counts drifting from printed reality was a
/// recurring off-by-one source (the next frame's cursor-up then lands
/// mid-frame and the layout smears).
pub(crate) struct FrameWriter {
    below_first: usize,
}

impl FrameWriter {
    pub(crate) fn new() -> Self {
        Self { below_first: 0 }
    }

    /// Print the frame's first line in place (carriage return + erase): the
    /// anchor row the next frame's cursor-up returns to. Not counted.
    pub(crate) fn first_line(&mut self, s: &str) {
        print!("\r\x1B[K{}", s);
    }

    /// Advance one line and print with erase-to-EOL.
    pub(crate) fn line(&mut self, s: &str) {
        print!("\n\r\x1B[K{}", s);
        self.below_first += 1;
    }

    /// Advance one line and print WITHOUT erase — sixel blocks erase
    /// themselves, and an EL here would wipe that row's slice of the image.
    pub(crate) fn line_raw(&mut self, s: &str) {
        print!("\n\r{}", s);
        self.below_first += 1;
    }

    /// Lines emitted below the anchor row this frame.
    pub(crate) fn count(&self) -> usize {
        self.below_first
    }
}

/// Top-level renderer dispatcher. Routes by the active theme: Minimal and HiFi
/// each own a full set of view renderers in `ui_minimal` / `ui_hifi`; Classic
/// (the default) falls through to `print_status_classic` below. Every renderer
/// honours the same contract — return the number of lines drawn below the
/// anchor row (line 1) so the caller's cursor-up math stays exact.
#[allow(clippy::too_many_arguments)] // cohesive render context; bundling into a struct adds no clarity
pub fn print_status(state: &PlayerState, ui: &mut UiState, name: &str, track_info: &str, ext: &str, eq_preset: &crate::eq::EqPreset, fx_name: &str, cf_name: &str, stats: &mut StatsMonitor, prev_frame_lines: usize, playlist: &[PathBuf], analyser: &VizAnalyser) -> usize {
    use crate::theme::ThemeKind;
    // Keep the live EQ gains mirroring the selected preset while not editing, so
    // the curve/editor show the active shape and edits start from it.
    if !state.is_eq_custom() {
        state.set_eq_gains(&eq_preset.gains_10());
    }
    // The EQ+FX editor is one shared, palette-driven screen across all themes.
    if ui.view_mode == ViewMode::Eq {
        return print_status_eq_view(state, ui, eq_preset, fx_name, cf_name, prev_frame_lines);
    }
    if state.theme_kind() == ThemeKind::Minimal {
        match ui.view_mode {
            ViewMode::Player => {
                return crate::ui_minimal::print_status_minimal(state, ui, name, track_info, eq_preset, fx_name, cf_name, stats, prev_frame_lines, analyser);
            }
            ViewMode::Playlist => {
                return crate::ui_minimal::print_status_minimal_library(state, ui, name, prev_frame_lines, playlist);
            }
            ViewMode::Lyrics => {
                return crate::ui_minimal::print_status_minimal_lyrics(state, ui, name, prev_frame_lines);
            }
            ViewMode::Eq => unreachable!("EQ view handled above"),
        }
    }
    if state.theme_kind() == ThemeKind::HiFi {
        match ui.view_mode {
            ViewMode::Player => {
                return crate::ui_hifi::print_status_hifi(state, ui, name, eq_preset, fx_name, cf_name, stats, prev_frame_lines);
            }
            ViewMode::Playlist => {
                return crate::ui_hifi::print_status_hifi_library(state, ui, name, prev_frame_lines, playlist);
            }
            ViewMode::Lyrics => {
                return crate::ui_hifi::print_status_hifi_lyrics(state, ui, name, prev_frame_lines);
            }
            ViewMode::Eq => unreachable!("EQ view handled above"),
        }
    }
    print_status_classic(state, ui, name, track_info, ext, eq_preset, fx_name, cf_name, stats, prev_frame_lines, playlist, analyser)
}

/// The EQ+FX editor screen — one shared renderer for all themes (palette-driven).
fn print_status_eq_view(
    state: &PlayerState,
    ui: &mut UiState,
    eq_preset: &crate::eq::EqPreset,
    fx_name: &str,
    cf_name: &str,
    prev_frame_lines: usize,
) -> usize {
    let kind = state.theme_kind();
    let p = crate::theme::palette(kind);
    let knob = match kind {
        crate::theme::ThemeKind::Minimal => '●',
        crate::theme::ThemeKind::HiFi => '◆',
        crate::theme::ThemeKind::Classic => '█',
    };
    let (term_w, term_h) = terminal::size()
        .map(|(w, h)| (w as usize, h as usize))
        .unwrap_or((120, 40));

    if prev_frame_lines != usize::MAX && prev_frame_lines > 0 {
        print!("\x1B[{}F", prev_frame_lines);
    }

    let title = if state.is_eq_custom() {
        "Custom".to_string()
    } else {
        eq_preset.name.clone()
    };
    let bal = state.balance_value();
    let bal_str = if bal == 0 {
        "centred".to_string()
    } else if bal < 0 {
        format!("L{}%", -bal)
    } else {
        format!("R{}%", bal)
    };
    let rg_str = match state.rg_mode() {
        crate::state::RgMode::Album => "album",
        crate::state::RgMode::Off => "off",
        crate::state::RgMode::Track => "track",
    };
    let readouts = [
        ("FX", fx_name),
        ("XFEED", cf_name),
        ("BAL", bal_str.as_str()),
        ("RG", rg_str),
    ];
    let gains = state.eq_gains_array();
    let body = crate::eq_ui::render_eq_screen(
        &gains,
        ui.eq_band,
        &title,
        &readouts,
        knob,
        p,
        term_w,
        term_h.saturating_sub(2),
    );

    // First body line is the anchor; the rest and the footer sit below it.
    if let Some(first) = body.first() {
        print!("\r\x1B[K{}", first);
    }
    let mut below = 0usize;
    for line in body.iter().skip(1) {
        print!("\n\r\x1B[K{}", line);
        below += 1;
    }
    print!(
        "\n\r\x1B[K  {dim}[←→] band   [↑↓] ±1 dB   [ [ / ] ] preset   [0] flatten band   [E/L/Esc] close{rst}",
        dim = p.dim, rst = p.reset,
    );
    below += 1;

    print!("\x1B[J");
    io::stdout().flush().ok();
    below
}

#[allow(clippy::too_many_arguments)] // cohesive render context; bundling into a struct adds no clarity
fn print_status_classic(state: &PlayerState, ui: &mut UiState, name: &str, track_info: &str, ext: &str, eq_preset: &crate::eq::EqPreset, fx_name: &str, cf_name: &str, stats: &mut StatsMonitor, prev_frame_lines: usize, playlist: &[PathBuf], analyser: &VizAnalyser) -> usize {
    let viz_mode = state.viz_mode();
    let viz_style = state.viz_style();
    let eq_name = &eq_preset.name;
    let eq_curve = crate::eq::render_eq_curve(&state.eq_gains_array());
    let eq_line = !eq_curve.is_empty();
    let (term_w, term_h) = terminal::size()
        .map(|(w, h)| (w as usize, h as usize))
        .unwrap_or((120, 40));
    // Clamp the analysis spectrogram (the tallest viz) so the full frame fits
    // the window: an overflowing frame makes every full repaint (viz switch,
    // track skip) scroll the banner top into scrollback.
    let rows_above_viz = ui.banner_lines + 2 + if eq_line { 1 } else { 0 };
    let ana_rows = analysis_rows_for_window(term_h, rows_above_viz);
    let viz_lines = if viz_mode == VizMode::SpectrogramAnalysis {
        ana_rows + 1
    } else {
        get_viz_line_count(viz_mode, viz_style)
    } + if eq_line { 1 } else { 0 };
    // Sixel emit-on-change bookkeeping: cleared every frame, re-asserted only
    // by the analysis branch below. Any frame that doesn't reach that branch
    // (playlist/lyrics view, another viz) may paint over the block, so the
    // next analysis render must re-emit instead of skipping.
    let block_was_intact = ui.spectro_block_intact;
    ui.spectro_block_intact = false;

    let track = state.current_track.load(Ordering::Relaxed) + 1;
    let total = state.total_tracks.load(Ordering::Relaxed);
    let icon = if state.is_paused() { "⏸" } else { "▶" };
    let icon_color = if state.is_paused() { C_YELLOW } else { C_GREEN };

    let cur = format_time(state.time_secs());
    let tot = format_time(state.total_secs());

    let progress = if state.total_secs() > 0.0 {
        (state.time_secs() / state.total_secs()).min(1.0)
    } else { 0.0 };

    let bar_w = 20;
    let sub = progress * bar_w as f64;
    let full = sub as usize;
    let bar_filled = match viz_style {
        VizStyle::Dots => {
            let frac = ((sub - full as f64) * 6.0) as usize;
            const PARTIALS: &[char] = &['⣀', '⣄', '⣤', '⣦', '⣶', '⣷'];
            format!("{}{}{}",
                "⣿".repeat(full),
                if full < bar_w { String::from(PARTIALS[frac.min(5)]) } else { String::new() },
                "⣀".repeat(bar_w.saturating_sub(full + 1)))
        }
        VizStyle::Bars => {
            let frac = ((sub - full as f64) * 8.0) as usize;
            const PARTIALS: &[char] = &['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
            let mut s = String::new();
            s.push_str(&"█".repeat(full));
            if full < bar_w {
                if frac > 0 {
                    s.push(PARTIALS[(frac - 1).min(7)]);
                    s.push_str(C_DIM);
                    s.push_str(&"▏".repeat(bar_w - full - 1));
                } else {
                    s.push_str(C_DIM);
                    s.push_str(&"▏".repeat(bar_w - full));
                }
            }
            s
        }
    };

    let buf = state.buffer_level.load(Ordering::Relaxed);
    let ring_cap = state.ring_capacity.load(Ordering::Relaxed).max(1);
    let raw_buf_pct = buf as f32 / ring_cap as f32 * 100.0;
    stats.update_buf(raw_buf_pct);
    let buf_pct = stats.smoothed_buf_pct as u32;

    // Truncate name to fit: leave room for track counter, icon, and track info
    // Format: "[N/M] ♪ NAME INFO" — overhead is ~10 + track_info.len()
    let overhead = format!("[{track}/{total}] ♪  ").len() + track_info.len() + 1;
    let max_name = term_w.saturating_sub(overhead).min(35);
    let display_name = truncate_plain(name, max_name);

    // Move cursor back to the frame's anchor row (single atomic escape).
    // prev_frame_lines = lines the previous frame emitted below its first
    // row, as counted by FrameWriter — derived, not predicted.
    if prev_frame_lines != usize::MAX && prev_frame_lines > 0 {
        print!("\x1B[{}F", prev_frame_lines); // CPL: up N lines, column 1
    }
    let mut w = FrameWriter::new();

    // Line 1: Track info (truncated to terminal width)
    let ic = icon_color_for_ext(ext);
    let line1 = format!("{C_DIM}[{track}/{total}]{C_RESET} {ic}♪{C_RESET} {C_BOLD}{C_CYAN}{display_name}{C_RESET} {C_DIM}{track_info}{C_RESET}");
    w.first_line(&truncate_ansi(&line1, term_w));

    // Line 2: Progress (truncated to terminal width)
    let vol = state.volume.load(Ordering::Relaxed);
    let fader = if state.is_pre_fader() { "pre" } else { "post" };
    let eq_display = if eq_name == "Flat" { String::new() } else { format!(" eq:{}", eq_name) };
    let fx_display = if fx_name == "None" { String::new() } else { format!(" fx:{}", fx_name) };
    let cf_display = if cf_name != "Off" { format!(" cf:{}", cf_name) } else { String::new() };
    let clip_display = if state.is_clipping() {
        format!(" {C_RED}●{C_RESET}")
    } else {
        format!(" {C_GREEN}●{C_RESET}")
    };
    let bal = state.balance_value();
    let bal_display = if bal != 0 {
        if bal < 0 { format!(" BAL:L{}%", -bal) } else { format!(" BAL:R{}%", bal) }
    } else { String::new() };
    let next_viz = match viz_mode.next() {
        VizMode::None => "Off",
        VizMode::VuMeter => "VU",
        VizMode::SpectrumHorizontal => "SpecH",
        VizMode::SpectrumVertical => "SpecV",
        VizMode::Oscilloscope => "Scope",
        VizMode::Lissajous => "Vector",
        VizMode::Spectrogram => "SpecGram",
        VizMode::SpectrogramAnalysis => "SpecAna",
    };
    let next_style = if viz_mode == VizMode::SpectrogramAnalysis {
        if matches!(viz_style, VizStyle::Dots) { "Linear" } else { "Log" }
    } else {
        match viz_style { VizStyle::Dots => "Bars", VizStyle::Bars => "Dots" }
    };
    let stats_display = if state.show_stats() {
        format!(" cpu:{:.1}% mem:{:.0}M", stats.cpu_usage, stats.memory_mb)
    } else {
        String::new()
    };
    let line2 = format!("  {icon_color}{icon}{C_RESET} {C_BOLD}[{cur}/{tot}]{C_RESET} {C_GREEN}{bar_filled}{C_RESET} {C_DIM}vol:{vol}%{eq_display}{fx_display}{cf_display}{clip_display}{bal_display} {fader} buf:{buf_pct}%{stats_display} {{V}}:{next_viz} {{B}}:{next_style}{C_RESET}");
    w.line(&truncate_ansi(&line2, term_w));

    // EQ curve visualization (when non-Flat preset is active)
    if eq_line {
        w.line(&eq_curve);
    }

    // Separation line and content area
    if ui.view_mode == ViewMode::Playlist {
        let term_h = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);
        let header_lines = 2 + if eq_line { 1 } else { 0 };
        let footer_lines = 2; // separator + footer
        let visible_rows = term_h.saturating_sub(header_lines + footer_lines + ui.banner_lines).max(1);
        ui.last_visible_rows = visible_rows;

        // Separator
        w.line(&format!("  {C_DIM}{}{C_RESET}", "─".repeat(term_w.saturating_sub(2))));

        if ui.library_tree_mode {
            let lines = render_tree_body(
                ui,
                visible_rows,
                term_w,
                crate::theme::palette(crate::theme::ThemeKind::Classic),
            );
            let n = lines.len();
            for line in &lines {
                w.line(line);
            }
            for _ in n..visible_rows {
                w.line("");
            }
        } else {
        let search_active = matches!(&ui.input_mode, InputMode::Search(q) if !q.is_empty());
        // Compute the item count without materializing the full index vector.
        // When the search filter is empty (and search inactive), iterate `0..playlist.len()`
        // virtually; otherwise iterate `ui.filtered_indices` directly.
        let items_len = if search_active && ui.filtered_indices.is_empty() {
            0
        } else if ui.filtered_indices.is_empty() {
            playlist.len()
        } else {
            ui.filtered_indices.len()
        };

        // Ensure cursor is visible with a scroll margin (scrolloff)
        let scroll_margin = 4.min(visible_rows / 2);

        if ui.cursor >= ui.scroll_offset + visible_rows.saturating_sub(scroll_margin) {
            ui.scroll_offset = ui.cursor.saturating_sub(visible_rows.saturating_sub(scroll_margin + 1));
        }
        if ui.cursor < ui.scroll_offset + scroll_margin {
            ui.scroll_offset = ui.cursor.saturating_sub(scroll_margin);
        }

        // Clamp offset to prevent overscroll empty padding at the bottom of the list
        let max_offset = items_len.saturating_sub(visible_rows);
        ui.scroll_offset = ui.scroll_offset.min(max_offset);

        if items_len == 0 && search_active {
            w.line(&format!("  {C_DIM}(no matches){C_RESET}"));
            for _ in 1..visible_rows {
                w.line("");
            }
        } else {
            let visible_count = visible_rows.min(items_len.saturating_sub(ui.scroll_offset));

            for row in 0..visible_count {
                let list_pos = ui.scroll_offset + row;
                let track_idx = if ui.filtered_indices.is_empty() {
                    list_pos
                } else {
                    ui.filtered_indices[list_pos]
                };
                let is_playing = track_idx == ui.current;
                let is_cursor = list_pos == ui.cursor;
                let fname = ui.metadata_cache.display_name(track_idx, &playlist[track_idx]);
                let album = ui.metadata_cache.album(track_idx).unwrap_or_default();
                let dur_str = match ui.metadata_cache.duration(track_idx) {
                    Some(d) => format_time(d),
                    None => String::new(),
                };

                let marker = if is_playing { "▶" } else { " " };
                let num = format!("{:>4}", track_idx + 1);
                // prefix: " ▶ 1234  " = 10 visible chars, dur + trailing space = dur_str.len() + 2
                let prefix_len = 10;
                let dur_col = if dur_str.is_empty() { 0 } else { dur_str.len() + 2 };
                let content_budget = term_w.saturating_sub(prefix_len + dur_col);
                // Reserve up to ~30% (or 32 chars max) for album, but only when
                // the row is wide enough to leave room for a meaningful name.
                let album_budget = if content_budget >= 50 {
                    (content_budget * 30 / 100).clamp(12, 32)
                } else {
                    0
                };
                let name_budget = content_budget.saturating_sub(if album_budget > 0 { album_budget + 2 } else { 0 });
                let truncated_name = truncate_plain(&fname, name_budget);
                let name_pad = name_budget.saturating_sub(visible_len(&truncated_name));
                let album_part = if album_budget > 0 {
                    let truncated_album = truncate_plain(&album, album_budget);
                    let album_pad = album_budget.saturating_sub(visible_len(&truncated_album));
                    format!("{}{C_DIM}{truncated_album}{C_RESET}{}", " ".repeat(name_pad + 2), " ".repeat(album_pad))
                } else {
                    " ".repeat(name_pad)
                };
                let dur_part = if dur_str.is_empty() {
                    String::new()
                } else {
                    format!(" {C_DIM}{dur_str}{C_RESET}")
                };

                let line = if is_cursor && is_playing {
                    format!(" {marker} \x1B[7m{C_GREEN}{num}  {truncated_name}{C_RESET}\x1B[7m{album_part}{dur_part}\x1B[27m")
                } else if is_cursor {
                    format!(" {marker} \x1B[7m{num}  {truncated_name}{album_part}{dur_part}\x1B[27m")
                } else if is_playing {
                    format!(" {marker} {C_GREEN}{num}  {truncated_name}{C_RESET}{album_part}{dur_part}")
                } else {
                    format!(" {marker} {C_DIM}{num}{C_RESET}  {truncated_name}{album_part}{dur_part}")
                };

                w.line(&line);
            }

            // Pad remaining rows
            for _ in visible_count..visible_rows {
                w.line("");
            }
        }
        }

        // Search prompt or hint line
        let footer = match &ui.input_mode {
            InputMode::Search(query) => {
                format!("  / {}{C_DIM}_{C_RESET}", query)
            }
            InputMode::SavePlaylist(name) => {
                format!("  Save playlist as: {}{C_DIM}_{C_RESET}", name)
            }
            InputMode::Normal => {
                if let Some(msg) = ui.active_status() {
                    format!("  {C_GREEN}{msg}{C_RESET}")
                } else if ui.library_tree_mode {
                    format!("  {C_DIM}[Tab] list  [←→] fold  [Enter] play  [/] filter  [D] remove  [L] close{C_RESET}")
                } else {
                    format!("  {C_DIM}[Tab] tree  [↑↓] scroll  [Enter] play  [A] enqueue  [/] search  [D] remove  [S] save{C_RESET}")
                }
            }
        };
        w.line(&truncate_ansi(&footer, term_w));

        print!("\x1B[J");
        io::stdout().flush().ok();
        return w.count();
    }

    // Lyrics view
    if ui.view_mode == ViewMode::Lyrics {
        let term_h = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);
        let header_lines = 2 + if eq_line { 1 } else { 0 };
        let footer_lines = 2;
        let visible_rows = term_h.saturating_sub(header_lines + footer_lines + ui.banner_lines).max(1);

        // Separator
        w.line(&format!("  {C_DIM}{}{C_RESET}", "─".repeat(term_w.saturating_sub(2))));

        if let Some(ref lyrics) = ui.lyrics {
            let total_lines = lyrics.line_count();
            let adjusted_time = state.time_secs() + ui.lyrics_offset;
            let current_line = lyrics.current_line(adjusted_time);

            // Auto-scroll for synced lyrics: center current line
            if lyrics.is_synced() && ui.lyrics_auto_scroll {
                if let Some(cur) = current_line {
                    let half = visible_rows / 2;
                    ui.lyrics_scroll = cur.saturating_sub(half);
                }
            }

            // Clamp scroll
            if total_lines > visible_rows {
                ui.lyrics_scroll = ui.lyrics_scroll.min(total_lines - visible_rows);
            } else {
                ui.lyrics_scroll = 0;
            }

            for row in 0..visible_rows {
                let line_idx = ui.lyrics_scroll + row;
                if line_idx < total_lines {
                    let text = lyrics.line_text(line_idx);
                    let is_current = current_line == Some(line_idx);
                    let line = if is_current {
                        format!("  {C_BOLD}{C_CYAN}{text}{C_RESET}")
                    } else {
                        format!("  {C_DIM}{text}{C_RESET}")
                    };
                    w.line(&truncate_ansi(&line, term_w));
                } else {
                    w.line("");
                }
            }
        } else {
            w.line(&format!("  {C_DIM}(no lyrics available){C_RESET}"));
            for _ in 1..visible_rows {
                w.line("");
            }
        }

        // Footer
        let is_synced = ui.lyrics.as_ref().map(|l| l.is_synced()).unwrap_or(false);
        let offset_display = if is_synced && ui.lyrics_offset != 0.0 {
            format!("  offset:{:+.1}s", ui.lyrics_offset)
        } else { String::new() };
        let sync_hint = if is_synced { "  [A/D] sync" } else { "" };
        let footer = format!("  {C_DIM}[Y] close  [W/S] scroll{sync_hint}{offset_display}{C_RESET}");
        w.line(&truncate_ansi(&footer, term_w));

        print!("\x1B[J");
        io::stdout().flush().ok();
        return w.count();
    }

    // Original Player mode rendering below
    if viz_mode != VizMode::None {
        w.line(&format!("  {C_DIM}{}{C_RESET}", "─".repeat(term_w.saturating_sub(2))));
    }

    match viz_mode {
        VizMode::None => {}
        VizMode::VuMeter => {
            for line in render_vu_meter(state, viz_style, term_w) {
                w.line(&line);
            }
        }
        VizMode::SpectrumHorizontal => {
            for line in render_spectrum_horizontal(state, viz_style) {
                w.line(&line);
            }
        }
        VizMode::SpectrumVertical => {
            for line in render_spectrum_vertical(state, viz_style) {
                w.line(&line);
            }
        }
        VizMode::Oscilloscope => {
            for line in render_oscilloscope(analyser, viz_style, term_w) {
                w.line(&line);
            }
        }
        VizMode::Lissajous => {
            for line in render_lissajous(analyser, viz_style, term_w) {
                w.line(&line);
            }
        }
        VizMode::Spectrogram => {
            for line in render_spectrogram(analyser, viz_style, term_w) {
                w.line(&line);
            }
        }
        VizMode::SpectrogramAnalysis => {
            // {B}/viz_style selects the frequency axis here: Dots = log, Bars = linear.
            let log_axis = matches!(viz_style, VizStyle::Dots);
            // Sixel: NO erase-to-EOL — the row-1 transmit paints the whole
            // block, and erasing the following rows would wipe the image down
            // to a 1-row strip. The sixel block erases itself before painting.
            let raw = analysis_needs_raw_lines();
            // Force a full re-emit when the screen was repainted from scratch
            // this frame (resize/viz-switch/skip path), when the block wasn't
            // ours last frame, or when the layout height changed (EQ curve
            // line toggling, transient status row) — the block moves rows
            // then, and a skipped emit would leave the image stranded at the
            // old position. Predicted height below the anchor = line2 (1) +
            // viz_lines; compared against the previous frame's derived count.
            let force = prev_frame_lines == usize::MAX
                || !block_was_intact
                || 1 + viz_lines != prev_frame_lines;
            ui.spectro_block_intact = true;
            for line in render_spectrogram_analysis(analyser, term_w, log_axis, state.is_paused(), ana_rows, force) {
                if raw { w.line_raw(&line); } else { w.line(&line); }
            }
        }
    }

    // Show status message in Player mode
    if let Some(msg) = ui.active_status() {
        w.line(&format!("  {C_GREEN}{msg}{C_RESET}"));
        print!("\x1B[J");
        io::stdout().flush().ok();
        return w.count();
    }

    print!("\x1B[J");
    io::stdout().flush().ok();
    w.count()
}

pub fn poll_input(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) -> bool {
    // Drain all pending events for responsive input
    while event::poll(Duration::ZERO).unwrap_or(false) {
        let ev = match event::read() { Ok(e) => e, Err(_) => continue };

        if let Event::Resize(_, _) = ev {
            ui.terminal_resized = true;
            // Probed cell metrics may be stale after a font-size change
            // (Ctrl+zoom also fires Resize). Too-small assumed cells make the
            // sixel image overflow its block — auto-scroll storm — so drop
            // back to the conservative floor; pixel-exact fill returns on the
            // next launch.
            crate::cover::set_cell_metrics(None);
            continue;
        }

        let k = match ev {
            Event::Key(k) => k,
            _ => continue,
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }

        // macOS terminals translate Cmd+Arrow (and similar shortcuts) into an ESC
        // byte followed by another char — crossterm hands us two separate events.
        // If a bare Esc is immediately followed by another pending event, treat
        // the pair as an unrecognized escape sequence and drop both. Human typing
        // rarely produces 0ms gaps, so a zero-duration poll returning true here
        // is a reliable signal.
        if k.code == KeyCode::Esc
            && k.modifiers.is_empty()
            && event::poll(Duration::ZERO).unwrap_or(false)
        {
            let _ = event::read();
            continue;
        }

            // In text input mode, route to text handler
            match &ui.input_mode {
                InputMode::Search(_) | InputMode::SavePlaylist(_) => {
                    return handle_text_input(state, ui, playlist, k);
                }
                InputMode::Normal => {}
            }

            // Lyrics view keys (when in Normal input mode)
            if ui.view_mode == ViewMode::Lyrics {
                match k {
                    KeyEvent { code: KeyCode::Char('w'), .. } => {
                        ui.lyrics_auto_scroll = false;
                        ui.lyrics_scroll = ui.lyrics_scroll.saturating_sub(1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('s'), .. } => {
                        ui.lyrics_auto_scroll = false;
                        if let Some(ref lyrics) = ui.lyrics {
                            let max = lyrics.line_count().saturating_sub(1);
                            if ui.lyrics_scroll < max {
                                ui.lyrics_scroll += 1;
                            }
                        }
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('d'), .. } => {
                        ui.lyrics_offset += 0.5;
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('a'), .. } => {
                        ui.lyrics_offset -= 0.5;
                        continue;
                    }
                    KeyEvent { code: KeyCode::Esc, .. } |
                    KeyEvent { code: KeyCode::Char('y'), .. } => {
                        ui.view_mode = ViewMode::Player;
                        continue;
                    }
                    _ => {} // Fall through to global keys
                }
            }

            // EQ editor keys: arrows select/adjust bands; brackets cycle presets.
            if ui.view_mode == ViewMode::Eq {
                match k {
                    KeyEvent { code: KeyCode::Left, .. } => {
                        ui.eq_band = ui.eq_band.saturating_sub(1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Right, .. } => {
                        if ui.eq_band + 1 < crate::eq::EQ_BANDS {
                            ui.eq_band += 1;
                        }
                        continue;
                    }
                    KeyEvent { code: KeyCode::Up, .. } => {
                        state.nudge_eq_gain(ui.eq_band, 1.0);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Down, .. } => {
                        state.nudge_eq_gain(ui.eq_band, -1.0);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('['), .. } => {
                        state.step_eq_preset(-1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char(']'), .. } => {
                        state.step_eq_preset(1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('0'), .. } => {
                        // Flatten the selected band to 0 dB.
                        let cur = state.eq_gains_array()[ui.eq_band];
                        state.nudge_eq_gain(ui.eq_band, -cur);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Esc, .. } => {
                        ui.view_mode = ViewMode::Player;
                        continue;
                    }
                    _ => {} // Fall through to global keys (E/L close, q, space, …)
                }
            }

            // Playlist view keys (when in Normal input mode)
            if ui.view_mode == ViewMode::Playlist {
                // Tab flips the library between the flat list and the artist→album tree.
                if matches!(k, KeyEvent { code: KeyCode::Tab, .. }) {
                    ui.library_tree_mode = !ui.library_tree_mode;
                    if ui.library_tree_mode {
                        ui.tree_dirty = true;
                    }
                    return false;
                }
                if ui.library_tree_mode {
                    // A staged bulk remove is awaiting confirmation: `y` removes,
                    // any other key cancels.
                    if let Some((label, indices)) = ui.tree_pending_remove.take() {
                        if matches!(k, KeyEvent { code: KeyCode::Char('y'), .. }) {
                            tree_remove_indices(state, ui, playlist, &indices);
                        } else {
                            ui.set_status(format!("cancelled removing {label}"));
                        }
                        return false;
                    }
                    match k {
                        KeyEvent { code: KeyCode::Up, .. } => { tree_move(ui, -1); continue; }
                        KeyEvent { code: KeyCode::Down, .. } => { tree_move(ui, 1); continue; }
                        KeyEvent { code: KeyCode::Left, .. } => { tree_collapse_under_cursor(ui); continue; }
                        KeyEvent { code: KeyCode::Right, .. } => { tree_expand_under_cursor(ui); continue; }
                        KeyEvent { code: KeyCode::Enter, .. } => {
                            if let Some(idx) = tree_cursor_play_index(ui) {
                                state.jump_to(idx);
                            }
                            return false;
                        }
                        KeyEvent { code: KeyCode::Char('/'), .. } => {
                            // Seed with the current filter so `/` edits, not resets it.
                            ui.input_mode = InputMode::Search(ui.tree_filter.clone());
                            return false;
                        }
                        KeyEvent { code: KeyCode::PageUp, .. } => { tree_page(ui, -1); continue; }
                        KeyEvent { code: KeyCode::PageDown, .. } => { tree_page(ui, 1); continue; }
                        KeyEvent { code: KeyCode::Char('u'), modifiers, .. }
                            if modifiers.contains(KeyModifiers::CONTROL) => { tree_page(ui, -1); continue; }
                        KeyEvent { code: KeyCode::Char('d'), modifiers, .. }
                            if modifiers.contains(KeyModifiers::CONTROL) => { tree_page(ui, 1); continue; }
                        KeyEvent { code: KeyCode::Char('d'), .. }
                        | KeyEvent { code: KeyCode::Delete, .. } => {
                            tree_remove_under_cursor(state, ui, playlist);
                            return false;
                        }
                        KeyEvent { code: KeyCode::Home, .. } => { ui.tree_cursor = 0; continue; }
                        KeyEvent { code: KeyCode::End, .. } => {
                            ui.tree_cursor = tree_visible_len(ui).saturating_sub(1);
                            continue;
                        }
                        KeyEvent { code: KeyCode::Char('g'), modifiers, .. }
                            if !modifiers.contains(KeyModifiers::SHIFT) => { ui.tree_cursor = 0; continue; }
                        KeyEvent { code: KeyCode::Char('G'), .. } => {
                            ui.tree_cursor = tree_visible_len(ui).saturating_sub(1);
                            continue;
                        }
                        KeyEvent { code: KeyCode::Esc, .. } => {
                            // Esc clears an active filter first, then exits the library.
                            if ui.tree_filter.is_empty() {
                                ui.view_mode = ViewMode::Player;
                            } else {
                                ui.tree_filter.clear();
                                refresh_tree_rows(ui);
                                ui.tree_cursor = 0;
                                ui.tree_scroll = 0;
                            }
                            return false;
                        }
                        _ => {} // fall through to global keys (L, Y, space, q, v, b, …)
                    }
                } else {
                match k {
                    KeyEvent { code: KeyCode::Up, .. } => {
                        playlist_cursor_up(ui);
                        continue; // Drain remaining events for smooth scrolling
                    }
                    KeyEvent { code: KeyCode::Down, .. } => {
                        playlist_cursor_down(ui, playlist);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Home, .. } => {
                        playlist_cursor_home(ui);
                        continue;
                    }
                    KeyEvent { code: KeyCode::End, .. } => {
                        playlist_cursor_end(ui, playlist);
                        continue;
                    }
                    KeyEvent { code: KeyCode::PageUp, .. } => {
                        playlist_cursor_page_up(ui);
                        continue;
                    }
                    KeyEvent { code: KeyCode::PageDown, .. } => {
                        playlist_cursor_page_down(ui, playlist);
                        continue;
                    }
                    // Vim-style fallbacks for Mac keyboards that lack Home/End/PgUp/PgDn.
                    KeyEvent { code: KeyCode::Char('g'), modifiers, .. } if !modifiers.contains(KeyModifiers::SHIFT) => {
                        playlist_cursor_home(ui);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('g'), modifiers, .. } if modifiers.contains(KeyModifiers::SHIFT) => {
                        playlist_cursor_end(ui, playlist);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('G'), .. } => {
                        playlist_cursor_end(ui, playlist);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('u'), modifiers, .. } if modifiers.contains(KeyModifiers::CONTROL) => {
                        playlist_cursor_page_up(ui);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('d'), modifiers, .. } if modifiers.contains(KeyModifiers::CONTROL) => {
                        playlist_cursor_page_down(ui, playlist);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('s'), modifiers, .. } if modifiers.contains(KeyModifiers::SHIFT) => {
                        sort_playlist_by_tags(state, ui, playlist);
                        return false;
                    }
                    KeyEvent { code: KeyCode::Char('S'), .. } => {
                        sort_playlist_by_tags(state, ui, playlist);
                        return false;
                    }
                    KeyEvent { code: KeyCode::Enter, .. } => {
                        let target = if ui.filtered_indices.is_empty() {
                            ui.cursor
                        } else {
                            ui.filtered_indices.get(ui.cursor).copied().unwrap_or(ui.cursor)
                        };
                        state.jump_to(target);
                        return false;
                    }
                    KeyEvent { code: KeyCode::Char('/'), .. } => {
                        ui.input_mode = InputMode::Search(String::new());
                        return false;
                    }
                    KeyEvent { code: KeyCode::Char('a'), .. } => {
                        enqueue_track(state, ui, playlist);
                        return false;
                    }
                    KeyEvent { code: KeyCode::Char('d'), .. } |
                    KeyEvent { code: KeyCode::Delete, .. } => {
                        remove_track(state, ui, playlist);
                        return false;
                    }
                    KeyEvent { code: KeyCode::Esc, .. } => {
                        ui.view_mode = ViewMode::Player;
                        return false;
                    }
                    _ => {} // Fall through to global keys
                }
                }
            }

            // Global keys (work in all view modes)
            match k {
                KeyEvent { code: KeyCode::Char(' '), .. } => state.toggle_pause(),
                KeyEvent { code: KeyCode::Up, .. } => state.next(),
                KeyEvent { code: KeyCode::Down, .. } => state.prev(),
                KeyEvent { code: KeyCode::Right, .. } => state.seek(10),
                KeyEvent { code: KeyCode::Left, .. } => state.seek(-10),
                KeyEvent { code: KeyCode::Char('v'), .. } => {
                    state.cycle_viz_mode();
                    // Re-anchor the UI at the top of the screen (resize-repaint
                    // path): a taller viz then grows into the reclaimed rows
                    // instead of scrolling the whole screen up.
                    ui.terminal_resized = true;
                }
                KeyEvent { code: KeyCode::Char('+'), .. } |
                KeyEvent { code: KeyCode::Char('='), .. } => state.volume_up(),
                KeyEvent { code: KeyCode::Char('-'), .. } => state.volume_down(),
                KeyEvent { code: KeyCode::Char('e'), .. } => {
                    // E opens (and closes) the EQ+FX editor screen.
                    ui.view_mode = match ui.view_mode {
                        ViewMode::Eq => ViewMode::Player,
                        _ => ViewMode::Eq,
                    };
                }
                KeyEvent { code: KeyCode::Char('x'), .. } => state.cycle_effects(),
                KeyEvent { code: KeyCode::Char('f'), .. } => state.toggle_pre_fader(),
                KeyEvent { code: KeyCode::Char('b'), .. } => {
                    state.toggle_viz_style();
                    // Style can change the viz height (VU: 4 vs 3 lines) — same
                    // re-anchor as 'v' so growth never scrolls the screen.
                    ui.terminal_resized = true;
                }
                KeyEvent { code: KeyCode::Char('l'), .. } => {
                    ui.view_mode = match ui.view_mode {
                        ViewMode::Player | ViewMode::Lyrics | ViewMode::Eq => {
                            ui.cursor = ui.current;
                            ensure_cursor_visible(ui, playlist);
                            ViewMode::Playlist
                        }
                        ViewMode::Playlist => ViewMode::Player,
                    };
                }
                KeyEvent { code: KeyCode::Char('y'), .. } => {
                    ui.view_mode = match ui.view_mode {
                        ViewMode::Player | ViewMode::Playlist | ViewMode::Eq => {
                            ui.lyrics_scroll = 0;
                            ui.lyrics_auto_scroll = true;
                            ViewMode::Lyrics
                        }
                        ViewMode::Lyrics => ViewMode::Player,
                    };
                }
                KeyEvent { code: KeyCode::Char('s'), .. } => {
                    ui.input_mode = InputMode::SavePlaylist(String::new());
                }
                KeyEvent { code: KeyCode::Char('r'), modifiers, .. } if modifiers.contains(KeyModifiers::SHIFT) => {
                    toggle_repeat(ui, state);
                }
                KeyEvent { code: KeyCode::Char('R'), .. } => {
                    toggle_repeat(ui, state);
                }
                KeyEvent { code: KeyCode::Char('r'), .. } => {
                    rescan(state, ui, playlist);
                }
                KeyEvent { code: KeyCode::Char('z'), .. } => {
                    toggle_shuffle(ui, playlist);
                }
                KeyEvent { code: KeyCode::Char('o'), .. } => {
                    let picked = prompt_path_line();
                    let _ = terminal::enable_raw_mode();
                    // prompt_path_line prints the prompt/echoed chars inline, which
                    // pushes the UI's cursor-tracking out of sync. Force a full redraw
                    // on the next frame via the same path as a terminal resize.
                    ui.terminal_resized = true;
                    match picked {
                        Some(p) => switch_source_paths(state, ui, playlist, p),
                        None => ui.set_status("Cancelled".to_string()),
                    }
                }
                KeyEvent { code: KeyCode::Char('p'), .. } => {
                    if has_native_picker() {
                        match pick_folder_native() {
                            Some(p) => switch_source_paths(state, ui, playlist, p),
                            None => ui.set_status("Cancelled".to_string()),
                        }
                    } else {
                        ui.set_status("Native picker unavailable; press O to type a path".to_string());
                    }
                }
                KeyEvent { code: KeyCode::Char('q'), .. } |
                KeyEvent { code: KeyCode::Esc, .. } => { state.quit(); return true; }
                KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. } => {
                    state.quit(); return true;
                }
                KeyEvent { code: KeyCode::Char('c'), .. } => state.cycle_crossfeed(),
                KeyEvent { code: KeyCode::Char('i'), .. } => state.toggle_stats(),
                KeyEvent { code: KeyCode::Char('['), .. } => state.balance_left(),
                KeyEvent { code: KeyCode::Char(']'), .. } => state.balance_right(),
                KeyEvent { code: KeyCode::Char('t'), .. } => {
                    let kind = state.cycle_theme();
                    ui.set_status(format!("theme: {}", kind.name()));
                    // Theme switch changes paint top-to-bottom; force a full redraw so
                    // residual lines from the previous theme don't bleed through.
                    // Banner also changes shape per theme, so trigger a banner rebuild.
                    ui.banner_dirty = true;
                    ui.terminal_resized = true;
                }
                _ => {}
            }
    }
    false
}

/// `/` search while the tree view is showing: keystrokes drive `ui.tree_filter`
/// live. Enter keeps the filter and returns to navigating the results; Esc
/// clears it. Up/Down/PageUp/PageDown move the cursor through the filtered rows.
fn tree_search_input(ui: &mut UiState, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => {
            ui.input_mode = InputMode::Normal;
            ui.tree_filter.clear();
            refresh_tree_rows(ui);
            ui.tree_cursor = 0;
            ui.tree_scroll = 0;
        }
        KeyCode::Enter => {
            ui.input_mode = InputMode::Normal; // keep the filter, navigate results
        }
        KeyCode::Backspace => {
            if let InputMode::Search(ref mut q) = ui.input_mode {
                q.pop();
            }
            rebuild_tree_filter(ui);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let InputMode::Search(ref mut q) = ui.input_mode {
                q.push(c);
            }
            rebuild_tree_filter(ui);
        }
        KeyCode::Up => tree_move(ui, -1),
        KeyCode::Down => tree_move(ui, 1),
        KeyCode::PageUp => tree_page(ui, -1),
        KeyCode::PageDown => tree_page(ui, 1),
        _ => {}
    }
    false
}

fn handle_text_input(state: &PlayerState, ui: &mut UiState, _playlist: &mut Vec<PathBuf>, key: KeyEvent) -> bool {
    // Ctrl+C quits from any text prompt, same as everywhere else. Without this
    // the Char(c) arms below would type a literal 'c' into the query — the
    // global Ctrl+C handler is never reached while a prompt is active.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.quit();
        return true;
    }
    // The tree view filters itself; its `/` search has its own handling.
    if ui.library_tree_mode && matches!(ui.input_mode, InputMode::Search(_)) {
        return tree_search_input(ui, key);
    }
    match &mut ui.input_mode {
        InputMode::Search(ref mut query) => {
            match key.code {
                KeyCode::Esc => {
                    ui.input_mode = InputMode::Normal;
                    ui.filtered_indices.clear();
                    ui.cursor = 0;
                    ui.scroll_offset = 0;
                }
                KeyCode::Enter => {
                    // A non-empty query with zero hits leaves filtered_indices
                    // empty — falling through to ui.cursor here would jump to
                    // an unrelated track. Just close the search instead.
                    let no_matches = !query.is_empty() && ui.filtered_indices.is_empty();
                    if !no_matches {
                        let target = if ui.filtered_indices.is_empty() {
                            ui.cursor
                        } else {
                            ui.filtered_indices.get(ui.cursor).copied().unwrap_or(0)
                        };
                        state.jump_to(target);
                    }
                    ui.input_mode = InputMode::Normal;
                    ui.filtered_indices.clear();
                    ui.cursor = 0;
                    ui.scroll_offset = 0;
                }
                KeyCode::Backspace => {
                    query.pop();
                    rebuild_filter(ui, _playlist);
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(c);
                    rebuild_filter(ui, _playlist);
                }
                KeyCode::Up => {
                    playlist_cursor_up(ui);
                }
                KeyCode::Down => {
                    playlist_cursor_down(ui, _playlist);
                }
                KeyCode::Home => {
                    playlist_cursor_home(ui);
                }
                KeyCode::End => {
                    playlist_cursor_end(ui, _playlist);
                }
                KeyCode::PageUp => {
                    playlist_cursor_page_up(ui);
                }
                KeyCode::PageDown => {
                    playlist_cursor_page_down(ui, _playlist);
                }
                _ => {}
            }
        }
        InputMode::SavePlaylist(ref mut name) => {
            match key.code {
                KeyCode::Esc => {
                    ui.input_mode = InputMode::Normal;
                }
                KeyCode::Enter => {
                    let save_name = name.clone();
                    ui.input_mode = InputMode::Normal;
                    if !save_name.is_empty() {
                        match crate::playlist::save_m3u(_playlist, &save_name) {
                            Ok(path) => {
                                let fname = path.file_name().unwrap_or_default().to_string_lossy();
                                ui.set_status(format!("Saved {} tracks to {}", _playlist.len(), fname));
                            }
                            Err(e) => {
                                ui.set_status(format!("Save failed: {}", e));
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    name.push(c);
                }
                _ => {}
            }
        }
        InputMode::Normal => {}
    }
    false
}

fn rebuild_filter(ui: &mut UiState, playlist: &[PathBuf]) {
    let query = match &ui.input_mode {
        InputMode::Search(q) => q.to_lowercase(),
        _ => return,
    };

    if query.is_empty() {
        ui.filtered_indices.clear();
        ui.cursor = 0;
        ui.scroll_offset = 0;
        return;
    }

    let cache = &ui.metadata_cache;
    ui.filtered_indices = playlist.iter()
        .enumerate()
        .filter(|(i, p)| {
            cache.search_matches(*i, p, &query)
        })
        .map(|(i, _)| i)
        .collect();

    ui.cursor = 0;
    ui.scroll_offset = 0;
}

fn playlist_cursor_up(ui: &mut UiState) {
    if ui.cursor > 0 {
        ui.cursor -= 1;
        if ui.cursor < ui.scroll_offset {
            ui.scroll_offset = ui.cursor;
        }
    }
}

fn playlist_cursor_down(ui: &mut UiState, playlist: &[PathBuf]) {
    let max = if ui.filtered_indices.is_empty() {
        playlist.len().saturating_sub(1)
    } else {
        ui.filtered_indices.len().saturating_sub(1)
    };
    if ui.cursor < max {
        ui.cursor += 1;
    }
}

fn playlist_cursor_home(ui: &mut UiState) {
    ui.cursor = 0;
    ui.scroll_offset = 0;
}

fn playlist_cursor_end(ui: &mut UiState, playlist: &[PathBuf]) {
    let max = if ui.filtered_indices.is_empty() {
        playlist.len().saturating_sub(1)
    } else {
        ui.filtered_indices.len().saturating_sub(1)
    };
    ui.cursor = max;
}

fn playlist_cursor_page_up(ui: &mut UiState) {
    // One-line overlap so the old top line becomes the new bottom — easier to track.
    let page = ui.last_visible_rows.saturating_sub(1).max(1);
    ui.cursor = ui.cursor.saturating_sub(page);
    if ui.cursor < ui.scroll_offset {
        ui.scroll_offset = ui.cursor;
    }
}

fn playlist_cursor_page_down(ui: &mut UiState, playlist: &[PathBuf]) {
    let max = if ui.filtered_indices.is_empty() {
        playlist.len().saturating_sub(1)
    } else {
        ui.filtered_indices.len().saturating_sub(1)
    };
    let page = ui.last_visible_rows.saturating_sub(1).max(1);
    ui.cursor = (ui.cursor + page).min(max);
}

fn ensure_cursor_visible(ui: &mut UiState, _playlist: &[PathBuf]) {
    if ui.cursor < ui.scroll_offset {
        ui.scroll_offset = ui.cursor;
    }
}

/// Reorder `current` to follow `saved` (the pre-shuffle snapshot): tracks still
/// present keep their saved order, tracks added since (rescan etc.) append at
/// the end in their current relative order.
fn restore_order(saved: &[PathBuf], current: &[PathBuf]) -> Vec<PathBuf> {
    use std::collections::HashSet;
    use std::path::Path;
    let current_set: HashSet<&Path> = current.iter().map(|p| p.as_path()).collect();
    let saved_set: HashSet<&Path> = saved.iter().map(|p| p.as_path()).collect();
    let mut out: Vec<PathBuf> = saved.iter()
        .filter(|p| current_set.contains(p.as_path()))
        .cloned()
        .collect();
    out.extend(current.iter().filter(|p| !saved_set.contains(p.as_path())).cloned());
    out
}

fn remove_track(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    if playlist.len() <= 1 {
        ui.set_status("Can't remove the last track".to_string());
        return;
    }

    // Resolve cursor to actual playlist index
    let track_idx = if ui.filtered_indices.is_empty() {
        ui.cursor
    } else {
        match ui.filtered_indices.get(ui.cursor) {
            Some(&idx) => idx,
            None => return,
        }
    };
    if track_idx >= playlist.len() { return; }

    let removed_name = ui.metadata_cache.display_name(track_idx, &playlist[track_idx]);

    // Track removed path so repeat cycle doesn't bring it back
    if let Ok(canon) = std::fs::canonicalize(&playlist[track_idx]) {
        ui.removed_paths.insert(canon);
    } else {
        ui.removed_paths.insert(playlist[track_idx].clone());
    }

    // Remove from the playlist, then remap the cache through the scan-safe
    // path below. Shifting the cache positionally (remove_at) while the
    // background scan is running would let in-flight workers — which write by
    // the index of the playlist snapshot they were spawned with — land tags
    // one slot off past the removal point.
    let old_playlist = playlist.clone();
    playlist.remove(track_idx);

    // Adjust current track index
    if track_idx == ui.current {
        // Removing current track: ui.current now points to the right next track
        ui.current = ui.current.min(playlist.len().saturating_sub(1));
        state.next(); // Signal producer to skip current track
        ui.current_track_removed = true; // dirty handler should jump to ui.current, not ui.current+1
    } else if track_idx < ui.current {
        ui.current -= 1;
    }

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(ui.current, Ordering::Relaxed);
    ui.playlist_dirty = true;
    reindex_and_restart_scan(ui, playlist, &old_playlist);

    // Rebuild filter if searching, otherwise just adjust cursor
    if !ui.filtered_indices.is_empty() {
        rebuild_filter(ui, playlist);
    }
    let max_cursor = if ui.filtered_indices.is_empty() {
        playlist.len().saturating_sub(1)
    } else {
        ui.filtered_indices.len().saturating_sub(1)
    };
    if ui.cursor > max_cursor {
        ui.cursor = max_cursor;
    }

    ui.set_status(format!("Removed: {}", removed_name));
}

/// Cancel the in-flight metadata scan, remap the cache to the reordered playlist,
/// then spawn a fresh scan. Reordering the playlist (sort/shuffle/rescan/source-switch)
/// must go through here: the scan workers write metadata by the index of the playlist
/// snapshot they were spawned with, so reindexing without first joining them lets
/// in-flight writes land in the wrong (remapped) cache slots.
pub(crate) fn reindex_and_restart_scan(
    ui: &mut UiState,
    playlist: &[PathBuf],
    old_playlist: &[PathBuf],
) {
    ui.metadata_cache.cancel.store(true, Ordering::Relaxed);
    if let Some(h) = ui.scan_handle.take() {
        h.join().ok();
    }
    ui.metadata_cache.reindex(playlist, old_playlist);
    ui.metadata_cache.cancel.store(false, Ordering::Relaxed);
    ui.scan_handle = Some(crate::metadata::spawn_metadata_scan(
        playlist.to_vec(),
        std::sync::Arc::clone(&ui.metadata_cache),
    ));
    ui.tree_dirty = true; // playlist reordered/rescanned — the tree needs rebuilding
}

/// True when the source paths are a folder/file collection we should auto-sort
/// into artist→album order: non-empty and containing no curated `.m3u`/`.m3u8`
/// playlist (an M3U's order is the user's curation and must be preserved).
fn source_is_sortable(paths: &[PathBuf]) -> bool {
    !paths.is_empty()
        && paths.iter().all(|p| {
            let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
            !matches!(ext.as_deref(), Some("m3u") | Some("m3u8"))
        })
}

/// One-shot auto-sort gate: the sort should run only when it's armed, the
/// background metadata scan has finished (so tags are actually loaded), and
/// we're not shuffling (shuffle order is intentional).
fn auto_sort_should_run(pending: bool, scan_finished: bool, shuffle: bool) -> bool {
    pending && scan_finished && !shuffle
}

/// Fire the one-shot artist→album auto-sort once the background metadata scan
/// has loaded tags. Called every UI frame: a no-op until armed and the scan is
/// finished. The flag is spent the moment the scan completes — even if we're
/// shuffling and skip the sort — so it never fires twice or retroactively after
/// a later shuffle toggle.
pub fn poll_auto_sort(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    if !ui.auto_sort_pending {
        return;
    }
    let scan_finished = ui.scan_handle.as_ref().is_some_and(|h| h.is_finished());
    if !scan_finished {
        return; // tags not loaded yet — keep waiting
    }
    let do_sort = auto_sort_should_run(ui.auto_sort_pending, scan_finished, ui.shuffle);
    ui.auto_sort_pending = false;
    if do_sort {
        sort_playlist_by_tags(state, ui, playlist);
    }
}

/// Arm the one-shot auto-sort after a folder-sourced playlist is (re)built
/// (startup / rescan / source-switch). No-op for curated M3U sources or while
/// shuffling; `poll_auto_sort` then fires it once the scan finishes.
pub fn arm_auto_sort(ui: &mut UiState) {
    ui.auto_sort_pending = source_is_sortable(&ui.source_paths) && !ui.shuffle;
}

// ===== Library tree view (artist → album → track browser) =====

/// Rebuild the artist→album tree from the current playlist + metadata cache.
/// Fold state (kept by name on `ui.tree_fold`) survives; the cursor is clamped.
pub fn rebuild_library_tree(ui: &mut UiState, playlist: &[PathBuf]) {
    let tags: Vec<crate::library::TrackTags> = playlist
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (artist, album) = ui.metadata_cache.artist_album(i);
            let title = ui.metadata_cache.title(i).unwrap_or_else(|| {
                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            });
            crate::library::TrackTags {
                artist,
                album,
                disc: ui.metadata_cache.disc_number(i),
                track: ui.metadata_cache.track_number(i),
                title,
            }
        })
        .collect();
    ui.library_tree = crate::library::build(&tags);
    refresh_tree_rows(ui);
    let n = ui.tree_rows.len();
    if ui.tree_cursor >= n {
        ui.tree_cursor = n.saturating_sub(1);
    }
    ui.tree_dirty = false;
}

/// Throttle for rebuilding the tree while the metadata scan is still loading
/// tags: rebuild on the first frame, then at most every 500 ms. Rebuilding the
/// whole tree (full tag projection + sort) at 20 fps for a large library
/// burned CPU for the entire scan with no visible benefit.
fn tree_scan_refresh_due(elapsed_since_last: Option<Duration>) -> bool {
    elapsed_since_last.is_none_or(|e| e >= Duration::from_millis(500))
}

/// Called each UI frame: while the tree view is showing, keep it fresh — rebuild
/// when the playlist changed (`tree_dirty`) or, throttled, while the scan is
/// still loading tags (so `Unknown` rows settle into their real artists as tags
/// arrive). One final rebuild fires when the scan completes, so the last tags
/// to load always land.
pub fn poll_library_tree(ui: &mut UiState, playlist: &[PathBuf]) {
    if !ui.library_tree_mode {
        return;
    }
    let scan_running = ui.scan_handle.as_ref().is_some_and(|h| !h.is_finished());
    if ui.tree_scan_was_running && !scan_running {
        ui.tree_dirty = true; // scan just finished — settle the final tags
    }
    ui.tree_scan_was_running = scan_running;
    let refresh_due = scan_running
        && tree_scan_refresh_due(ui.tree_scan_refreshed_at.map(|t| t.elapsed()));
    if ui.tree_dirty || refresh_due {
        rebuild_library_tree(ui, playlist);
        ui.tree_scan_refreshed_at = Some(std::time::Instant::now());
    }
}

/// Render the tree body for a themed library renderer: adjust scroll to keep the
/// cursor visible (small margin), record the viewport height for paging, and
/// return the palette-coloured lines.
pub fn render_tree_body(
    ui: &mut UiState,
    height: usize,
    width: usize,
    p: &crate::theme::Palette,
) -> Vec<String> {
    let n = ui.tree_rows.len();
    if ui.tree_cursor >= n {
        ui.tree_cursor = n.saturating_sub(1);
    }
    ui.tree_view_height = height;
    let margin = 4.min(height / 2);
    if ui.tree_cursor < ui.tree_scroll + margin {
        ui.tree_scroll = ui.tree_cursor.saturating_sub(margin);
    } else if ui.tree_cursor + margin + 1 > ui.tree_scroll + height {
        ui.tree_scroll = (ui.tree_cursor + margin + 1).saturating_sub(height);
    }
    let max_scroll = n.saturating_sub(height);
    if ui.tree_scroll > max_scroll {
        ui.tree_scroll = max_scroll;
    }
    crate::library::render_library_tree(
        &ui.library_tree,
        &ui.tree_fold,
        &ui.tree_rows,
        ui.tree_cursor,
        ui.tree_scroll,
        height,
        width,
        p,
        Some(ui.current),
    )
}

/// Re-materialize `ui.tree_rows`: the filtered projection when a `/` filter is
/// active, else the normal fold-based rows. Must be called after every tree,
/// fold, or filter mutation — navigation and rendering read the cache instead
/// of rebuilding the projection per keypress/frame (which cloned artist/album
/// names for every row, every time, on large libraries).
fn refresh_tree_rows(ui: &mut UiState) {
    ui.tree_rows = if ui.tree_filter.is_empty() {
        crate::library::visible_rows(&ui.library_tree, &ui.tree_fold)
    } else {
        crate::library::visible_rows_filtered(&ui.library_tree, &ui.tree_filter)
    };
}

fn tree_visible_len(ui: &UiState) -> usize {
    ui.tree_rows.len()
}

fn tree_row_at_cursor(ui: &UiState) -> Option<crate::library::VisibleRow> {
    ui.tree_rows.get(ui.tree_cursor).copied()
}

/// Re-read the `/` query into `ui.tree_filter` and clamp the cursor to the new
/// filtered row count. Called on each keystroke while searching in the tree.
fn rebuild_tree_filter(ui: &mut UiState) {
    ui.tree_filter = match &ui.input_mode {
        InputMode::Search(q) => q.clone(),
        _ => String::new(),
    };
    refresh_tree_rows(ui);
    let n = tree_visible_len(ui);
    if ui.tree_cursor >= n {
        ui.tree_cursor = n.saturating_sub(1);
    }
    ui.tree_scroll = 0;
}

fn tree_move(ui: &mut UiState, delta: isize) {
    let n = tree_visible_len(ui) as isize;
    if n == 0 {
        ui.tree_cursor = 0;
        return;
    }
    ui.tree_cursor = (ui.tree_cursor as isize + delta).clamp(0, n - 1) as usize;
}

fn tree_page(ui: &mut UiState, dir: isize) {
    let page = ui.tree_view_height.max(1) as isize;
    tree_move(ui, dir * page);
}

fn tree_expand_under_cursor(ui: &mut UiState) {
    if let Some(row) = tree_row_at_cursor(ui) {
        crate::library::expand(&ui.library_tree, &mut ui.tree_fold, row);
        refresh_tree_rows(ui);
    }
}

fn tree_collapse_under_cursor(ui: &mut UiState) {
    use crate::library::VisibleRow;
    if let Some(row) = tree_row_at_cursor(ui) {
        if let VisibleRow::Track { artist, album, .. } = row {
            // Collapse the parent album and land the cursor on it.
            let album_row = VisibleRow::Album { artist, album };
            crate::library::collapse(&ui.library_tree, &mut ui.tree_fold, album_row);
            refresh_tree_rows(ui);
            if let Some(pos) = ui.tree_rows.iter().position(|r| *r == album_row) {
                ui.tree_cursor = pos;
            }
        } else {
            crate::library::collapse(&ui.library_tree, &mut ui.tree_fold, row);
            refresh_tree_rows(ui);
        }
    }
    let n = tree_visible_len(ui);
    if ui.tree_cursor >= n {
        ui.tree_cursor = n.saturating_sub(1);
    }
}

/// The playlist index `Enter` plays: the first track under the cursor (a track →
/// itself, an album → its first track, an artist → their first track).
fn tree_cursor_play_index(ui: &UiState) -> Option<usize> {
    tree_row_at_cursor(ui).and_then(|row| crate::library::first_track_index(&ui.library_tree, row))
}

/// Remove the tracks under the cursor. A track removes just itself; an album or
/// artist stages a confirmation (`tree_pending_remove`) that `y` completes.
fn tree_remove_under_cursor(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    let Some(row) = tree_row_at_cursor(ui) else { return };
    let indices = crate::library::subtree_track_indices(&ui.library_tree, row);
    if indices.is_empty() {
        return;
    }
    match row {
        crate::library::VisibleRow::Track { .. } => {
            tree_remove_indices(state, ui, playlist, &indices);
        }
        crate::library::VisibleRow::Album { artist, album } => {
            let label = format!("album {}", ui.library_tree.artists[artist].albums[album].name);
            ui.set_status(format!("remove {label} — {} tracks?  [y/n]", indices.len()));
            ui.tree_pending_remove = Some((label, indices));
        }
        crate::library::VisibleRow::Artist { artist } => {
            let label = format!("artist {}", ui.library_tree.artists[artist].name);
            ui.set_status(format!("remove {label} — {} tracks?  [y/n]", indices.len()));
            ui.tree_pending_remove = Some((label, indices));
        }
    }
}

/// Actually remove a set of playlist indices: drop them (descending, so earlier
/// indices don't shift), record them in `removed_paths` so a rescan won't re-add
/// them, fix the playing/cursor position, reindex the cache, and rebuild the tree.
fn tree_remove_indices(
    state: &PlayerState,
    ui: &mut UiState,
    playlist: &mut Vec<PathBuf>,
    indices: &[usize],
) {
    let mut idx: Vec<usize> = indices.to_vec();
    idx.sort_unstable();
    idx.dedup();
    let old_playlist = playlist.clone();
    for &i in idx.iter().rev() {
        if i < playlist.len() {
            let key = std::fs::canonicalize(&playlist[i]).unwrap_or_else(|_| playlist[i].clone());
            ui.removed_paths.insert(key);
            playlist.remove(i);
        }
    }
    // Shift the playing index down by however many removed tracks preceded it.
    let removed_before_current = idx.iter().filter(|&&i| i < ui.current).count();
    ui.current = ui.current.saturating_sub(removed_before_current).min(playlist.len().saturating_sub(1));
    ui.tree_cursor = 0;
    ui.tree_scroll = 0;
    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    reindex_and_restart_scan(ui, playlist, &old_playlist);
    ui.set_status(format!("removed {} track(s)", idx.len()));
}

/// Sort the playlist by tag metadata: artist → album → disc → track → title → filename.
/// Tracks without any tags fall to the bottom (sorted among themselves by filename).
/// Preserves the currently-playing track's logical position.
fn sort_playlist_by_tags(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    if playlist.len() < 2 {
        ui.set_status("Nothing to sort".to_string());
        return;
    }

    let old_playlist = playlist.clone();
    let current_path = playlist.get(ui.current).cloned();

    // (bucket, artist, album, disc, track, title, filename). The leading u8
    // partitions tagged-vs-untagged so tracks without tags cluster at the bottom
    // rather than mingling alphabetically.
    type SortKey = (u8, String, String, u32, u32, String, String);
    let mut keyed: Vec<(SortKey, PathBuf)> =
        playlist.iter().enumerate().map(|(i, p)| {
            let (artist, album) = ui.metadata_cache.artist_album(i);
            let title = ui.metadata_cache.title(i);
            let track_no = ui.metadata_cache.track_number(i).unwrap_or(0);
            let disc_no = ui.metadata_cache.disc_number(i).unwrap_or(0);
            let filename = p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            let bucket = if artist.is_some() || album.is_some() || title.is_some() { 0 } else { 1 };
            let key = (
                bucket,
                artist.unwrap_or_default().to_lowercase(),
                album.unwrap_or_default().to_lowercase(),
                disc_no,
                track_no,
                title.unwrap_or_default().to_lowercase(),
                filename,
            );
            (key, p.clone())
        }).collect();

    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    *playlist = keyed.into_iter().map(|(_, p)| p).collect();

    // Re-locate the playing track in the new ordering.
    if let Some(ref cp) = current_path {
        if let Some(idx) = playlist.iter().position(|p| p == cp) {
            ui.current = idx;
        }
    }

    // Sorting invalidates the enqueue queue (positions no longer reflect user intent).
    ui.enqueue_count = 0;

    reindex_and_restart_scan(ui, playlist, &old_playlist);
    state.current_track.store(ui.current, Ordering::Relaxed);
    ui.cursor = ui.current;
    ensure_cursor_visible(ui, playlist);
    ui.playlist_dirty = true;
    ui.banner_dirty = true;
    let was_shuffled = ui.shuffle;
    ui.shuffle = false;
    // The user picked an explicit new order; the pre-shuffle snapshot is stale.
    ui.pre_shuffle_order = None;
    ui.set_status(
        if was_shuffled { "Sorted by tags (shuffle off)" } else { "Sorted by tags" }.to_string()
    );
}

/// Toggle runtime shuffle. When turning ON, snapshots the current order and
/// shuffles the tracks after the current one (so the now-playing song isn't
/// interrupted). When turning OFF, restores the snapshotted order — sorting
/// would destroy an M3U's curated order — falling back to a path sort when no
/// snapshot exists (e.g. the session started with --shuffle).
fn toggle_shuffle(ui: &mut UiState, playlist: &mut [PathBuf]) {
    let old_playlist = playlist.to_vec();
    ui.shuffle = !ui.shuffle;
    let current_path = playlist.get(ui.current).cloned();

    if ui.shuffle {
        ui.pre_shuffle_order = Some(old_playlist.clone());
        // Shuffle everything after the currently-playing track
        let start = ui.current + 1;
        if start < playlist.len() {
            let tail = &mut playlist[start..];
            crate::playlist::shuffle_list(tail);
        }
        ui.set_status("Shuffle ON".to_string());
    } else {
        let restored = match ui.pre_shuffle_order.take() {
            Some(saved) => restore_order(&saved, playlist),
            None => {
                let mut sorted = playlist.to_vec();
                sorted.sort();
                sorted
            }
        };
        if restored.len() == playlist.len() {
            playlist.clone_from_slice(&restored);
        } else {
            // Duplicate/uncanonicalizable paths can make restore_order return a
            // different length; clone_from_slice would panic on the mismatch.
            // Fall back to the same path sort used when no snapshot exists.
            playlist.sort();
        }
        if let Some(ref cp) = current_path {
            if let Some(idx) = playlist.iter().position(|p| p == cp) {
                ui.current = idx;
            }
        }
        ui.set_status("Shuffle OFF".to_string());
    }
    // Cached metadata is indexed by position — remap it to match the reordered paths.
    reindex_and_restart_scan(ui, playlist, &old_playlist);
    ui.playlist_dirty = true;
    ui.banner_dirty = true;
}

fn toggle_repeat(ui: &mut UiState, state: &PlayerState) {
    ui.repeat_mode = ui.repeat_mode.next();
    state.repeat_mode.store(ui.repeat_mode as u8, Ordering::Relaxed);
    let msg = match ui.repeat_mode {
        crate::state::RepeatMode::Off => "Repeat OFF",
        crate::state::RepeatMode::All => "Repeat ALL",
        crate::state::RepeatMode::One => "Repeat ONE",
    };
    ui.set_status(msg.to_string());
    ui.banner_dirty = true;
}

fn enqueue_track(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    let track_idx = if ui.filtered_indices.is_empty() {
        ui.cursor
    } else {
        match ui.filtered_indices.get(ui.cursor) {
            Some(&idx) => idx,
            None => return,
        }
    };
    if track_idx >= playlist.len() || track_idx == ui.current { return; }

    // Target position: right after current + any previously enqueued tracks
    let target = ui.current + 1 + ui.enqueue_count;
    let target = target.min(playlist.len().saturating_sub(1));

    if track_idx == target { return; }

    let name = ui.metadata_cache.display_name(track_idx, &playlist[track_idx]);

    // Move the track in the playlist, then remap the cache through the
    // scan-safe path below (same hazard as remove_track: a positional
    // move_entry races in-flight scan workers writing by stale indices).
    let old_playlist = playlist.clone();
    let path = playlist.remove(track_idx);
    let dst = if track_idx < target { target - 1 } else { target };
    playlist.insert(dst, path);

    // Recalculate ui.current — it may have shifted
    // If we removed before current, current shifted down; if we inserted at/before current, it shifted up
    if track_idx < ui.current && dst >= ui.current {
        ui.current -= 1;
    } else if track_idx > ui.current && dst <= ui.current {
        ui.current += 1;
    }

    // Keep cursor on the same logical track
    if track_idx == ui.cursor {
        ui.cursor = dst;
    } else if track_idx < ui.cursor && dst >= ui.cursor {
        ui.cursor -= 1;
    } else if track_idx > ui.cursor && dst <= ui.cursor {
        ui.cursor += 1;
    }

    ui.enqueue_count += 1;
    ui.playlist_dirty = true;
    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    reindex_and_restart_scan(ui, playlist, &old_playlist);
    ui.set_status(format!("Queued: {}", name));
}

/// Replace the current music source with a new path, rebuild the playlist,
/// reindex the metadata cache, and jump playback to the new first track.
fn switch_source_paths(
    state: &PlayerState,
    ui: &mut UiState,
    playlist: &mut Vec<PathBuf>,
    new_path: PathBuf,
) {
    use std::sync::atomic::Ordering;

    if !new_path.exists() {
        ui.set_status(format!("Path not found: {}", new_path.display()));
        return;
    }

    // Honor the current session's shuffle setting. Repeat is preserved implicitly —
    // main.rs's repeat-cycle loop keeps running regardless of source.
    let new_list = match crate::playlist::build_playlist(&new_path, ui.shuffle) {
        Ok(list) => list,
        Err(e) => {
            ui.set_status(format!("Failed to read source: {}", e));
            return;
        }
    };

    let old_playlist = std::mem::replace(playlist, new_list);
    ui.source_paths = vec![new_path.clone()];
    ui.pre_shuffle_order = None; // snapshot belongs to the previous source
    ui.current = 0;
    ui.cursor = 0;
    ui.scroll_offset = 0;

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(0, Ordering::Relaxed);

    reindex_and_restart_scan(ui, playlist, &old_playlist);
    arm_auto_sort(ui); // new folder source → auto-sort once its tags load

    // Signal the producer to break out of the current track and jump to index 0
    // of the new playlist on its next iteration.
    state.jump_to(0);

    let name = new_path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| new_path.display().to_string());
    ui.set_status(format!("Source: {} ({} tracks)", name, playlist.len()));
}

fn rescan(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    use std::sync::atomic::Ordering;

    let old_playlist = playlist.clone();
    let current_track_path = playlist.get(ui.current).cloned();
    let mut total_added = 0usize;
    let mut total_removed = 0usize;
    let mut had_error = false;

    for source in ui.source_paths.clone() {
        match crate::playlist::rescan_playlist(
            &source,
            playlist,
            current_track_path.as_deref(),
        ) {
            Ok((added, removed)) => {
                total_added += added;
                total_removed += removed;
            }
            Err(_) => { had_error = true; }
        }
    }

    // Deduplicate after rescan
    let mut seen = std::collections::HashSet::new();
    playlist.retain(|p| {
        let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        seen.insert(key)
    });

    // Find current track's new index
    if let Some(ref track_path) = current_track_path {
        if let Some(new_idx) = playlist.iter().position(|p| p == track_path) {
            ui.current = new_idx;
        } else {
            ui.current = ui.current.min(playlist.len().saturating_sub(1));
        }
    }

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(ui.current, Ordering::Relaxed);

    reindex_and_restart_scan(ui, playlist, &old_playlist);
    arm_auto_sort(ui); // re-settle newly-added tracks into artist→album order

    if playlist.is_empty() || (playlist.len() == 1 && total_removed > 0 && current_track_path.is_some()) {
        ui.set_status("All files removed, finishing current track".to_string());
    } else if total_added == 0 && total_removed == 0 && !had_error {
        ui.set_status("No changes found".to_string());
    } else if had_error && total_added == 0 && total_removed == 0 {
        ui.set_status("Rescan failed for some sources".to_string());
    } else {
        ui.set_status(format!("+{} added, -{} removed", total_added, total_removed));
    }
}

/// Opens a native folder-picker dialog on macOS via AppleScript.
#[cfg(target_os = "macos")]
fn pick_folder_native() -> Option<PathBuf> {
    let output = std::process::Command::new("osascript")
        .args([
            "-e",
            "try\nPOSIX path of (choose folder with prompt \"Select a music folder\")\non error\nreturn \"\"\nend try",
        ])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(PathBuf::from(s)) }
}

/// Opens a native folder-picker dialog on Windows via PowerShell's Shell.Application COM object.
#[cfg(target_os = "windows")]
fn pick_folder_native() -> Option<PathBuf> {
    let script = "$s = New-Object -ComObject Shell.Application; \
        $f = $s.BrowseForFolder(0, 'Select a music folder', 0, 0); \
        if ($f) { $f.Self.Path }";
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(PathBuf::from(s)) }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn pick_folder_native() -> Option<PathBuf> { None }

fn has_native_picker() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// Prompt for a path in raw mode so Esc can cancel. Enter submits, Backspace edits,
/// Ctrl-C cancels. On entry/exit this leaves the terminal in cooked mode — the caller
/// is responsible for re-enabling raw mode if it needs it.
fn prompt_path_line() -> Option<PathBuf> {
    let _ = terminal::enable_raw_mode();
    print!("\n\r  {}Enter path (Esc to cancel):{} ", C_BOLD, C_RESET);
    io::stdout().flush().ok();

    let mut buf = String::new();
    let result = loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Esc => break None,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break None,
                KeyCode::Enter => {
                    let trimmed = buf.trim().to_string();
                    break if trimmed.is_empty() { None } else { Some(PathBuf::from(trimmed)) };
                }
                KeyCode::Backspace
                    if buf.pop().is_some() => {
                        print!("\x08 \x08");
                        io::stdout().flush().ok();
                    }
                KeyCode::Char(c) => {
                    buf.push(c);
                    print!("{}", c);
                    io::stdout().flush().ok();
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break None,
        }
    };

    let _ = terminal::disable_raw_mode();
    print!("\r\n");
    io::stdout().flush().ok();
    result
}

/// Interactive first-launch picker shown when the user runs keet with no args
/// and no saved session. Returns the selected path or None if the user quit.
pub fn run_first_launch_picker() -> Option<PathBuf> {
    let native = has_native_picker();
    loop {
        println!();
        println!("  {}Keet{} — no music source given and no saved session.", C_BOLD, C_RESET);
        println!();
        if native {
            println!("  {}P{}  Pick a folder", C_CYAN, C_RESET);
        }
        println!("  {}T{}  Type a path", C_CYAN, C_RESET);
        println!("  {}Q{}  Quit", C_CYAN, C_RESET);
        println!();
        print!("  {}Choose:{} ", C_DIM, C_RESET);
        io::stdout().flush().ok();

        if terminal::enable_raw_mode().is_err() {
            return None;
        }
        let key = loop {
            if let Ok(true) = event::poll(Duration::from_millis(500)) {
                if let Ok(Event::Key(k)) = event::read() {
                    if k.kind == KeyEventKind::Release { continue; }
                    break k;
                }
            }
        };
        let _ = terminal::disable_raw_mode();
        println!();

        let chosen = match key.code {
            KeyCode::Char('p') | KeyCode::Char('P') if native => pick_folder_native(),
            KeyCode::Char('t') | KeyCode::Char('T') => prompt_path_line(),
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return None,
            _ => continue,
        };

        match chosen {
            Some(p) if p.exists() => return Some(p),
            Some(p) => {
                println!("  {}Path not found:{} {}", C_RED, C_RESET, p.display());
            }
            None => {
                println!("  {}Cancelled{}", C_DIM, C_RESET);
            }
        }
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;

    #[test]
    fn frame_writer_derives_count_from_emission() {
        let mut w = FrameWriter::new();
        w.first_line("anchor");
        assert_eq!(w.count(), 0, "the first line is the anchor row, not counted");
        w.line("progress");
        w.line_raw("sixel transmit");
        w.line("");
        assert_eq!(w.count(), 3, "count must equal the lines actually emitted");
    }

    #[test]
    fn format_time_under_an_hour_keeps_mm_ss() {
        assert_eq!(format_time(59.0), "00:59");
        assert_eq!(format_time(125.4), "02:05");
    }

    #[test]
    fn format_time_above_an_hour_shows_h_mm_ss() {
        assert_eq!(format_time(3600.0), "1:00:00");
        assert_eq!(format_time(3725.0), "1:02:05");
        assert_eq!(format_time(2.0 * 3600.0 + 59.0 * 60.0 + 59.0), "2:59:59");
    }

    #[test]
    fn restore_order_keeps_saved_order_and_appends_new() {
        let p = |s: &str| PathBuf::from(s);
        let saved = vec![p("a"), p("b"), p("c")];
        // b removed, d added, rest shuffled
        let current = vec![p("c"), p("d"), p("a")];
        assert_eq!(restore_order(&saved, &current), vec![p("a"), p("c"), p("d")]);
    }

    #[test]
    fn source_is_sortable_false_when_any_m3u_present_or_empty() {
        let p = |s: &str| PathBuf::from(s);
        // Folder / file sources are sortable.
        assert!(source_is_sortable(&[p("/music/rock"), p("/music/song.flac")]));
        // Any .m3u / .m3u8 source is a curated order — not sortable (case-insensitive).
        assert!(!source_is_sortable(&[p("/music/mix.m3u")]));
        assert!(!source_is_sortable(&[p("/music/rock"), p("/lists/set.M3U8")]));
        // No sources → nothing to auto-sort.
        assert!(!source_is_sortable(&[]));
    }

    #[test]
    fn auto_sort_should_run_only_when_pending_scanned_and_not_shuffling() {
        assert!(auto_sort_should_run(true, true, false)); // armed, scan done, not shuffling
        assert!(!auto_sort_should_run(false, true, false)); // not armed
        assert!(!auto_sort_should_run(true, false, false)); // scan not finished — tags not loaded
        assert!(!auto_sort_should_run(true, true, true)); // shuffling — leave the order alone
    }

    fn test_ui(playlist_len: usize) -> UiState {
        let cache = crate::metadata::MetadataCache::new(playlist_len);
        UiState::new(vec![PathBuf::from("/nonexistent-src")], cache)
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn ctrl_c_quits_from_text_input_instead_of_typing_c() {
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        ui.input_mode = InputMode::Search(String::new());
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3")];

        let quit = handle_text_input(
            &state, &mut ui, &mut playlist,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(quit && state.should_quit(), "Ctrl+C in text input must quit");
        if let InputMode::Search(q) = &ui.input_mode {
            assert!(q.is_empty(), "Ctrl+C must not type into the query: {q:?}");
        }

        // A plain 'c' (no modifier) still types.
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        ui.input_mode = InputMode::Search(String::new());
        handle_text_input(
            &state, &mut ui, &mut playlist,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );
        assert!(matches!(&ui.input_mode, InputMode::Search(q) if q == "c"));
        assert!(!state.should_quit());
    }

    #[test]
    fn tree_scan_refresh_due_first_time_then_every_half_second() {
        use std::time::Duration;
        // Never refreshed → due immediately.
        assert!(tree_scan_refresh_due(None));
        // Refreshed recently → wait (rebuilding a large tree at 20 fps burned
        // CPU for the whole scan duration with no visible benefit).
        assert!(!tree_scan_refresh_due(Some(Duration::from_millis(100))));
        // Half a second on → due again.
        assert!(tree_scan_refresh_due(Some(Duration::from_millis(600))));
    }

    #[test]
    fn tree_rows_cache_refreshes_on_expand_and_filter() {
        let tag = |artist: &str, title: &str| crate::library::TrackTags {
            artist: Some(artist.into()),
            album: Some("Album".into()),
            disc: None,
            track: Some(1),
            title: title.into(),
        };
        let mut ui = test_ui(2);
        ui.library_tree = crate::library::build(&[tag("A", "t1"), tag("B", "t2")]);
        refresh_tree_rows(&mut ui);
        assert_eq!(ui.tree_rows.len(), 2, "two collapsed artist rows");

        // Expanding must refresh the cache — navigation reads it, not a rebuild.
        ui.tree_cursor = 0;
        tree_expand_under_cursor(&mut ui);
        assert_eq!(ui.tree_rows.len(), 3, "expand must refresh the cached rows");

        // Filter change must refresh too.
        ui.input_mode = InputMode::Search("t2".into());
        rebuild_tree_filter(&mut ui);
        assert_eq!(
            ui.tree_rows.len(),
            3, // artist B + album + track t2
            "filter must refresh the cached rows: {:?}",
            ui.tree_rows
        );
    }

    #[test]
    fn shuffle_off_with_mismatched_snapshot_falls_back_instead_of_panicking() {
        // A duplicate path surviving dedup (e.g. canonicalize failed on one of
        // two spellings) makes restore_order return FEWER entries than the live
        // playlist — clone_from_slice would panic on the length mismatch.
        let mut ui = test_ui(3);
        ui.shuffle = true;
        ui.pre_shuffle_order = Some(vec![p("/a.mp3")]);
        let mut playlist = vec![p("/b.mp3"), p("/a.mp3"), p("/a.mp3")];

        toggle_shuffle(&mut ui, &mut playlist);

        assert!(!ui.shuffle);
        assert_eq!(playlist.len(), 3, "fallback must keep every track");
        let mut sorted = playlist.clone();
        sorted.sort();
        assert_eq!(playlist, sorted, "mismatch falls back to the path sort");
    }

    #[test]
    fn remove_track_restarts_scan_and_marks_tree_dirty() {
        // Flat-list remove must go through reindex_and_restart_scan: mutating
        // the cache positionally (remove_at) while the background scan is
        // running lets in-flight workers write tags into the wrong slots.
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3")];
        ui.tree_dirty = false;
        ui.cursor = 1; // not the playing track (current = 0)

        remove_track(&state, &mut ui, &mut playlist);

        assert_eq!(playlist, vec![p("/a.mp3"), p("/c.mp3")]);
        assert!(ui.tree_dirty, "remove must reindex via the scan-safe path");
        assert!(ui.scan_handle.is_some(), "scan must be restarted after reindex");
    }

    #[test]
    fn enqueue_track_restarts_scan_and_marks_tree_dirty() {
        // Same hazard as remove: move_entry during an active scan misplaces
        // in-flight tag writes.
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3")];
        ui.tree_dirty = false;
        ui.cursor = 2; // enqueue /c.mp3 to play right after current (index 0)

        enqueue_track(&state, &mut ui, &mut playlist);

        assert_eq!(playlist, vec![p("/a.mp3"), p("/c.mp3"), p("/b.mp3")]);
        assert!(ui.tree_dirty, "enqueue must reindex via the scan-safe path");
        assert!(ui.scan_handle.is_some(), "scan must be restarted after reindex");
    }

    #[test]
    fn ctrl_c_quits_from_save_playlist_and_tree_filter_input() {
        // SavePlaylist prompt.
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        ui.input_mode = InputMode::SavePlaylist(String::new());
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3")];
        let quit = handle_text_input(
            &state, &mut ui, &mut playlist,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(quit && state.should_quit());

        // Tree-filter search (`/` in the tree view) routes through tree_search_input.
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        ui.library_tree_mode = true;
        ui.input_mode = InputMode::Search(String::new());
        let quit = handle_text_input(
            &state, &mut ui, &mut playlist,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(quit && state.should_quit());
        assert!(ui.tree_filter.is_empty(), "Ctrl+C must not type into the tree filter");
    }
}
