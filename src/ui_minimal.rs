//! Minimal theme — Player, Library, and Lyrics renderers.
//!
//! Editorial / monochrome layout. Single warm-cyan accent. Matches the
//! `variant-b-screens.jsx` mocks from the design handoff. Renderers share
//! the line-count contract used by the Classic renderer: they return the
//! number of lines drawn *below* the anchor (line 1). The caller rewinds
//! via `\x1B[<n>F` on the next frame, so mismatched counts leave orphans.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crossterm::terminal;

use crate::state::{InputMode, PlayerState, UiState, VizMode, VizStyle};
use crate::theme::{palette, ThemeKind};
use crate::viz::{
    StatsMonitor, VizAnalyser, analysis_needs_raw_lines, analysis_rows_for_window,
    render_lissajous, render_oscilloscope, render_spectrogram,
    render_spectrogram_analysis, render_spectrum_horizontal,
    render_spectrum_vertical, render_vu_meter,
};

/// Render the Minimal Now Playing screen.
///
/// Layout (single-column for now; SIGNAL details fold into the metadata line):
/// ```text
/// Do I Wanna Know?                                         ← line 1 (anchor, bold)
/// Arctic Monkeys · AM                                      ← line 2 (dim)
///                                                          ← blank
/// NOW PLAYING                                              ← dim small-caps
/// 02:28  ████████░░░░░░░░░░░░  04:32                      ← time + bar + total
/// track 5 of 34 · 24-bit stereo · 44.1k → 96k · vol 100% · buf 76%
///                                                          ← blank
/// SPECTRUM                                                 ← dim small-caps (if viz on)
/// [N viz lines]
///                                                          ← blank
/// ␣ play  ←→ seek  ↑↓ track  L lib  Y lyrics  E eq  ? all
/// ```
#[allow(clippy::too_many_arguments)] // cohesive render context; bundling into a struct adds no clarity
pub fn print_status_minimal(
    state: &PlayerState,
    ui: &mut UiState,
    name: &str,
    _track_info: &str,
    eq_preset: &crate::eq::EqPreset,
    fx_name: &str,
    cf_name: &str,
    stats: &mut StatsMonitor,
    prev_viz_lines: usize,
    analyser: &VizAnalyser,
) -> usize {
    let p = palette(ThemeKind::Minimal);
    let (term_w, term_h) = terminal::size()
        .map(|(w, h)| (w as usize, h as usize))
        .unwrap_or((120, 40));
    let viz_mode = state.viz_mode();
    let viz_style = state.viz_style();

    // Sixel emit-on-change bookkeeping (mirrors Classic): read whether we owned
    // the spectrogram block last frame, then clear — re-asserted below only if
    // the analysis spectrogram actually renders this frame.
    let block_was_intact = ui.spectro_block_intact;
    ui.spectro_block_intact = false;

    if prev_viz_lines != usize::MAX && prev_viz_lines > 0 {
        print!("\x1B[{}F", prev_viz_lines);
    }

    let idx = state.current_track.load(Ordering::Relaxed);
    let title = ui
        .metadata_cache
        .title(idx)
        .unwrap_or_else(|| name.to_string());
    let (artist, album) = ui.metadata_cache.artist_album(idx);

    let buf = state.buffer_level.load(Ordering::Relaxed);
    let ring_cap = state.ring_capacity.load(Ordering::Relaxed).max(1);
    stats.update_buf(buf as f32 / ring_cap as f32 * 100.0);
    let buf_pct = stats.smoothed_buf_pct as u32;

    // === Anchor (line 1): wordmark ===
    let mut w = crate::ui::FrameWriter::new();
    w.first_line(&wordmark_anchor(p));

    // === Line 2: dim rule separating the wordmark from the song info ===
    let rule_w = term_w.saturating_sub(4);
    w.line(&format!(
        "  {rule}{rl}{rst}",
        rule = p.rule, rst = p.reset, rl = "─".repeat(rule_w),
    ));

    // === Line 3: bold title (pushed down so the wordmark survives scroll) ===
    let title_truncated = truncate_plain(&title, term_w.saturating_sub(2));
    w.line(&format!(
        "  {bold}{fg}{title}{rst}",
        bold = p.bold, fg = p.fg, rst = p.reset, title = title_truncated,
    ));

    // === Line 4: dim Artist · Album ===
    let mut sub = String::new();
    if let Some(a) = artist.as_ref() { sub.push_str(a); }
    if let Some(al) = album.as_ref() {
        if !sub.is_empty() { sub.push_str("  ·  "); }
        sub.push_str(al);
    }
    w.line(&format!(
        "  {dim}{sub}{rst}",
        dim = p.dim, rst = p.reset,
        sub = truncate_plain(&sub, term_w.saturating_sub(2)),
    ));

    // === Whitespace gap before NOW PLAYING / SIGNAL block ===
    w.line("");

    // === Two-column block: NOW PLAYING (left) │ SIGNAL (right) ===
    // Layout: 2 (margin) + left_w + 1 (space) + 1 (│) + 1 (space) + signal_w
    //         + 2 (margin) = term_w → left_w = term_w - signal_w - 7
    let signal_w: usize = 30; // right column reserved
    let two_col = term_w >= 70 + signal_w;
    let left_w = if two_col { term_w.saturating_sub(signal_w + 7) } else { term_w.saturating_sub(4) };

    // Build SIGNAL rows (k/v pairs) — only shown if two-col fits. Device name
    // is in the banner already, so SIGNAL focuses on per-frame state.
    let out_rate = state.output_rate.load(Ordering::Relaxed) as u32;
    let vol = state.volume.load(Ordering::Relaxed);
    let bal = state.balance_value();
    let bal_str = if bal == 0 { "centred".to_string() }
                  else if bal < 0 { format!("L{}%", -bal) }
                  else { format!("R{}%", bal) };
    let fader = if state.is_pre_fader() { "pre" } else { "post" };
    let signal_rows: Vec<(&'static str, String, bool /*good*/)> = vec![
        ("output",  format!("{:.1}k", out_rate as f32 / 1000.0), false),
        ("volume",  format!("{}%", vol), false),
        ("buffer",  format!("{}%", buf_pct), buf_pct >= 60),
        ("fader",   fader.to_string(), false),
        ("balance", bal_str, false),
    ];

    // === Header row: "NOW PLAYING" │ "SIGNAL" ===
    let np_label = if state.is_paused() { "PAUSED" } else { "NOW PLAYING" };
    if two_col {
        let left = format!("{dim}{l}{rst}", dim = p.dim, rst = p.reset, l = np_label);
        let right = format!("{dim}SIGNAL{rst}", dim = p.dim, rst = p.reset);
        w.line(&fmt_two_col(&left, np_label.len(), &right, left_w));
    } else {
        w.line(&format!("  {dim}{l}{rst}", dim = p.dim, rst = p.reset, l = np_label));
    }

    // === Time/bar/total row paired with first SIGNAL row (output) ===
    let progress = if state.total_secs() > 0.0 {
        (state.time_secs() / state.total_secs()).min(1.0)
    } else { 0.0 };
    let cur = format_time(state.time_secs());
    let tot = format_time(state.total_secs());
    let bar_w = left_w.saturating_sub(20).clamp(20, 60);
    let bar = render_progress_bar(progress, bar_w, p.accent, p.rule, p.reset);
    let time_line_plain_len = cur.len() + 3 + bar_w + 3 + tot.len();
    let time_line = format!(
        "{accent}{cur}{rst}   {bar}   {dim}{tot}{rst}",
        accent = p.accent, rst = p.reset, dim = p.dim,
        cur = cur, bar = bar, tot = tot,
    );

    if two_col {
        let (k, v, good) = &signal_rows[0];
        let row = render_signal_row(p, k, v, *good, signal_w);
        let row_plain_len = signal_row_plain_len(k, v, signal_w);
        w.line(&fmt_two_col_padded(&time_line, time_line_plain_len, &row, row_plain_len, left_w));
    } else {
        w.line(&format!("  {}", time_line));
    }

    // === Track meta line paired with second SIGNAL row (volume) ===
    let track_n = state.current_track.load(Ordering::Relaxed) + 1;
    let track_total = state.total_tracks.load(Ordering::Relaxed);
    let src_rate = state.sample_rate.load(Ordering::Relaxed) as u32;
    let bits = state.bits_per_sample.load(Ordering::Relaxed);
    let channels = state.channels.load(Ordering::Relaxed);
    let ch_label = match channels { 1 => "mono", 2 => "stereo", _ => "multi" };
    let rate_label = if src_rate == out_rate {
        format!("{:.1}k", src_rate as f32 / 1000.0)
    } else {
        format!("{:.1}k → {:.1}k", src_rate as f32 / 1000.0, out_rate as f32 / 1000.0)
    };
    let mut meta = format!(
        "track {n} of {tot}  ·  {bits}-bit {ch}  ·  {rate}",
        n = track_n, tot = track_total, bits = bits, ch = ch_label, rate = rate_label,
    );
    let eq_name = &eq_preset.name;
    if eq_name != "Flat" { meta.push_str(&format!("  ·  eq {}", eq_name)); }
    if fx_name != "None" { meta.push_str(&format!("  ·  fx {}", fx_name)); }
    if cf_name != "Off"  { meta.push_str(&format!("  ·  cf {}", cf_name)); }
    if state.is_clipping() {
        meta.push_str(&format!("  ·  {danger}clipping{rst}", danger = p.danger, rst = p.reset));
    }
    if state.show_stats() {
        meta.push_str(&format!("  ·  cpu {:.1}%  ·  mem {:.0}M", stats.cpu_usage, stats.memory_mb));
    }

    let meta_left_w = left_w.saturating_sub(2);
    let meta_visible_len = visible_len_ansi(&meta).min(meta_left_w);
    let meta_styled = if visible_len_ansi(&meta) > meta_left_w {
        format!("{dim}{m}{rst}", dim = p.dim, rst = p.reset, m = truncate_plain_ansi_aware(&meta, meta_left_w))
    } else {
        format!("{dim}{m}{rst}", dim = p.dim, rst = p.reset, m = meta)
    };
    if two_col {
        let (k, v, good) = &signal_rows[1];
        let row = render_signal_row(p, k, v, *good, signal_w);
        let row_plain_len = signal_row_plain_len(k, v, signal_w);
        w.line(&fmt_two_col_padded(&meta_styled, meta_visible_len, &row, row_plain_len, left_w));
    } else {
        w.line(&format!("  {}", meta_styled));
    }

    // === Remaining SIGNAL rows on the right (left side blank) ===
    if two_col {
        for (k, v, good) in signal_rows.iter().skip(2) {
            let row = render_signal_row(p, k, v, *good, signal_w);
            let row_plain_len = signal_row_plain_len(k, v, signal_w);
            w.line(&fmt_two_col_padded("", 0, &row, row_plain_len, left_w));
        }
    }

    // === SPECTRUM section ===
    if viz_mode != VizMode::None {
        w.line("");
        w.line(&format!(
            "  {dim}{label}{rst}",
            dim = p.dim, rst = p.reset, label = viz_section_label(viz_mode),
        ));

        if viz_mode == VizMode::SpectrogramAnalysis {
            // Real analysis spectrogram: a pixel image (Kitty on Ghostty/Kitty,
            // fixed-palette Sixel on Windows Terminal) or a half-block fallback.
            // Mirrors the Classic sixel rules exactly (see CLAUDE.md "Terminal
            // Graphics"): image rows must NOT get erase-to-EOL — an EL wipes a
            // Sixel image down to a strip — and the block height is clamped so
            // it can't overflow past the window bottom (auto-scroll storm).
            let log_axis = matches!(viz_style, VizStyle::Dots);
            // Rows above the image = anchor (1) + everything through the label.
            // Minimal's footer below the viz is a blank line + the 3-line
            // command-bar box (4 rows); analysis_rows_for_window reserves 3, so
            // pass +1 to reserve the tallest footer and never overflow.
            let ana_rows = analysis_rows_for_window(term_h, 1 + w.count() + 1);
            let raw = analysis_needs_raw_lines();
            let force = prev_viz_lines == usize::MAX || !block_was_intact;
            ui.spectro_block_intact = true;
            for line in render_spectrogram_analysis(
                analyser, term_w, log_axis, state.is_paused(), ana_rows, force,
            ) {
                if raw {
                    w.line_raw(&line);
                } else {
                    w.line(&line);
                }
            }
        } else {
            // Character-cell viz modes: erase-to-EOL is fine.
            let viz_lines: Vec<String> = match viz_mode {
                VizMode::None => Vec::new(),
                VizMode::VuMeter => render_vu_meter(state, viz_style, term_w),
                VizMode::SpectrumHorizontal => render_spectrum_horizontal(state, viz_style),
                VizMode::SpectrumVertical => render_spectrum_vertical(state, viz_style),
                VizMode::Oscilloscope => render_oscilloscope(analyser, viz_style, term_w),
                VizMode::Lissajous => render_lissajous(analyser, viz_style, term_w),
                VizMode::Spectrogram => render_spectrogram(analyser, viz_style, term_w),
                VizMode::SpectrogramAnalysis => unreachable!("handled above"),
            };
            for line in &viz_lines {
                w.line(line);
            }
        }
    }

    // === Footer command tray (boxed) ===
    w.line("");

    if let Some(msg) = ui.take_status() {
        w.line(&format!(
            "  {accent}{msg}{rst}",
            accent = p.accent, rst = p.reset, msg = msg,
        ));
    } else {
        let inner_w = term_w.saturating_sub(4);
        let h_w = inner_w.saturating_sub(2);
        let bar = slim_cmd_bar_inner(p);
        let bar_visible = visible_len_ansi(&bar);
        let pad = inner_w.saturating_sub(bar_visible + 2);
        w.line(&format!(
            "  {rule}┌{h}┐{rst}",
            rule = p.rule, rst = p.reset, h = "─".repeat(h_w),
        ));
        w.line(&format!(
            "  {rule}│{rst} {bar}{pad} {rule}│{rst}",
            rule = p.rule, rst = p.reset, bar = bar, pad = " ".repeat(pad),
        ));
        w.line(&format!(
            "  {rule}└{h}┘{rst}",
            rule = p.rule, rst = p.reset, h = "─".repeat(h_w),
        ));
    }

    print!("\x1B[J");
    io::stdout().flush().ok();
    w.count()
}


