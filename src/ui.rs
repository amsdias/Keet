use std::io::{self, Write};
use std::time::Duration;
use std::sync::atomic::Ordering;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;

use crate::state::{
    PlayerState, VizMode, VizStyle, RING_BUFFER_SIZE,
    C_RESET, C_BOLD, C_DIM, C_CYAN, C_GREEN, C_YELLOW, C_MAGENTA, C_RED,
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

pub fn print_status(state: &PlayerState, name: &str, track_info: &str, ext: &str, eq_preset: &crate::eq::EqPreset, fx_name: &str, stats: &mut StatsMonitor, prev_viz_lines: usize) -> usize {
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
    let style_name = viz_style.name();
    let line2 = format!("  {icon_color}{icon}{C_RESET} {C_BOLD}[{cur}/{tot}]{C_RESET} {C_GREEN}{bar_filled}{C_RESET} {C_DIM}vol:{vol}%{eq_display}{fx_display} {fader} buf:{buf_pct}% cpu:{:.1}% mem:{:.0}M [V]:{} [B]:{style_name}{C_RESET}",
           stats.cpu_usage, stats.memory_mb, viz_mode.name());
    print!("\r\x1B[K{}", truncate_ansi(&line2, term_w));

    // EQ curve visualization (when non-Flat preset is active)
    if eq_line {
        print!("\n\r\x1B[K{}", eq_curve);
    }

    // Separation line
    if viz_mode != VizMode::None {
        print!("\n\r\x1B[K  {C_DIM}{}{C_RESET}", "─".repeat(term_w.saturating_sub(2)));
    }

    // Render visualization
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

    // Clear any leftover lines from previous wider viz mode
    print!("\x1B[J");

    io::stdout().flush().ok();
    viz_lines
}

pub fn poll_input(state: &PlayerState) -> bool {
    if event::poll(Duration::ZERO).unwrap_or(false) {
        if let Ok(Event::Key(k)) = event::read() {
            if k.kind != KeyEventKind::Press {
                return false;
            }
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
                KeyEvent { code: KeyCode::Char('r'), .. } => state.cycle_effects(),
                KeyEvent { code: KeyCode::Char('f'), .. } => state.toggle_pre_fader(),
                KeyEvent { code: KeyCode::Char('b'), .. } => state.toggle_viz_style(),
                KeyEvent { code: KeyCode::Char('q'), .. } |
                KeyEvent { code: KeyCode::Esc, .. } => { state.quit(); return true; }
                KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. } => {
                    state.quit(); return true;
                }
                _ => {}
            }
        }
    }
    false
}
