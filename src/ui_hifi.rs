//! Hi-Fi theme — Now Playing renderer.
//!
//! Amber-CRT studio-monitor aesthetic. Matches `variant-c-screens.jsx`:
//! double-line header strip + transport row with segmented time box +
//! single-bordered VU panel with dB scale + knob rack + dot-marquee key bar.
//!
//! Same line-count contract as Classic and Minimal: returns lines drawn
//! *below* the anchor (line 1). The anchor is the header strip's top edge.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crossterm::terminal;

use crate::ansi::{truncate_ansi, visible_len};
use crate::state::{InputMode, PlayerState, UiState};
use crate::theme::{palette, ThemeKind};
use crate::viz::StatsMonitor;

#[allow(clippy::too_many_arguments)] // cohesive render context; bundling into a struct adds no clarity
pub fn print_status_hifi(
    state: &PlayerState,
    ui: &mut UiState,
    name: &str,
    eq_preset: &crate::eq::EqPreset,
    fx_name: &str,
    cf_name: &str,
    stats: &mut StatsMonitor,
    prev_viz_lines: usize,
) -> usize {
    let p = palette(ThemeKind::HiFi);
    let term_w = terminal::size().map(|(w, _)| w as usize).unwrap_or(120);

    if prev_viz_lines != usize::MAX && prev_viz_lines > 0 {
        print!("\x1B[{}F", prev_viz_lines);
    }

    // Pull metadata
    let idx = state.current_track.load(Ordering::Relaxed);
    let title = ui
        .metadata_cache
        .title(idx)
        .unwrap_or_else(|| name.to_string());
    let (artist, album) = ui.metadata_cache.artist_album(idx);

    // Buffer pct (smoothed)
    let buf = state.buffer_level.load(Ordering::Relaxed);
    let ring_cap = state.ring_capacity.load(Ordering::Relaxed).max(1);
    stats.update_buf(buf as f32 / ring_cap as f32 * 100.0);
    let buf_pct = stats.smoothed_buf_pct as u32;

    // Width budget. Keep the cap modest so the design stays poster-like
    // even on very wide terminals.
    let inner_w = term_w.saturating_sub(4).clamp(60, 110);

    // === Anchor (line 1): top edge of header strip (double border) ===
    let mut w = crate::ui::FrameWriter::new();
    let top = format!("╔{}╗", "═".repeat(inner_w));
    w.first_line(&format!("  {fg}{bar}{rst}", fg = p.fg, rst = p.reset, bar = top));

    // === Header strip content row: K E E T │ ▶ PLAY · SHUFFLE · RPT-ALL  ...  TRACK 05 / 34 ===
    let track_n = state.current_track.load(Ordering::Relaxed) + 1;
    let track_total = state.total_tracks.load(Ordering::Relaxed);
    let shuffle_on = ui.shuffle;
    let repeat_label = repeat_short(state.repeat_mode());
    let play_label = if state.is_paused() { "⏸ PAUSE" } else { "▶ PLAY" };

    let mut header_left = String::new();
    header_left.push_str(&format!(
        "{accent}{bold}K E E T{rst}",
        accent = p.accent, bold = p.bold, rst = p.reset,
    ));
    header_left.push_str(&format!("  {dim}│{rst}  ", dim = p.dim, rst = p.reset));
    header_left.push_str(&format!(
        "{accent}{play}{rst}",
        accent = p.accent, rst = p.reset, play = play_label,
    ));
    header_left.push_str(&format!("  {dim}·{rst}  ", dim = p.dim, rst = p.reset));
    header_left.push_str(&format!(
        "{c}SHUFFLE{rst}",
        c = if shuffle_on { p.fg } else { p.dim }, rst = p.reset,
    ));
    header_left.push_str(&format!("  {dim}·{rst}  ", dim = p.dim, rst = p.reset));
    header_left.push_str(&format!(
        "{c}{rep}{rst}",
        c = if repeat_label != "RPT-OFF" { p.fg } else { p.dim }, rst = p.reset, rep = repeat_label,
    ));
    let header_right = format!(
        "{dim}TRACK{rst} {fg}{n:02}{rst} {dim}/ {tot}{rst}",
        dim = p.dim, fg = p.fg, rst = p.reset, n = track_n, tot = track_total,
    );

    let lvis = visible_len(&header_left);
    let rvis = visible_len(&header_right);
    let row_inner_w = inner_w.saturating_sub(2);
    let hpad = row_inner_w.saturating_sub(lvis + rvis).max(2);
    w.line(&format!(
        "  {fg}║{rst} {left}{gap}{right} {fg}║{rst}",
        fg = p.fg, rst = p.reset,
        left = header_left, gap = " ".repeat(hpad), right = header_right,
    ));

    // === Bottom edge of header strip ===
    let header_bot = format!("╚{}╝", "═".repeat(inner_w));
    w.line(&format!("  {fg}{bar}{rst}", fg = p.fg, rst = p.reset, bar = header_bot));

    // Spacer
    w.line("");

    // === Transport row: segmented time box (3 lines) + title/meta/progress on right ===
    let progress = if state.total_secs() > 0.0 {
        (state.time_secs() / state.total_secs()).min(1.0)
    } else { 0.0 };
    let cur = format_time(state.time_secs());
    let tot = format_time(state.total_secs());

    // Segmented time box (single border, per CSS in mock):
    //   ┌─────────┐
    //   │  02:28  │
    //   └─────────┘
    let seg_w = 9; // inner width including spaces around time
    let seg_top = format!("┌{}┐", "─".repeat(seg_w));
    let seg_mid_inner = pad_or_truncate(&format!(" {} ", cur), seg_w);
    let seg_bot = format!("└{}┘", "─".repeat(seg_w));

    // Right column: title (UPPERCASE bold accent), meta, then progress + " / 04:32"
    let title_up = title.to_uppercase();
    let mut meta = String::new();
    if let Some(a) = artist.as_ref() { meta.push_str(&a.to_uppercase()); }
    if let Some(al) = album.as_ref() {
        if !meta.is_empty() { meta.push_str("  ·  "); }
        meta.push_str(&al.to_uppercase());
    }
    let src_rate = state.sample_rate.load(Ordering::Relaxed) as u32;
    let out_rate = state.output_rate.load(Ordering::Relaxed) as u32;
    let bits = state.bits_per_sample.load(Ordering::Relaxed);
    let rate_part = if src_rate == out_rate {
        format!("{}-BIT {:.1}K", bits, src_rate as f32 / 1000.0)
    } else {
        format!("{}-BIT {:.1}→{:.1}K", bits, src_rate as f32 / 1000.0, out_rate as f32 / 1000.0)
    };
    if !meta.is_empty() { meta.push_str("  ·  "); }
    meta.push_str(&rate_part);

    // Right column width = inner_w - seg_box_total_w - gap
    let seg_total_w = seg_w + 2; // borders included
    let gap_left = 4; // spacing between seg box and right column
    let right_w = inner_w.saturating_sub(seg_total_w + gap_left).max(20);

    // Row 1: seg_top  +  title (right column)
    let title_truncated = pad_or_truncate(&title_up, right_w);
    w.line(&format!(
        "  {fg}{seg}{rst}{gap}{accent}{bold}{title}{rst}",
        fg = p.fg, accent = p.accent, bold = p.bold, rst = p.reset,
        seg = seg_top,
        gap = " ".repeat(gap_left),
        title = title_truncated,
    ));

    // Row 2: seg_mid  +  meta
    let meta_truncated = pad_or_truncate(&meta, right_w);
    w.line(&format!(
        "  {fg}│{rst}{accent}{bold}{val}{rst}{fg}│{rst}{gap}{dim}{meta}{rst}",
        fg = p.fg, accent = p.accent, bold = p.bold, dim = p.dim, rst = p.reset,
        val = seg_mid_inner,
        gap = " ".repeat(gap_left),
        meta = meta_truncated,
    ));

    // Row 3: seg_bot  +  progress + " / TOTAL"
    let bar_w = right_w.saturating_sub(tot.chars().count() + 4).max(20);
    let bar = render_solid_bar(progress, bar_w);
    w.line(&format!(
        "  {fg}{seg}{rst}{gap}{accent}{bar}{rst}  {dim}/ {tot}{rst}",
        fg = p.fg, accent = p.accent, dim = p.dim, rst = p.reset,
        seg = seg_bot,
        gap = " ".repeat(gap_left),
        bar = bar, tot = tot,
    ));

    // Spacer
    w.line("");

    // === VU panel (single border) ===
    let (lp, rp) = state.get_peaks();
    let (lp_dot, rp_dot) = state.get_vu_dots();
    w.line(&format!(
        "  {fg}┌{bar}┐{rst}",
        fg = p.fg, rst = p.reset, bar = "─".repeat(inner_w),
    ));

    // Label row inside box. Inner content between the two `│` chars must span
    // `inner_w` columns to match the box top `┌─*inner_w┐`. The format adds a
    // leading and trailing space inside, so the label itself is `inner_w - 2`.
    let label_inner = pad_or_truncate("VU METER", inner_w.saturating_sub(2));
    w.line(&format!(
        "  {fg}│{rst} {dim}{label}{rst} {fg}│{rst}",
        fg = p.fg, dim = p.dim, rst = p.reset, label = label_inner,
    ));

    // L/R bars: row inner content (between │ │) is " L {bar}{tail} " — that's
    // 4 fixed chars (lead space, L, space, trailing space) + meter_w + l_pad.
    // Solve for l_pad so that 4 + meter_w + l_pad == inner_w.
    let meter_w = inner_w.saturating_sub(8).max(24);
    let l_bar = vu_bar(lp, lp_dot, meter_w, p);
    let r_bar = vu_bar(rp, rp_dot, meter_w, p);
    let l_pad = inner_w.saturating_sub(meter_w + 4);
    w.line(&format!(
        "  {fg}│{rst} {dim}L{rst} {bar}{tail} {fg}│{rst}",
        fg = p.fg, dim = p.dim, rst = p.reset, bar = l_bar, tail = " ".repeat(l_pad),
    ));
    w.line(&format!(
        "  {fg}│{rst} {dim}R{rst} {bar}{tail} {fg}│{rst}",
        fg = p.fg, dim = p.dim, rst = p.reset, bar = r_bar, tail = " ".repeat(l_pad),
    ));

    // dB scale row: `   {scale}{tail} ` must total `inner_w`.
    let scale = render_db_scale(meter_w, p);
    let scale_visible = visible_len(&scale);
    let scale_pad = inner_w.saturating_sub(scale_visible + 4);
    w.line(&format!(
        "  {fg}│{rst}   {scale}{tail} {fg}│{rst}",
        fg = p.fg, rst = p.reset, scale = scale, tail = " ".repeat(scale_pad),
    ));

    // Bottom
    w.line(&format!(
        "  {fg}└{bar}┘{rst}",
        fg = p.fg, rst = p.reset, bar = "─".repeat(inner_w),
    ));

    // Spacer
    w.line("");

    // === Knob rack: 6 cells, single border each ===
    let vol = state.volume.load(Ordering::Relaxed);
    let bal_val = state.balance_value();
    let bal_str = if bal_val == 0 {
        "C".to_string()
    } else if bal_val < 0 {
        format!("L{}", -bal_val)
    } else {
        format!("R{}", bal_val)
    };
    let knobs: Vec<(&str, String, bool)> = vec![
        ("VOL",   format!("{}", vol),                   false),
        ("EQ",    eq_preset.name.to_uppercase(),        false),
        ("FX",    fx_name.to_uppercase(),               false),
        ("XFEED", cf_name.to_uppercase(),               false),
        ("BAL",   bal_str,                              false),
        ("BUF",   format!("{}", buf_pct),               buf_pct >= 60),
    ];
    let knob_unit: Vec<&str> = vec!["%", "", "", "", "", "%"];

    // Knob cell width: total inner / 6 minus gap
    let gap_w = 1usize;
    let cell_w = (inner_w.saturating_sub(gap_w * (knobs.len() - 1))) / knobs.len();
    let cell_w = cell_w.clamp(8, 14);
    let total_used = cell_w * knobs.len() + gap_w * (knobs.len() - 1);
    let lead = 2 + (inner_w.saturating_sub(total_used) / 2);

    let mut top_row = String::new();
    let mut label_row = String::new();
    let mut value_row = String::new();
    let mut bot_row = String::new();
    top_row.push_str(&" ".repeat(lead));
    label_row.push_str(&" ".repeat(lead));
    value_row.push_str(&" ".repeat(lead));
    bot_row.push_str(&" ".repeat(lead));
    for (i, (label, value, good)) in knobs.iter().enumerate() {
        if i > 0 {
            top_row.push(' ');
            label_row.push(' ');
            value_row.push(' ');
            bot_row.push(' ');
        }
        top_row.push_str(&format!(
            "{fg}┌{bar}┐{rst}",
            fg = p.fg, rst = p.reset, bar = "─".repeat(cell_w.saturating_sub(2)),
        ));
        let cell_inner = cell_w.saturating_sub(2);
        // Label row: dim, centered.
        let pl = label.chars().count();
        let lpad = (cell_inner.saturating_sub(pl)) / 2;
        let rpad = cell_inner.saturating_sub(pl + lpad);
        label_row.push_str(&format!(
            "{fg}│{rst}{lp}{dim}{label}{rst}{rp}{fg}│{rst}",
            fg = p.fg, dim = p.dim, rst = p.reset,
            lp = " ".repeat(lpad), rp = " ".repeat(rpad), label = label,
        ));
        // Value row: bold accent, with optional dim unit suffix.
        let unit = knob_unit[i];
        let val_color = if *good { p.good } else { p.accent };
        let val_visible = value.chars().count() + if unit.is_empty() { 0 } else { 1 + unit.chars().count() };
        let v_lpad = (cell_inner.saturating_sub(val_visible)) / 2;
        let v_rpad = cell_inner.saturating_sub(val_visible + v_lpad);
        let val_styled = if unit.is_empty() {
            format!(
                "{bold}{vc}{val}{rst}",
                bold = p.bold, vc = val_color, rst = p.reset, val = value,
            )
        } else {
            format!(
                "{bold}{vc}{val}{rst} {dim}{unit}{rst}",
                bold = p.bold, vc = val_color, rst = p.reset,
                dim = p.dim, val = value, unit = unit,
            )
        };
        value_row.push_str(&format!(
            "{fg}│{rst}{lp}{val}{rp}{fg}│{rst}",
            fg = p.fg, rst = p.reset,
            lp = " ".repeat(v_lpad), val = val_styled, rp = " ".repeat(v_rpad),
        ));
        bot_row.push_str(&format!(
            "{fg}└{bar}┘{rst}",
            fg = p.fg, rst = p.reset, bar = "─".repeat(cell_w.saturating_sub(2)),
        ));
    }
    w.line(&top_row);
    w.line(&label_row);
    w.line(&value_row);
    w.line(&bot_row);

    // Spacer
    w.line("");

    // === Marquee key bar ===
    let footer = if let Some(msg) = ui.active_status() {
        format!("  {accent}{msg}{rst}", accent = p.accent, rst = p.reset, msg = msg)
    } else {
        hifi_marquee_keys(p, term_w)
    };
    w.line(&truncate_ansi(&footer, term_w));

    print!("\x1B[J");
    io::stdout().flush().ok();

    w.count()
}