/// Print the Minimal wordmark anchor row (no leading newline, full-line clear).
/// Rendered every frame so it survives terminal scroll when viz pushes content
/// past the bottom row. Uses bold for `K E E T` and dim for the subtitle so
/// the line is always visible (the dim attribute alone can render invisible
/// on some palette/background combos).
fn wordmark_anchor(p: &crate::theme::Palette) -> String {
    format!(
        "  {bold}{fg}K  E  E  T{rst}   {dim}music for terminals{rst}",
        bold = p.bold, fg = p.fg, dim = p.dim, rst = p.reset,
    )
}

/// Print a styled left-column string and a styled right-column string on the
/// same line. Layout: `  {left}{pad} │ {right}` — the `│` is rendered in
/// rule color so it reads as a quiet column divider.
fn fmt_two_col(left: &str, left_visible_len: usize, right: &str, left_w: usize) -> String {
    let p = palette(ThemeKind::Minimal);
    let pad = left_w.saturating_sub(left_visible_len);
    format!(
        "  {left}{pad} {rule}│{rst} {right}",
        left = left,
        pad = " ".repeat(pad),
        rule = p.rule, rst = p.reset,
        right = right,
    )
}

fn fmt_two_col_padded(
    left: &str,
    left_visible_len: usize,
    right: &str,
    _right_visible_len: usize,
    left_w: usize,
) -> String {
    let p = palette(ThemeKind::Minimal);
    let pad = left_w.saturating_sub(left_visible_len.min(left_w));
    format!(
        "  {left}{pad} {rule}│{rst} {right}",
        left = left,
        pad = " ".repeat(pad),
        rule = p.rule, rst = p.reset,
        right = right,
    )
}

