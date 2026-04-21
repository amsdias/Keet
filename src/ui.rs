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
    render_spectrogram, get_viz_line_count,
};

pub fn format_time(secs: f64) -> String {
    format!("{:02}:{:02}", (secs / 60.0) as u32, (secs % 60.0) as u32)
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

/// Truncate a string containing ANSI escape codes to fit within `max_width` visible characters.
/// Returns the truncated string with all ANSI codes preserved up to the cut point.
fn truncate_ansi(s: &str, max_width: usize) -> String {
    let mut visible = 0;
    let mut result = String::with_capacity(s.len());
    let mut in_escape = false;

    for ch in s.chars() {
        if in_escape {
            result.push(ch);
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if ch == '\x1B' {
            in_escape = true;
            result.push(ch);
        } else {
            if visible >= max_width {
                break;
            }
            result.push(ch);
            visible += 1;
        }
    }
    result
}

fn truncate_plain(s: &str, max_width: usize) -> String {
    if s.chars().count() <= max_width {
        s.to_string()
    } else if max_width > 1 {
        let mut out: String = s.chars().take(max_width - 1).collect();
        out.push('…');
        out
    } else {
        s.chars().take(max_width).collect()
    }
}

fn visible_len(s: &str) -> usize {
    s.chars().count()
}

pub fn print_status(state: &PlayerState, ui: &mut UiState, name: &str, track_info: &str, ext: &str, eq_preset: &crate::eq::EqPreset, fx_name: &str, cf_name: &str, stats: &mut StatsMonitor, prev_viz_lines: usize, playlist: &[PathBuf], analyser: &VizAnalyser) -> usize {
    let viz_mode = state.viz_mode();
    let viz_style = state.viz_style();
    let eq_name = &eq_preset.name;
    let eq_curve = crate::eq::render_eq_curve(eq_preset);
    let eq_line = !eq_curve.is_empty();
    let viz_lines = get_viz_line_count(viz_mode, viz_style) + if eq_line { 1 } else { 0 };
    let term_w = terminal::size().map(|(w, _)| w as usize).unwrap_or(120);

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

    // Move cursor back to start of our output area (single atomic escape)
    if prev_viz_lines != usize::MAX {
        let up = 1 + prev_viz_lines;
        print!("\x1B[{}F", up); // CPL: move up N lines, go to column 1
    }

    // Line 1: Track info (truncated to terminal width)
    let ic = icon_color_for_ext(ext);
    let line1 = format!("{C_DIM}[{track}/{total}]{C_RESET} {ic}♪{C_RESET} {C_BOLD}{C_CYAN}{display_name}{C_RESET} {C_DIM}{track_info}{C_RESET}");
    print!("\r\x1B[K{}\n", truncate_ansi(&line1, term_w));

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
    };
    let next_style = match viz_style {
        VizStyle::Dots => "Bars",
        VizStyle::Bars => "Dots",
    };
    let stats_display = if state.show_stats() {
        format!(" cpu:{:.1}% mem:{:.0}M", stats.cpu_usage, stats.memory_mb)
    } else {
        String::new()
    };
    let line2 = format!("  {icon_color}{icon}{C_RESET} {C_BOLD}[{cur}/{tot}]{C_RESET} {C_GREEN}{bar_filled}{C_RESET} {C_DIM}vol:{vol}%{eq_display}{fx_display}{cf_display}{clip_display}{bal_display} {fader} buf:{buf_pct}%{stats_display} {{V}}:{next_viz} {{B}}:{next_style}{C_RESET}");
    print!("\r\x1B[K{}", truncate_ansi(&line2, term_w));

    // EQ curve visualization (when non-Flat preset is active)
    if eq_line {
        print!("\n\r\x1B[K{}", eq_curve);
    }

    // Separation line and content area
    if ui.view_mode == ViewMode::Playlist {
        let term_h = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);
        let header_lines = 2 + if eq_line { 1 } else { 0 };
        let footer_lines = 2; // separator + footer
        let visible_rows = term_h.saturating_sub(header_lines + footer_lines + ui.banner_lines).max(1);
        ui.last_visible_rows = visible_rows;

        // Separator
        print!("\n\r\x1B[K  {C_DIM}{}{C_RESET}", "─".repeat(term_w.saturating_sub(2)));

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
            print!("\n\r\x1B[K  {C_DIM}(no matches){C_RESET}");
            for _ in 1..visible_rows {
                print!("\n\r\x1B[K");
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

                print!("\n\r\x1B[K{}", line);
            }

            // Pad remaining rows
            for _ in visible_count..visible_rows {
                print!("\n\r\x1B[K");
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
                if let Some(msg) = ui.take_status() {
                    format!("  {C_GREEN}{msg}{C_RESET}")
                } else {
                    format!("  {C_DIM}[L] close  [↑↓] scroll  [Enter] play  [A] enqueue  [/] search  [D] remove  [S] save{C_RESET}")
                }
            }
        };
        print!("\n\r\x1B[K{}", truncate_ansi(&footer, term_w));

        let total_lines = 1 + visible_rows + 1; // separator + rows + footer
        print!("\x1B[J");
        io::stdout().flush().ok();
        return total_lines;
    }

    // Lyrics view
    if ui.view_mode == ViewMode::Lyrics {
        let term_h = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);
        let header_lines = 2 + if eq_line { 1 } else { 0 };
        let footer_lines = 2;
        let visible_rows = term_h.saturating_sub(header_lines + footer_lines + ui.banner_lines).max(1);

        // Separator
        print!("\n\r\x1B[K  {C_DIM}{}{C_RESET}", "─".repeat(term_w.saturating_sub(2)));

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
                    print!("\n\r\x1B[K{}", truncate_ansi(&line, term_w));
                } else {
                    print!("\n\r\x1B[K");
                }
            }
        } else {
            print!("\n\r\x1B[K  {C_DIM}(no lyrics available){C_RESET}");
            for _ in 1..visible_rows {
                print!("\n\r\x1B[K");
            }
        }

        // Footer
        let is_synced = ui.lyrics.as_ref().map(|l| l.is_synced()).unwrap_or(false);
        let offset_display = if is_synced && ui.lyrics_offset != 0.0 {
            format!("  offset:{:+.1}s", ui.lyrics_offset)
        } else { String::new() };
        let sync_hint = if is_synced { "  [A/D] sync" } else { "" };
        let footer = format!("  {C_DIM}[Y] close  [W/S] scroll{sync_hint}{offset_display}{C_RESET}");
        print!("\n\r\x1B[K{}", truncate_ansi(&footer, term_w));

        let total_lines = 1 + visible_rows + 1;
        print!("\x1B[J");
        io::stdout().flush().ok();
        return total_lines;
    }

    // Original Player mode rendering below
    if viz_mode != VizMode::None {
        print!("\n\r\x1B[K  {C_DIM}{}{C_RESET}", "─".repeat(term_w.saturating_sub(2)));
    }

    match viz_mode {
        VizMode::None => {}
        VizMode::VuMeter => {
            for line in render_vu_meter(state, viz_style) {
                print!("\n\r\x1B[K{}", line);
            }
        }
        VizMode::SpectrumHorizontal => {
            for line in render_spectrum_horizontal(state, viz_style) {
                print!("\n\r\x1B[K{}", line);
            }
        }
        VizMode::SpectrumVertical => {
            for line in render_spectrum_vertical(state, viz_style) {
                print!("\n\r\x1B[K{}", line);
            }
        }
        VizMode::Oscilloscope => {
            for line in render_oscilloscope(analyser, viz_style) {
                print!("\n\r\x1B[K{}", line);
            }
        }
        VizMode::Lissajous => {
            for line in render_lissajous(analyser, viz_style) {
                print!("\n\r\x1B[K{}", line);
            }
        }
        VizMode::Spectrogram => {
            for line in render_spectrogram(analyser, viz_style) {
                print!("\n\r\x1B[K{}", line);
            }
        }
    }

    // Show status message in Player mode
    if let Some(msg) = ui.take_status() {
        print!("\n\r\x1B[K  {C_GREEN}{msg}{C_RESET}");
        print!("\x1B[J");
        io::stdout().flush().ok();
        return viz_lines + 1;
    }

    print!("\x1B[J");
    io::stdout().flush().ok();
    viz_lines
}