fn repeat_short(mode: crate::state::RepeatMode) -> &'static str {
    use crate::state::RepeatMode::*;
    match mode {
        Off => "RPT-OFF",
        One => "RPT-ONE",
        All => "RPT-ALL",
    }
}

fn render_db_scale(meter_w: usize, p: &crate::theme::Palette) -> String {
    let labels: &[(&str, f32, bool /*red*/)] = &[
        ("-∞", 0.00, false),
        ("-30", 0.20, false),
        ("-20", 0.40, false),
        ("-12", 0.60, false),
        ("-6",  0.75, false),
        ("-3",  0.85, false),
        ("0dB", 1.00, true),
    ];
    let mut out = String::new();
    let mut placed: usize = 0;
    for (i, (label, frac, red)) in labels.iter().enumerate() {
        let target = (frac * meter_w as f32) as usize;
        // For first label, place at column 0; for last, anchor flush to meter_w end.
        let col = if i == labels.len() - 1 {
            meter_w.saturating_sub(label.chars().count())
        } else {
            target
        };
        if col > placed {
            out.push_str(&" ".repeat(col - placed));
            placed = col;
        }
        let color = if *red { p.danger } else { p.dim };
        out.push_str(color);
        out.push_str(label);
        out.push_str(p.reset);
        placed += label.chars().count();
    }
    out
}