/// Render one SIGNAL row: "key" (dim, left) + value (fg or accent, right) padded
/// to `width` total visible chars.
fn render_signal_row(
    p: &crate::theme::Palette,
    key: &str,
    value: &str,
    good: bool,
    width: usize,
) -> String {
    let visible = key.chars().count() + value.chars().count();
    let gap = width.saturating_sub(visible).max(1);
    let value_color = if good { p.accent } else { p.fg };
    format!(
        "{dim}{key}{rst}{gap}{val_color}{value}{rst}",
        dim = p.dim,
        rst = p.reset,
        key = key,
        gap = " ".repeat(gap),
        val_color = value_color,
        value = value,
    )
}

fn signal_row_plain_len(key: &str, value: &str, width: usize) -> usize {
    let visible = key.chars().count() + value.chars().count();
    let gap = width.saturating_sub(visible).max(1);
    key.chars().count() + gap + value.chars().count()
}

fn visible_len_ansi(s: &str) -> usize {
    let mut n = 0usize;
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            if ch.is_ascii_alphabetic() { in_esc = false; }
        } else if ch == '\x1B' {
            in_esc = true;
        } else {
            n += 1;
        }
    }
    n
}

fn truncate_plain_ansi_aware(s: &str, max_width: usize) -> String {
    let visible = visible_len_ansi(s);
    if visible <= max_width { s.to_string() } else { truncate_ansi(s, max_width) }
}