pub fn poll_input(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) -> bool {
    // Drain all pending events for responsive input
    while event::poll(Duration::ZERO).unwrap_or(false) {
        let ev = match event::read() { Ok(e) => e, Err(_) => continue };

        if let Event::Resize(_, _) = ev {
            ui.terminal_resized = true;
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

            // Playlist view keys (when in Normal input mode)
            if ui.view_mode == ViewMode::Playlist {
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

            // Global keys (work in all view modes)
            match k {
                KeyEvent { code: KeyCode::Char(' '), .. } => state.toggle_pause(),
                KeyEvent { code: KeyCode::Up, .. } => state.next(),
                KeyEvent { code: KeyCode::Down, .. } => state.prev(),
                KeyEvent { code: KeyCode::Right, .. } => state.seek(10),
                KeyEvent { code: KeyCode::Left, .. } => state.seek(-10),
                KeyEvent { code: KeyCode::Char('v'), .. } => state.cycle_viz_mode(),
                KeyEvent { code: KeyCode::Char('+'), .. } |
                KeyEvent { code: KeyCode::Char('='), .. } => state.volume_up(),
                KeyEvent { code: KeyCode::Char('-'), .. } => state.volume_down(),
                KeyEvent { code: KeyCode::Char('e'), .. } => state.cycle_eq(),
                KeyEvent { code: KeyCode::Char('x'), .. } => state.cycle_effects(),
                KeyEvent { code: KeyCode::Char('f'), .. } => state.toggle_pre_fader(),
                KeyEvent { code: KeyCode::Char('b'), .. } => state.toggle_viz_style(),
                KeyEvent { code: KeyCode::Char('l'), .. } => {
                    ui.view_mode = match ui.view_mode {
                        ViewMode::Player | ViewMode::Lyrics => {
                            ui.cursor = ui.current;
                            ensure_cursor_visible(ui, playlist);
                            ViewMode::Playlist
                        }
                        ViewMode::Playlist => ViewMode::Player,
                    };
                }
                KeyEvent { code: KeyCode::Char('y'), .. } => {
                    ui.view_mode = match ui.view_mode {
                        ViewMode::Player | ViewMode::Playlist => {
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
                _ => {}
            }
    }
    false
}

fn handle_text_input(state: &PlayerState, ui: &mut UiState, _playlist: &mut Vec<PathBuf>, key: KeyEvent) -> bool {
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
                    let target = if ui.filtered_indices.is_empty() {
                        ui.cursor
                    } else {
                        ui.filtered_indices.get(ui.cursor).copied().unwrap_or(0)
                    };
                    state.jump_to(target);
                    ui.input_mode = InputMode::Normal;
                    ui.filtered_indices.clear();
                    ui.cursor = 0;
                    ui.scroll_offset = 0;
                }
                KeyCode::Backspace => {
                    query.pop();
                    rebuild_filter(ui, _playlist);
                }
                KeyCode::Char(c) => {
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
                KeyCode::Char(c) => {
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

    // Remove from playlist and metadata cache
    playlist.remove(track_idx);
    ui.metadata_cache.remove_at(track_idx);

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

    ui.metadata_cache.reindex(playlist, &old_playlist);
    state.current_track.store(ui.current, Ordering::Relaxed);
    ui.cursor = ui.current;
    ensure_cursor_visible(ui, playlist);
    ui.playlist_dirty = true;
    ui.banner_dirty = true;
    let was_shuffled = ui.shuffle;
    ui.shuffle = false;
    ui.set_status(
        if was_shuffled { "Sorted by tags (shuffle off)" } else { "Sorted by tags" }.to_string()
    );
}

/// Toggle runtime shuffle. When turning ON, shuffles the tracks after the current
/// one (so the now-playing song isn't interrupted). When turning OFF, re-sorts
/// the playlist by PathBuf order and relocates the current track.
fn toggle_shuffle(ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    let old_playlist = playlist.clone();
    ui.shuffle = !ui.shuffle;
    let current_path = playlist.get(ui.current).cloned();

    if ui.shuffle {
        // Shuffle everything after the currently-playing track
        let start = ui.current + 1;
        if start < playlist.len() {
            let tail = &mut playlist[start..];
            crate::playlist::shuffle_list(tail);
        }
        ui.set_status("Shuffle ON".to_string());
    } else {
        // Sort and re-locate the current track
        playlist.sort();
        if let Some(ref cp) = current_path {
            if let Some(idx) = playlist.iter().position(|p| p == cp) {
                ui.current = idx;
            }
        }
        ui.set_status("Shuffle OFF".to_string());
    }
    // Cached metadata is indexed by position — remap it to match the reordered paths.
    ui.metadata_cache.reindex(playlist, &old_playlist);
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

    // Move track in playlist and metadata cache
    let path = playlist.remove(track_idx);
    let dst = if track_idx < target { target - 1 } else { target };
    playlist.insert(dst, path);
    ui.metadata_cache.move_entry(track_idx, dst);

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
    ui.current = 0;
    ui.cursor = 0;
    ui.scroll_offset = 0;

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(0, Ordering::Relaxed);

    // Reindex metadata cache: cancel old scan, remap entries, spawn fresh scan
    ui.metadata_cache.cancel.store(true, Ordering::Relaxed);
    if let Some(h) = ui.scan_handle.take() {
        h.join().ok();
    }
    ui.metadata_cache.reindex(playlist, &old_playlist);
    ui.metadata_cache.cancel.store(false, Ordering::Relaxed);
    ui.scan_handle = Some(crate::metadata::spawn_metadata_scan(
        playlist.clone(),
        std::sync::Arc::clone(&ui.metadata_cache),
    ));

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

    // Reindex metadata cache: cancel old scan, remap entries, spawn new scan
    ui.metadata_cache.cancel.store(true, Ordering::Relaxed);
    if let Some(h) = ui.scan_handle.take() {
        h.join().ok();
    }
    ui.metadata_cache.reindex(playlist, &old_playlist);
    ui.metadata_cache.cancel.store(false, Ordering::Relaxed);
    ui.scan_handle = Some(crate::metadata::spawn_metadata_scan(
        playlist.clone(),
        std::sync::Arc::clone(&ui.metadata_cache),
    ));

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
                KeyCode::Backspace => {
                    if buf.pop().is_some() {
                        print!("\x08 \x08");
                        io::stdout().flush().ok();
                    }
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