fn hifi_marquee_keys(p: &crate::theme::Palette, term_w: usize) -> String {
    // Variant-c key bar: " ␣ PLAY · ←→ SEEK · ↑↓ TRACK · +/− VOL · E EQ · X FX · V VIZ · L LIB · Y LYRICS · Q QUIT"
    let pairs: &[(&str, &str)] = &[
        ("␣",   "PLAY"),
        ("←→",  "SEEK"),
        ("↑↓",  "TRACK"),
        ("+/−", "VOL"),
        ("E",   "EQ"),
        ("X",   "FX"),
        ("V",   "VIZ"),
        ("L",   "LIB"),
        ("Y",   "LYRICS"),
        ("T",   "THEME"),
    ];
    let mut s = String::with_capacity(200);
    s.push_str("  ");
    for (i, (k, label)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push_str(&format!(" {dim}·{rst} ", dim = p.dim, rst = p.reset));
        }
        s.push_str(p.accent);
        s.push_str(k);
        s.push_str(p.reset);
        s.push(' ');
        s.push_str(p.dim);
        s.push_str(label);
        s.push_str(p.reset);
    }
    let _ = term_w;
    s
}

/// VU bar without channel label (label is rendered outside the bar by the
/// caller). 0.85+ region renders in danger color, peak-hold dot in fg/danger.
fn vu_bar(
    peak: f32,
    peak_dot: f32,
    width: usize,
    p: &crate::theme::Palette,
) -> String {
    let bar_w = width.max(8);
    let seg = (peak.clamp(0.0, 1.0) * bar_w as f32) as usize;
    let dot = (peak_dot.clamp(0.0, 1.0) * bar_w as f32) as usize;
    let hot_threshold = (bar_w as f32 * 0.85) as usize;

    let mut bar = String::with_capacity(bar_w * 4);
    for i in 0..bar_w {
        let in_hot = i >= hot_threshold;
        if i < seg {
            if in_hot { bar.push_str(p.danger); } else { bar.push_str(p.accent); }
            bar.push('█');
        } else if i == dot && dot >= seg {
            if in_hot { bar.push_str(p.danger); } else { bar.push_str(p.fg); }
            bar.push('█');
        } else {
            bar.push_str(p.rule);
            bar.push('·');
        }
    }
    bar.push_str(p.reset);
    bar
}