/// Inner content of the player slim cmd bar (no leading margin spaces). The
/// caller wraps it in a box frame.
fn slim_cmd_bar_inner(p: &crate::theme::Palette) -> String {
    let pairs: &[(&str, &str)] = &[
        ("␣", "play"),
        ("←→", "seek"),
        ("↑↓", "track"),
        ("L", "library"),
        ("Y", "lyrics"),
        ("E", "eq"),
        ("T", "theme"),
    ];
    let mut s = String::with_capacity(160);
    for (i, (k, label)) in pairs.iter().enumerate() {
        if i > 0 {
            s.push_str("   ");
        }
        s.push_str(p.fg);
        s.push_str(k);
        s.push_str(p.reset);
        s.push(' ');
        s.push_str(p.dim);
        s.push_str(label);
        s.push_str(p.reset);
    }
    s
}

fn viz_section_label(mode: VizMode) -> &'static str {
    match mode {
        VizMode::None => "",
        VizMode::VuMeter => "VU",
        VizMode::SpectrumHorizontal => "SPECTRUM",
        VizMode::SpectrumVertical => "SPECTRUM",
        VizMode::Oscilloscope => "OSCILLOSCOPE",
        VizMode::Lissajous => "VECTOR",
        VizMode::Spectrogram | VizMode::SpectrogramAnalysis => "SPECTROGRAM",
    }
}

fn format_time(secs: f64) -> String {
    let m = (secs / 60.0) as u32;
    let s = (secs % 60.0) as u32;
    format!("{:02}:{:02}", m, s)
}

