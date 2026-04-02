use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;
use std::sync::atomic::Ordering;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;

use crate::state::{
    PlayerState, VizMode, VizStyle, RING_BUFFER_SIZE,
    C_RESET, C_BOLD, C_DIM, C_CYAN, C_GREEN, C_YELLOW, C_MAGENTA, C_RED,
    ViewMode, InputMode, UiState,
};
use crate::viz::{
    StatsMonitor, render_vu_meter, render_spectrum_horizontal,
    render_spectrum_vertical, get_viz_line_count,
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

pub fn print_status(state: &PlayerState, ui: &mut UiState, name: &str, track_info: &str, ext: &str, eq_preset: &crate::eq::EqPreset, fx_name: &str, cf_name: &str, stats: &mut StatsMonitor, prev_viz_lines: usize, playlist: &[PathBuf]) -> usize {
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
    let raw_buf_pct = buf as f32 / RING_BUFFER_SIZE as f32 * 100.0;
    stats.update_buf(raw_buf_pct);
    let buf_pct = stats.smoothed_buf_pct as u32;

    // Truncate name to fit: leave room for track counter, icon, and track info
    // Format: "[N/M] ♪ NAME INFO" — overhead is ~10 + track_info.len()
    let overhead = format!("[{track}/{total}] ♪  ").len() + track_info.len() + 1;
    let max_name = term_w.saturating_sub(overhead).min(35);
    let display_name = if name.len() > max_name {
        if max_name > 1 {
            format!("{}…", &name[..max_name - 1])
        } else {
            "…".to_string()
        }
    } else {
        name.to_string()
    };

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

        // Separator
        print!("\n\r\x1B[K  {C_DIM}{}{C_RESET}", "─".repeat(term_w.saturating_sub(2)));

        // Ensure cursor is visible
        if ui.cursor >= ui.scroll_offset + visible_rows {
            ui.scroll_offset = ui.cursor.saturating_sub(visible_rows - 1);
        }
        if ui.cursor < ui.scroll_offset {
            ui.scroll_offset = ui.cursor;
        }

        let search_active = matches!(&ui.input_mode, InputMode::Search(q) if !q.is_empty());
        let items: Vec<usize> = if search_active && ui.filtered_indices.is_empty() {
            Vec::new()
        } else if !search_active && ui.filtered_indices.is_empty() {
            (0..playlist.len()).collect()
        } else {
            ui.filtered_indices.clone()
        };

        if items.is_empty() && search_active {
            print!("\n\r\x1B[K  {C_DIM}(no matches){C_RESET}");
            for _ in 1..visible_rows {
                print!("\n\r\x1B[K");
            }
        } else {
            let display_items: Vec<usize> = items.iter()
                .skip(ui.scroll_offset)
                .take(visible_rows)
                .copied()
                .collect();

            for (row, &track_idx) in display_items.iter().enumerate() {
                let list_pos = ui.scroll_offset + row;
                let is_playing = track_idx == ui.current;
                let is_cursor = list_pos == ui.cursor;
                let fname = ui.metadata_cache.display_name(track_idx, &playlist[track_idx]);

                let marker = if is_playing { "▶" } else { " " };
                let num = format!("{:>4}", track_idx + 1);

                let line = if is_cursor && is_playing {
                    format!(" {marker} \x1B[7m{C_GREEN}{num}  {fname}{C_RESET}")
                } else if is_cursor {
                    format!(" {marker} \x1B[7m{num}  {fname}\x1B[27m")
                } else if is_playing {
                    format!(" {marker} {C_GREEN}{num}  {fname}{C_RESET}")
                } else {
                    format!(" {marker} {C_DIM}{num}{C_RESET}  {fname}")
                };

                print!("\n\r\x1B[K{}", truncate_ansi(&line, term_w));
            }

            // Pad remaining rows
            for _ in display_items.len()..visible_rows {
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
                    format!("  {C_DIM}[L] close  [↑↓] scroll  [Enter] play  [/] search  [D] remove  [S] save{C_RESET}")
                }
            }
        };
        print!("\n\r\x1B[K{}", truncate_ansi(&footer, term_w));

        let total_lines = 1 + visible_rows + 1; // separator + rows + footer
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

            // In text input mode, route to text handler
            match &ui.input_mode {
                InputMode::Search(_) | InputMode::SavePlaylist(_) => {
                    return handle_text_input(state, ui, playlist, k);
                }
                InputMode::Normal => {}
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
                        ViewMode::Player => {
                            ui.cursor = ui.current;
                            ensure_cursor_visible(ui, playlist);
                            ViewMode::Playlist
                        }
                        ViewMode::Playlist => ViewMode::Player,
                    };
                }
                KeyEvent { code: KeyCode::Char('s'), .. } => {
                    ui.input_mode = InputMode::SavePlaylist(String::new());
                }
                KeyEvent { code: KeyCode::Char('r'), .. } => {
                    rescan(state, ui, playlist);
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