fn render_solid_bar(progress: f64, width: usize) -> String {
    const PARTIALS: &[char] = &['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    // Two-tone bar: bright accent fill (the caller has already selected the
    // accent foreground) meeting a solid dim-amber rail. The rail's colour is
    // used both as its own foreground AND as the boundary cell's background, so
    // the bright fill and dim rail always meet seamlessly — a left-partial block
    // only inks its left `frac/8`, so its empty right must be painted the rail
    // colour or the terminal background (black) shows through as a notch that
    // oscillates as the cell fills. A solid rail (not the old ░ stipple) lets
    // the boundary background match it exactly.
    const RAIL_FG: &str = "\x1B[38;2;122;94;58m"; // dim amber, foreground
    const RAIL_BG: &str = "\x1B[48;2;122;94;58m"; // same colour, as background
    let sub = (progress * width as f64).max(0.0);
    let full = sub as usize;
    let frac = ((sub - full as f64) * 8.0) as usize;
    let has_partial = full < width && frac > 0;
    let mut s = String::with_capacity(width * 8);

    // Bright fill.
    for _ in 0..full.min(width) {
        s.push('█');
    }

    // Boundary cell: bright fill on the left, rail colour on the right (its
    // background), so the fill grows into the rail with no gap. Reset only the
    // background afterwards — the accent foreground still applies to the fill.
    if has_partial {
        s.push_str(RAIL_BG);
        s.push(PARTIALS[(frac - 1).min(PARTIALS.len() - 1)]);
        s.push_str("\x1B[49m");
    }

    // Dim rail: solid blocks in the rail colour (shrinks as the fill grows).
    let tail = width.saturating_sub(full + if has_partial { 1 } else { 0 });
    if tail > 0 {
        s.push_str(RAIL_FG);
        for _ in 0..tail {
            s.push('█');
        }
    }
    s
}

fn format_time(secs: f64) -> String {
    let m = (secs / 60.0) as u32;
    let s = (secs % 60.0) as u32;
    format!("{:02}:{:02}", m, s)
}

/// Pad with spaces to `width` (or truncate with ellipsis if too long).
fn pad_or_truncate(s: &str, width: usize) -> String {
    let visible = s.chars().count();
    if visible == width {
        s.to_string()
    } else if visible < width {
        let mut out = String::from(s);
        out.push_str(&" ".repeat(width - visible));
        out
    } else if width > 1 {
        let mut out: String = s.chars().take(width - 1).collect();
        out.push('…');
        out
    } else {
        s.chars().take(width).collect()
    }
}


/// HiFi Library: double-bordered header strip + single-bordered list area
/// + dot-marquee key bar. Matches `variant-c-screens.jsx::CLibrary`.
pub fn print_status_hifi_library(
    _state: &PlayerState,
    ui: &mut UiState,
    _name: &str,
    prev_viz_lines: usize,
    playlist: &[PathBuf],
) -> usize {
    let p = palette(ThemeKind::HiFi);
    let term_w = terminal::size().map(|(w, _)| w as usize).unwrap_or(120);
    let term_h = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);

    if prev_viz_lines != usize::MAX && prev_viz_lines > 0 {
        print!("\x1B[{}F", prev_viz_lines);
    }

    let inner_w = term_w.saturating_sub(4).clamp(60, 110);
    let row_inner_w = inner_w.saturating_sub(2);

    // === Anchor (line 1): top edge of header strip (double border) ===
    let mut w = crate::ui::FrameWriter::new();
    let top = format!("╔{}╗", "═".repeat(inner_w));
    w.first_line(&format!("  {fg}{bar}{rst}", fg = p.fg, rst = p.reset, bar = top));

    // === Header content row: "L I B R A R Y" + right "N TRK · Hh Mm" ===
    let total_secs: f64 = (0..playlist.len())
        .filter_map(|i| ui.metadata_cache.duration(i))
        .sum();
    let dur_summary = format_dur_short(total_secs);
    let header_left = format!(
        "{accent}{bold}L I B R A R Y{rst}",
        accent = p.accent, bold = p.bold, rst = p.reset,
    );
    let header_right = format!(
        "{dim}{n} TRK · {dur}{rst}",
        dim = p.dim, rst = p.reset, n = playlist.len(), dur = dur_summary,
    );
    let lvis = visible_len(&header_left);
    let rvis = visible_len(&header_right);
    let hpad = row_inner_w.saturating_sub(lvis + rvis).max(2);
    w.line(&format!(
        "  {fg}║{rst} {left}{gap}{right} {fg}║{rst}",
        fg = p.fg, rst = p.reset,
        left = header_left, gap = " ".repeat(hpad), right = header_right,
    ));

    // Bottom edge of header strip
    let header_bot = format!("╚{}╝", "═".repeat(inner_w));
    w.line(&format!("  {fg}{bar}{rst}", fg = p.fg, rst = p.reset, bar = header_bot));

    // === List area: single border ===
    let list_top = format!("┌{}┐", "─".repeat(inner_w));
    w.line(&format!("  {fg}{bar}{rst}", fg = p.fg, rst = p.reset, bar = list_top));

    // Compute visible rows. Above: anchor + header_bot + list_top = 2 below the anchor.
    // Below: list_bot + spacer + key bar = 3.
    // Cap the list area itself to 20 rows so the screen stays poster-like
    // on tall terminals; on shorter ones it shrinks to fit.
    let header_consumed = 2;
    let footer_consumed = 3;
    const HIFI_BODY_CAP: usize = 20;
    let visible_rows = term_h
        .saturating_sub(header_consumed + footer_consumed + ui.banner_lines + 1)
        .clamp(1, HIFI_BODY_CAP);
    ui.last_visible_rows = visible_rows;

    if ui.library_tree_mode {
        // Tree body wrapped in the HiFi list frame (each row is `│ … │`).
        let lines = crate::ui::render_tree_body(ui, visible_rows, inner_w, p);
        for i in 0..visible_rows {
            match lines.get(i) {
                Some(line) => {
                    let pad = inner_w.saturating_sub(visible_len(line));
                    w.line(&format!(
                        "  {fg}│{rst}{line}{sp}{fg}│{rst}",
                        fg = p.fg, rst = p.reset, line = line, sp = " ".repeat(pad),
                    ));
                }
                None => {
                    w.line(&format!(
                        "  {fg}│{rst}{blank}{fg}│{rst}",
                        fg = p.fg, rst = p.reset, blank = " ".repeat(inner_w),
                    ));
                }
            }
        }
    } else {
    let search_active = matches!(&ui.input_mode, InputMode::Search(q) if !q.is_empty());
    let items_len = if search_active && ui.filtered_indices.is_empty() {
        0
    } else if ui.filtered_indices.is_empty() {
        playlist.len()
    } else {
        ui.filtered_indices.len()
    };

    let scroll_margin = 4.min(visible_rows / 2);
    if ui.cursor >= ui.scroll_offset + visible_rows.saturating_sub(scroll_margin) {
        ui.scroll_offset = ui.cursor.saturating_sub(visible_rows.saturating_sub(scroll_margin + 1));
    }
    if ui.cursor < ui.scroll_offset + scroll_margin {
        ui.scroll_offset = ui.cursor.saturating_sub(scroll_margin);
    }
    let max_offset = items_len.saturating_sub(visible_rows);
    ui.scroll_offset = ui.scroll_offset.min(max_offset);

    // Row layout (inside the box, padded "  " on each side). Total body
    // visible chars (between the two `│`) must equal `inner_w` to match the
    // box top `┌─*inner_w┐`.
    //   pad_l | marker | sp | num | sp | name | sp | dur | pad_r
    let pad_l = 2usize;
    let pad_r = 2usize;
    let marker_w = 1usize;
    let num_w = 2usize;
    let dur_w = 5usize;
    let inter = 1usize;
    let name_w = inner_w
        .saturating_sub(pad_l + marker_w + inter + num_w + inter + inter + dur_w + pad_r)
        .max(10);

    if items_len == 0 && search_active {
        let inner_text = pad_or_truncate("  (no matches)", inner_w);
        w.line(&format!(
            "  {fg}│{rst}{dim}{txt}{rst}{fg}│{rst}",
            fg = p.fg, rst = p.reset, dim = p.dim, txt = inner_text,
        ));
        for _ in 1..visible_rows {
            let blank = " ".repeat(inner_w);
            w.line(&format!(
                "  {fg}│{rst}{txt}{fg}│{rst}",
                fg = p.fg, rst = p.reset, txt = blank,
            ));
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
            let row_title = ui
                .metadata_cache
                .title(track_idx)
                .unwrap_or_else(|| ui.metadata_cache.display_name(track_idx, &playlist[track_idx]))
                .to_uppercase();
            let dur_str = match ui.metadata_cache.duration(track_idx) {
                Some(d) => format_time(d),
                None => "  :  ".to_string(),
            };

            let marker = if is_playing { "▶" } else { " " };
            let num = format!("{:0>2}", track_idx + 1);

            let row_color = if is_playing { p.accent } else { p.fg };
            let mark_color = if is_playing { p.accent } else { p.dim };

            let title_truncated = pad_or_truncate(&row_title, name_w);
            let dur_padded = format!("{:>w$}", dur_str, w = dur_w);

            let body = format!(
                "{lp}{mc}{marker}{rst} {dim}{num}{rst} {rc}{name}{rst} {dim}{dur}{rst}{rp}",
                lp = " ".repeat(pad_l), rp = " ".repeat(pad_r),
                mc = mark_color, marker = marker, rst = p.reset,
                dim = p.dim,
                num = num,
                rc = row_color, name = title_truncated,
                dur = dur_padded,
            );

            let line = if is_cursor && !p.cursor_bg.is_empty() {
                format!(
                    "  {fg}│{rst}{bg}{body}{rst}{fg}│{rst}",
                    fg = p.fg, rst = p.reset, bg = p.cursor_bg, body = body,
                )
            } else {
                format!(
                    "  {fg}│{rst}{body}{fg}│{rst}",
                    fg = p.fg, rst = p.reset, body = body,
                )
            };
            w.line(&line);
        }
        for _ in visible_count..visible_rows {
            let blank = " ".repeat(inner_w);
            w.line(&format!(
                "  {fg}│{rst}{txt}{fg}│{rst}",
                fg = p.fg, rst = p.reset, txt = blank,
            ));
        }
    }
    }

    // List bottom border
    let list_bot = format!("└{}┘", "─".repeat(inner_w));
    w.line(&format!("  {fg}{bar}{rst}", fg = p.fg, rst = p.reset, bar = list_bot));

    // Spacer
    w.line("");

    // === Marquee key bar / search prompt / status ===
    let footer = match &ui.input_mode {
        InputMode::Search(query) => format!(
            "  {accent}/{rst} {q}{dim}_{rst}",
            accent = p.accent, rst = p.reset, dim = p.dim, q = query,
        ),
        InputMode::SavePlaylist(saved) => format!(
            "  {dim}SAVE AS:{rst} {n}{dim}_{rst}",
            dim = p.dim, rst = p.reset, n = saved,
        ),
        InputMode::Normal => {
            if let Some(msg) = ui.active_status() {
                format!("  {accent}{msg}{rst}", accent = p.accent, rst = p.reset, msg = msg)
            } else if ui.library_tree_mode {
                format!(
                    "  {dim}[TAB] LIST · [←→] FOLD · [↵] PLAY · [/] FILTER · [D] REMOVE · [L] CLOSE{rst}",
                    dim = p.dim, rst = p.reset,
                )
            } else {
                hifi_library_marquee(p)
            }
        }
    };
    w.line(&truncate_ansi(&footer, term_w));

    print!("\x1B[J");
    io::stdout().flush().ok();
    w.count()
}