/// Editorial progress bar: solid accent fill + 1/8 partial + dotted rule rail.
fn render_progress_bar(progress: f64, width: usize, fg: &str, rule: &str, reset: &str) -> String {
    const PARTIALS: &[char] = &['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let sub = (progress * width as f64).max(0.0);
    let full = sub as usize;
    let frac = ((sub - full as f64) * 8.0) as usize;
    let mut s = String::with_capacity(width * 4 + fg.len() + rule.len() + reset.len());
    s.push_str(fg);
    for _ in 0..full.min(width) {
        s.push('█');
    }
    if full < width && frac > 0 {
        s.push(PARTIALS[(frac - 1).min(PARTIALS.len() - 1)]);
    }
    s.push_str(rule);
    let tail = width.saturating_sub(full + if frac > 0 { 1 } else { 0 });
    for _ in 0..tail {
        s.push('─');
    }
    s.push_str(reset);
    s
}

fn visible_len(s: &str) -> usize {
    s.chars().count()
}

/// Plain-text truncation with a trailing ellipsis. Matches the helper in
/// `ui.rs`; duplicated here to keep the module standalone.
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

/// Render the Minimal Library (playlist) view.
///
/// Editorial table per variant-b:
/// ```text
///   Library    34 tracks · ~2h 14m              / search   S save
///   ──────────────────────────────────────────────────────────────
///   #     TITLE                          ALBUM            TIME
///   01    Do I Wanna Know?  Arctic Mon…  AM               03:55
/// ▶ 05    I Want It All  Arctic Monkeys  AM               03:05   ← playing (accent)
///   06    No.1 Party Anth…  Arctic Mon…  AM               04:03   ← cursor (bg-tint)
///   ↵ play  ↑↓ nav  / search  A queue  D remove  S save  L close   ? all commands
/// ```
pub fn print_status_minimal_library(
    _state: &PlayerState,
    ui: &mut UiState,
    _name: &str,
    prev_viz_lines: usize,
    playlist: &[PathBuf],
) -> usize {
    let p = palette(ThemeKind::Minimal);
    let term_w = terminal::size().map(|(w, _)| w as usize).unwrap_or(120);
    let term_h = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);

    if prev_viz_lines != usize::MAX && prev_viz_lines > 0 {
        print!("\x1B[{}F", prev_viz_lines);
    }

    // === Anchor (line 1): wordmark — survives terminal scroll ===
    let mut w = crate::ui::FrameWriter::new();
    w.first_line(&wordmark_anchor(p));

    // === Library header row: "Library  N tracks · Hh Mm" + right hints ===
    let total_tracks = playlist.len();
    let total_secs: f64 = (0..total_tracks)
        .filter_map(|i| ui.metadata_cache.duration(i))
        .sum();
    let dur_summary = format_duration_total(total_secs);
    let left_label = format!(
        "{bold}{fg}Library{rst}  {dim}{n} tracks · {dur}{rst}",
        bold = p.bold, fg = p.fg, rst = p.reset, dim = p.dim,
        n = total_tracks, dur = dur_summary,
    );
    let right_hints = format!(
        "{fg}/{rst} {dim}search{rst}   {fg}S{rst} {dim}save{rst}",
        fg = p.fg, rst = p.reset, dim = p.dim,
    );
    let left_visible = visible_len_ansi(&left_label);
    let right_visible = visible_len_ansi(&right_hints);
    let pad = term_w
        .saturating_sub(2 + left_visible + right_visible + 2)
        .max(1);
    w.line(&format!(
        "  {left}{gap}{right}",
        left = left_label,
        gap = " ".repeat(pad),
        right = right_hints,
    ));

    // Top rule
    let rule_w = term_w.saturating_sub(4);
    w.line(&format!(
        "  {rule}{rl}{rst}",
        rule = p.rule, rst = p.reset, rl = "─".repeat(rule_w),
    ));

    // === Column layout: # | TITLE | ALBUM | TIME ===
    let num_w = 4usize;
    let time_w = 5usize;
    let inter_gap = 2usize;
    // Album takes ~22% of remaining space, clamped 14..28.
    let avail = term_w.saturating_sub(2 + num_w + inter_gap + inter_gap + time_w + 2);
    let album_w = (avail * 22 / 100).clamp(0, 28);
    let title_w = avail.saturating_sub(album_w + if album_w > 0 { inter_gap } else { 0 });

    // Column headers (dim small-caps, letter-spaced — we approximate spacing
    // with single spaces since true CSS letter-spacing isn't possible).
    let header_line = format!(
        "  {dim}{num:<num_w$}{gap}{title:<title_w$}{gap}{album:<album_w$}{gap}{time:>time_w$}{rst}",
        dim = p.dim, rst = p.reset,
        num = "#", title = "TITLE", album = "ALBUM", time = "TIME",
        gap = " ".repeat(inter_gap),
        num_w = num_w, title_w = title_w, album_w = album_w.max(1), time_w = time_w,
    );
    w.line(&truncate_ansi(&header_line, term_w));

    // Compute visible rows. Header above the rows: library row + rule + col
    // headers = 3. Footer: top rule + key bar = 2. Anchor (+1) is the
    // wordmark; banner area is now zero-height for Minimal.
    let footer_lines = 2;
    let header_consumed = 3; // library header row + rule + column headers
    let visible_rows = term_h
        .saturating_sub(header_consumed + footer_lines + ui.banner_lines + 1)
        .max(1);
    ui.last_visible_rows = visible_rows;

    if ui.library_tree_mode {
        let lines = crate::ui::render_tree_body(ui, visible_rows, term_w, p);
        let n = lines.len();
        for line in &lines {
            w.line(line);
        }
        for _ in n..visible_rows {
            w.line("");
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
        ui.scroll_offset = ui
            .cursor
            .saturating_sub(visible_rows.saturating_sub(scroll_margin + 1));
    }
    if ui.cursor < ui.scroll_offset + scroll_margin {
        ui.scroll_offset = ui.cursor.saturating_sub(scroll_margin);
    }
    let max_offset = items_len.saturating_sub(visible_rows);
    ui.scroll_offset = ui.scroll_offset.min(max_offset);

    if items_len == 0 && search_active {
        w.line(&format!(
            "  {dim}(no matches){rst}",
            dim = p.dim, rst = p.reset
        ));
        for _ in 1..visible_rows { w.line(""); }
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
                .unwrap_or_else(|| ui.metadata_cache.display_name(track_idx, &playlist[track_idx]));
            let (row_artist_opt, _) = ui.metadata_cache.artist_album(track_idx);
            let row_artist = row_artist_opt.unwrap_or_default();
            let row_album = ui.metadata_cache.album(track_idx).unwrap_or_default();
            let dur_str = match ui.metadata_cache.duration(track_idx) {
                Some(d) => format_time(d),
                None => "  :  ".to_string(),
            };

            let num_cell = if is_playing {
                format!("{:<num_w$}", "▶", num_w = num_w)
            } else {
                format!("{:0>2}{:<rest$}", track_idx + 1, "", rest = num_w.saturating_sub(2))
            };

            // Title cell: title fg + dim artist appended ("Title  Artist").
            let title_full = if !row_artist.is_empty() {
                format!("{}  {}{}{}", row_title, p.dim, row_artist, p.reset)
            } else {
                row_title.clone()
            };
            let title_truncated = truncate_plain_ansi_aware(&title_full, title_w);
            let title_visible = visible_len_ansi(&title_truncated);
            let title_pad = title_w.saturating_sub(title_visible);

            // Album cell: dim, fixed width.
            let album_truncated = truncate_plain(&row_album, album_w);
            let album_pad = album_w.saturating_sub(visible_len(&album_truncated));

            // Time cell: dim, right-aligned, fixed width.
            let time_pad = time_w.saturating_sub(dur_str.chars().count());

            let row_color = if is_playing { p.accent } else { p.fg };
            let num_color = if is_playing { p.accent } else { p.dim };

            // Compose row content (without leading "  " or bg).
            let body = format!(
                "{nc}{num}{rst}{gap}{rc}{title}{rst}{tpad}{gap}{dim}{album}{rst}{apad}{gap}{tpad2}{dim}{time}{rst}",
                nc = num_color, num = num_cell, rst = p.reset,
                gap = " ".repeat(inter_gap),
                rc = row_color, title = title_truncated,
                tpad = " ".repeat(title_pad),
                dim = p.dim,
                album = album_truncated, apad = " ".repeat(if album_w > 0 { album_pad } else { 0 }),
                tpad2 = " ".repeat(time_pad),
                time = dur_str,
            );

            // Cursor row: bg-tint extending full width.
            let line = if is_cursor && !p.cursor_bg.is_empty() {
                let inner_visible = num_w + inter_gap + title_w + inter_gap
                    + album_w + if album_w > 0 { inter_gap } else { 0 } + time_w;
                let trail_pad = term_w.saturating_sub(2 + inner_visible);
                format!(
                    "{bg}  {body}{trail}{rst}",
                    bg = p.cursor_bg, body = body,
                    trail = " ".repeat(trail_pad), rst = p.reset,
                )
            } else {
                format!("  {}", body)
            };
            w.line(&truncate_ansi(&line, term_w));
        }
        for _ in visible_count..visible_rows { w.line(""); }
    }
    }

    // === Footer: rule-bordered cmd tray (3 lines) ===
    let footer_content: String = match &ui.input_mode {
        InputMode::Search(query) => format!(
            "  {accent}/{rst} {q}{dim}_{rst}",
            accent = p.accent, rst = p.reset, dim = p.dim, q = query,
        ),
        InputMode::SavePlaylist(n) => format!(
            "  {dim}save as:{rst} {name}{dim}_{rst}",
            dim = p.dim, rst = p.reset, name = n,
        ),
        InputMode::Normal => {
            if let Some(msg) = ui.take_status() {
                format!("  {accent}{msg}{rst}", accent = p.accent, rst = p.reset, msg = msg)
            } else if ui.library_tree_mode {
                format!(
                    "  {dim}[Tab] list  [←→] fold  [Enter] play  [/] filter  [D] remove  [L] close{rst}",
                    dim = p.dim, rst = p.reset,
                )
            } else {
                library_hint_bar(p, term_w)
            }
        }
    };
    w.line(&format!(
        "  {rule}{rl}{rst}",
        rule = p.rule, rst = p.reset, rl = "─".repeat(rule_w),
    ));
    w.line(&truncate_ansi(&footer_content, term_w));

    print!("\x1B[J");
    io::stdout().flush().ok();
    w.count()
}