fn format_dur_short(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    if h > 0 { format!("{h}H {m:02}M") } else { format!("{m}M") }
}

fn hifi_library_marquee(p: &crate::theme::Palette) -> String {
    let pairs: &[(&str, &str)] = &[
        ("↵", "PLAY"),
        ("↑↓", "NAV"),
        ("/", "SEARCH"),
        ("A", "QUEUE"),
        ("D", "REMOVE"),
        ("S", "SAVE"),
        ("L", "CLOSE"),
    ];
    let mut s = String::with_capacity(160);
    s.push_str("  ");
    for (i, (k, label)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push_str(&format!(" {dim}·{rst} ", dim = p.dim, rst = p.reset));
        }
        s.push_str(p.accent);
        s.push_str(k);
        s.push_str(p.reset);
        s.push(' ');
        s.push_str(p.dim);
        s.push_str(label);
        s.push_str(p.reset);
    }
    s
}

/// HiFi Lyrics: double-bordered header strip + single-bordered body box +
/// dot-marquee key bar. Matches `variant-c-screens.jsx::CLyrics`.
pub fn print_status_hifi_lyrics(
    state: &PlayerState,
    ui: &mut UiState,
    name: &str,
    prev_viz_lines: usize,
) -> usize {
    let p = palette(ThemeKind::HiFi);
    let term_w = terminal::size().map(|(w, _)| w as usize).unwrap_or(120);
    let term_h = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);

    if prev_viz_lines != usize::MAX && prev_viz_lines > 0 {
        print!("\x1B[{}F", prev_viz_lines);
    }

    let inner_w = term_w.saturating_sub(4).clamp(60, 110);
    let row_inner_w = inner_w.saturating_sub(2);

    // === Anchor (line 1): top edge of header strip (double border) ===
    let mut w = crate::ui::FrameWriter::new();
    let top = format!("╔{}╗", "═".repeat(inner_w));
    w.first_line(&format!("  {fg}{bar}{rst}", fg = p.fg, rst = p.reset, bar = top));

    // === Header content row: TITLE accent bold + right "SYNC · {SOURCE} · 02:28 / 04:32" ===
    let idx = state.current_track.load(Ordering::Relaxed);
    let title = ui.metadata_cache.title(idx).unwrap_or_else(|| name.to_string());
    let title_up = title.to_uppercase();
    let is_synced = ui.lyrics.as_ref().map(|l| l.is_synced()).unwrap_or(false);
    let cur_t = format_time(state.time_secs());
    let tot_t = format_time(state.total_secs());

    let mut right_meta = String::new();
    if ui.lyrics.is_some() {
        right_meta.push_str(if is_synced { "SYNC" } else { "PLAIN" });
        right_meta.push_str("  ·  ");
    }
    right_meta.push_str(&format!("{}  /  {}", cur_t, tot_t));

    let header_left = format!(
        "{accent}{bold}{title}{rst}",
        accent = p.accent, bold = p.bold, rst = p.reset, title = title_up,
    );
    let header_right = format!(
        "{dim}{m}{rst}",
        dim = p.dim, rst = p.reset, m = right_meta,
    );
    let lvis = visible_len(&header_left);
    let rvis = visible_len(&header_right);
    let allowed_left = row_inner_w.saturating_sub(rvis + 4);
    let header_left = if lvis > allowed_left {
        format!(
            "{accent}{bold}{title}{rst}",
            accent = p.accent, bold = p.bold, rst = p.reset,
            title = pad_or_truncate(&title_up, allowed_left),
        )
    } else { header_left };
    let lvis = visible_len(&header_left);
    let hpad = row_inner_w.saturating_sub(lvis + rvis).max(2);
    w.line(&format!(
        "  {fg}║{rst} {left}{gap}{right} {fg}║{rst}",
        fg = p.fg, rst = p.reset,
        left = header_left, gap = " ".repeat(hpad), right = header_right,
    ));

    let header_bot = format!("╚{}╝", "═".repeat(inner_w));
    w.line(&format!("  {fg}{bar}{rst}", fg = p.fg, rst = p.reset, bar = header_bot));

    // === Body box: single border, generous padding ===
    let body_top = format!("┌{}┐", "─".repeat(inner_w));
    w.line(&format!("  {fg}{bar}{rst}", fg = p.fg, rst = p.reset, bar = body_top));

    // Reserved: header_strip 1 + header_bot 1 + body_top 1 = 3 below anchor
    // Footer: body_bot 1 + spacer 1 + key bar 1 = 3
    // Cap the lyrics body itself to 20 rows (matches the library cap) so the
    // screen stays poster-like on tall terminals.
    let header_consumed = 3;
    let footer_consumed = 3;
    const HIFI_BODY_CAP: usize = 20;
    let body_rows = term_h
        .saturating_sub(header_consumed + footer_consumed + ui.banner_lines + 1)
        .clamp(1, HIFI_BODY_CAP);

    if let Some(ref lyrics) = ui.lyrics {
        let total = lyrics.line_count();
        let adj = state.time_secs() + ui.lyrics_offset;
        let current_line = lyrics.current_line(adj);

        if lyrics.is_synced() && ui.lyrics_auto_scroll {
            if let Some(cur) = current_line {
                let half = body_rows / 2;
                ui.lyrics_scroll = cur.saturating_sub(half);
            }
        }
        if total > body_rows {
            ui.lyrics_scroll = ui.lyrics_scroll.min(total - body_rows);
        } else {
            ui.lyrics_scroll = 0;
        }

        let lyric_pad_l = 4usize;
        let lyric_pad_r = 4usize;
        // Inner content between `│ │` must equal `inner_w` to match the body
        // top edge. Reserve 2 cols for the ▸ marker / leading "  " gutter.
        let text_w = inner_w.saturating_sub(lyric_pad_l + lyric_pad_r + 2);

        for row in 0..body_rows {
            let line_idx = ui.lyrics_scroll + row;
            if line_idx < total {
                let text = lyrics.line_text(line_idx).to_uppercase();
                let is_current = current_line == Some(line_idx);
                let line_truncated = pad_or_truncate(&text, text_w);
                let inner_str = if is_current {
                    format!(
                        "{lp}{accent}{bold}▸ {txt}{rst}{rp}",
                        lp = " ".repeat(lyric_pad_l), rp = " ".repeat(lyric_pad_r),
                        accent = p.accent, bold = p.bold, rst = p.reset, txt = line_truncated,
                    )
                } else {
                    format!(
                        "{lp}{dim}  {txt}{rst}{rp}",
                        lp = " ".repeat(lyric_pad_l), rp = " ".repeat(lyric_pad_r),
                        dim = p.dim, rst = p.reset, txt = line_truncated,
                    )
                };
                w.line(&format!(
                    "  {fg}│{rst}{inner}{fg}│{rst}",
                    fg = p.fg, rst = p.reset, inner = inner_str,
                ));
            } else {
                let blank = " ".repeat(inner_w);
                w.line(&format!(
                    "  {fg}│{rst}{txt}{fg}│{rst}",
                    fg = p.fg, rst = p.reset, txt = blank,
                ));
            }
        }
    } else {
        let msg = "  (NO LYRICS AVAILABLE)";
        let inner_text = pad_or_truncate(msg, inner_w);
        w.line(&format!(
            "  {fg}│{rst}{dim}{txt}{rst}{fg}│{rst}",
            fg = p.fg, dim = p.dim, rst = p.reset, txt = inner_text,
        ));
        for _ in 1..body_rows {
            let blank = " ".repeat(inner_w);
            w.line(&format!(
                "  {fg}│{rst}{txt}{fg}│{rst}",
                fg = p.fg, rst = p.reset, txt = blank,
            ));
        }
    }

    // Body bottom border
    let body_bot = format!("└{}┘", "─".repeat(inner_w));
    w.line(&format!("  {fg}{bar}{rst}", fg = p.fg, rst = p.reset, bar = body_bot));

    // Spacer
    w.line("");

    // === Marquee key bar ===
    let mut bar = String::new();
    bar.push_str("  ");
    bar.push_str(&format!(
        "{accent}W/S{rst} {dim}SCROLL{rst}",
        accent = p.accent, rst = p.reset, dim = p.dim,
    ));
    if is_synced {
        bar.push_str(&format!(" {dim}·{rst} ", dim = p.dim, rst = p.reset));
        bar.push_str(&format!(
            "{accent}A/D{rst} {dim}SYNC ±0.5s{rst}",
            accent = p.accent, rst = p.reset, dim = p.dim,
        ));
    }
    bar.push_str(&format!(" {dim}·{rst} ", dim = p.dim, rst = p.reset));
    bar.push_str(&format!(
        "{accent}Y{rst} {dim}BACK{rst}",
        accent = p.accent, rst = p.reset, dim = p.dim,
    ));
    if is_synced && ui.lyrics_offset != 0.0 {
        bar.push_str(&format!(
            " {dim}·{rst} {dim}OFFSET {ofs:+.1}s{rst}",
            dim = p.dim, rst = p.reset, ofs = ui.lyrics_offset,
        ));
    }
    w.line(&truncate_ansi(&bar, term_w));

    print!("\x1B[J");
    io::stdout().flush().ok();
    w.count()
}