fn format_duration_total(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    if h > 0 {
        format!("~{h}h {m:02}m")
    } else {
        format!("{m}m")
    }
}

fn library_hint_bar(p: &crate::theme::Palette, term_w: usize) -> String {
    let pairs: &[(&str, &str)] = &[
        ("↵", "play"),
        ("↑↓", "nav"),
        ("/", "search"),
        ("A", "queue"),
        ("D", "remove"),
        ("S", "save"),
        ("L", "close"),
    ];
    let mut left = String::with_capacity(160);
    for (i, (k, label)) in pairs.iter().enumerate() {
        if i > 0 { left.push_str("   "); }
        left.push_str(p.fg);
        left.push_str(k);
        left.push_str(p.reset);
        left.push(' ');
        left.push_str(p.dim);
        left.push_str(label);
        left.push_str(p.reset);
    }
    let right = format!(
        "{fg}?{rst} {dim}all commands{rst}",
        fg = p.fg, rst = p.reset, dim = p.dim,
    );
    let lvis = visible_len_ansi(&left);
    let rvis = visible_len_ansi(&right);
    let pad = term_w.saturating_sub(2 + lvis + rvis + 2).max(2);
    format!("  {left}{gap}{right}", left = left, gap = " ".repeat(pad), right = right)
}

/// Render the Minimal Lyrics view.
///
/// Editorial body: dim verses, accent bold for the current synced line.
/// ```text
///   Do I Wanna Know?                       ← anchor (bold)
///   Arctic Monkeys · AM                    ← dim
///                                          ← blank
///   LYRICS                                 ← dim small-caps
///                                          ← blank
///       Have you got colour in your cheeks?
///       Do you ever get that fear that you can't shift the tide?
///   ▸   The nights were mainly made for…   ← accent + bold (current)
///       That hung heavy on the crown        ← dim
///                                          ← blank
///   Y close   W/S scroll   A/D sync   offset:+0.0s
/// ```
pub fn print_status_minimal_lyrics(
    state: &PlayerState,
    ui: &mut UiState,
    name: &str,
    prev_viz_lines: usize,
) -> usize {
    let p = palette(ThemeKind::Minimal);
    let term_w = terminal::size().map(|(w, _)| w as usize).unwrap_or(120);
    let term_h = terminal::size().map(|(_, h)| h as usize).unwrap_or(24);

    if prev_viz_lines != usize::MAX && prev_viz_lines > 0 {
        print!("\x1B[{}F", prev_viz_lines);
    }

    // === Anchor (line 1): wordmark — survives terminal scroll ===
    let mut w = crate::ui::FrameWriter::new();
    w.first_line(&wordmark_anchor(p));

    // === Line 2: title bold + dim metadata + accent time on right ===
    let idx = state.current_track.load(Ordering::Relaxed);
    let title = ui
        .metadata_cache
        .title(idx)
        .unwrap_or_else(|| name.to_string());
    let (artist, _album) = ui.metadata_cache.artist_album(idx);
    let is_synced = ui.lyrics.as_ref().map(|l| l.is_synced()).unwrap_or(false);
    let mut meta = String::new();
    if let Some(a) = artist.as_ref() { meta.push_str(a); }
    if ui.lyrics.is_some() {
        if !meta.is_empty() { meta.push_str("  ·  "); }
        meta.push_str(if is_synced { "synced" } else { "plain" });
    }
    let cur_t = format_time(state.time_secs());
    let tot_t = format_time(state.total_secs());
    let time_str = format!("{}  /  {}", cur_t, tot_t);

    // Composition: budget the title to leave room for meta + spacing + time on right.
    let right_visible = time_str.chars().count();
    let title_budget = term_w
        .saturating_sub(2 + meta.chars().count() + 4 + right_visible + 2)
        .max(8);
    let title_truncated = truncate_plain(&title, title_budget);
    let left_str = format!(
        "{bold}{fg}{title}{rst}{gap}{dim}{meta}{rst}",
        bold = p.bold, fg = p.fg, rst = p.reset, dim = p.dim,
        title = title_truncated,
        gap = if meta.is_empty() { "" } else { "  " },
        meta = meta,
    );
    let left_visible = visible_len_ansi(&left_str);
    let right_str = format!("{accent}{t}{rst}", accent = p.accent, rst = p.reset, t = time_str);
    let pad = term_w.saturating_sub(2 + left_visible + right_visible + 2).max(1);
    w.line(&format!(
        "  {left}{gap}{right}",
        left = left_str, gap = " ".repeat(pad), right = right_str,
    ));

    // Top rule
    let rule_w = term_w.saturating_sub(4);
    w.line(&format!(
        "  {rule}{rl}{rst}",
        rule = p.rule, rst = p.reset, rl = "─".repeat(rule_w),
    ));

    // === Body region: vertically padded, centered text with falloff ===
    // Header consumed below anchor (wordmark): title row + rule = 2.
    // Footer: blank + centered key bar = 2.
    let header_consumed = 2;
    let footer_consumed = 2;
    let body_rows = term_h
        .saturating_sub(header_consumed + footer_consumed + ui.banner_lines + 1)
        .max(1);

    w.line("");
    let body_rows = body_rows.saturating_sub(1).max(1);

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

        for row in 0..body_rows {
            let line_idx = ui.lyrics_scroll + row;
            if line_idx < total {
                let text = lyrics.line_text(line_idx);
                let is_current = current_line == Some(line_idx);
                // Center the text horizontally.
                let text_visible = text.chars().count().min(term_w.saturating_sub(4));
                let leading = term_w.saturating_sub(text_visible) / 2;
                // Single-line highlight: current line in bold accent (the
                // warm-cyan accent makes the colour difference do the work,
                // not just the bold weight). All other lines are rendered in
                // the regular fg so the verse stays readable.
                let line = if is_current {
                    format!(
                        "{lead}{bold}{accent}{text}{rst}",
                        lead = " ".repeat(leading),
                        bold = p.bold, accent = p.accent, rst = p.reset,
                        text = truncate_plain(text, term_w.saturating_sub(4)),
                    )
                } else {
                    format!(
                        "{lead}{fg}{text}{rst}",
                        lead = " ".repeat(leading),
                        fg = p.fg, rst = p.reset,
                        text = truncate_plain(text, term_w.saturating_sub(4)),
                    )
                };
                w.line(&truncate_ansi(&line, term_w));
            } else {
                w.line("");
            }
        }
    } else {
        let msg = "(no lyrics available)";
        let leading = term_w.saturating_sub(msg.chars().count()) / 2;
        w.line(&format!(
            "{lead}{dim}{msg}{rst}",
            lead = " ".repeat(leading),
            dim = p.dim, rst = p.reset, msg = msg,
        ));
        for _ in 1..body_rows { w.line(""); }
    }

    // === Footer: blank + centered key bar ===
    w.line("");

    let mut hints: Vec<(String, String)> = vec![
        (format!("{fg}W/S{rst}", fg = p.fg, rst = p.reset),
         format!("{dim}scroll{rst}", dim = p.dim, rst = p.reset)),
    ];
    if is_synced {
        hints.push((
            format!("{fg}A/D{rst}", fg = p.fg, rst = p.reset),
            format!("{dim}sync ±0.5s{rst}", dim = p.dim, rst = p.reset),
        ));
    }
    hints.push((
        format!("{fg}Y{rst}", fg = p.fg, rst = p.reset),
        format!("{dim}back{rst}", dim = p.dim, rst = p.reset),
    ));
    let mut visible_count = 0usize;
    let sep_v = 5usize; // " · " padded
    let mut bar = String::new();
    for (i, (k, v)) in hints.iter().enumerate() {
        if i > 0 {
            bar.push_str(&format!(" {dim}·{rst} ", dim = p.dim, rst = p.reset));
            visible_count += 3;
        }
        bar.push_str(k);
        bar.push(' ');
        bar.push_str(v);
        visible_count += visible_len_ansi(k) + 1 + visible_len_ansi(v);
    }
    if is_synced && ui.lyrics_offset != 0.0 {
        bar.push_str(&format!(
            " {dim}·{rst} {dim}offset {ofs:+.1}s{rst}",
            dim = p.dim, rst = p.reset, ofs = ui.lyrics_offset,
        ));
        visible_count += 3 + 8 + format!("{:+.1}", ui.lyrics_offset).chars().count() + 2;
    }
    let _ = sep_v;
    let leading = term_w.saturating_sub(visible_count.min(term_w)) / 2;
    w.line(&format!(
        "{lead}{bar}",
        lead = " ".repeat(leading),
        bar = bar,
    ));

    print!("\x1B[J");
    io::stdout().flush().ok();
    w.count()
}

/// ANSI-aware truncation: counts only printable characters against `max_width`,
/// preserving any escape sequences that were emitted before the cut point.
fn truncate_ansi(s: &str, max_width: usize) -> String {
    let mut visible = 0usize;
    let mut out = String::with_capacity(s.len());
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            out.push(ch);
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if ch == '\x1B' {
            in_escape = true;
            out.push(ch);
        } else {
            if visible >= max_width {
                break;
            }
            out.push(ch);
            visible += 1;
        }
    }
    out
}