#[cfg(test)]
mod hifi_tests {
    use super::*;

    #[test]
    fn solid_bar_partial_meets_rail_seamlessly_no_black_notch() {
        // A fractional progress produces a leading partial block. Its empty
        // (right) portion must be painted the EXACT rail colour so the bright
        // fill grows into the dim rail with no black gap between them.
        // width=20, progress=0.51 → full=10, frac=1 → one partial cell.
        let bar = render_solid_bar(0.51, 20);
        let has_partial = "▏▎▍▌▋▊▉".chars().any(|c| bar.contains(c));
        assert!(has_partial, "expected a partial block char, got: {bar:?}");

        // The rail is drawn solid in a dim truecolor foreground (\x1B[38;2;R;G;Bm).
        let rail_rgb = bar
            .split("\x1B[38;2;")
            .nth(1)
            .and_then(|s| s.split('m').next())
            .expect("rail must be drawn in a truecolor foreground");

        // The boundary cell's background must be that SAME colour — otherwise a
        // gap (black, or a mismatched shade) shows where fill meets rail.
        assert!(
            bar.contains(&format!("\x1B[48;2;{rail_rgb}m")),
            "boundary background must equal the rail colour {rail_rgb:?} (seamless), got: {bar:?}"
        );
        // Background reset after the partial so the rail cells aren't tinted.
        assert!(bar.contains("\x1B[49m"), "background not reset after partial: {bar:?}");
    }
}
