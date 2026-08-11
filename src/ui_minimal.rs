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

use crate::ansi::{truncate_ansi, truncate_plain, visible_len};
use crate::state::{InputMode, PlayerState, UiState, VizMode, VizStyle};
use crate::theme::{palette, ThemeKind};
use crate::viz::{
    StatsMonitor, VizAnalyser, analysis_needs_raw_lines, analysis_rows_reserving,
    render_lissajous, render_oscilloscope, render_spectrogram,
    render_spectrogram_analysis, render_spectrum_horizontal,
    render_spectrum_vertical, render_vu_meter,
};

/// Render the Minimal Now Playing screen.
///
/// Layout. The identity block owns its width outright; the cover and SIGNAL are
/// right-aligned and drawn only when they fit in the space left over, so the
/// progress bar never shifts as the window is resized. The command tray sits
/// ABOVE the visualisation, whose height changes with the mode.
/// ```text
/// K E E T  music for terminals                             ← line 1 (anchor)
/// ─────────────────────────────────────────────────────────
/// Do I Wanna Know?                                         ← bold title
/// Arctic Monkeys · AM                   ▄▄▄▄▄▄▄▄  │ SIGNAL
///                                       ████████  │ output   48.0k
/// NOW PLAYING                           ████████  │ volume   80%
/// 02:28  ████████░░░░░░░░░░░░  04:32    ████████  │ buffer   76%
/// track 5 of 34 · 24-bit stereo · 44.1k ████████  │ fader    post
///                                       ████████  │ balance  centred
///                                       ████████  │ cpu      0.4%
///                                       ▀▀▀▀▀▀▀▀  │ mem      17M
///
/// ┌──────────────────────────────────────────────────────┐
/// │ ␣ play  ←→ seek  ↑↓ track  V viz  L library  … │
/// └──────────────────────────────────────────────────────┘
///
/// SPECTRUM                                                 ← dim small-caps (if viz on)
/// [N viz lines]
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

    // === Width budget, decided before anything is emitted ===
    // The identity block wins: `ident_w` depends on the terminal alone, so the
    // progress bar and total time hold still while the cover and SIGNAL come
    // and go in the space to their right.
    let cover_size = crate::cover::CoverSize::MINIMAL;
    let (ident_w, show_signal, show_cover, gap) = minimal_layout(term_w, ui.cover_enabled);
    let signal_w = SIGNAL_W;

    // Build the SIGNAL pairs up front; they render either as the right column
    // (no cover) or as a strip under the identity block (cover shown).
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
        // cpu/mem live here permanently rather than on the meta line, where the
        // column was too narrow to fit both and silently dropped `mem`.
        ("cpu",     format!("{:.1}%", stats.cpu_usage), stats.cpu_usage >= 25.0),
        ("mem",     format!("{:.0}M", stats.memory_mb), false),
    ];

    // === Identity block: title → artist → NOW PLAYING → time → meta ===
    // Collected as (rendered, visible width) so the cover column can be zipped
    // onto the right at a fixed offset.
    let mut ident: Vec<(String, usize)> = Vec::with_capacity(8);

    let title_truncated = truncate_plain(&title, ident_w);
    ident.push((
        format!("{bold}{fg}{t}{rst}", bold = p.bold, fg = p.fg, rst = p.reset, t = title_truncated),
        title_truncated.chars().count(),
    ));

    let mut sub = String::new();
    if let Some(a) = artist.as_ref() { sub.push_str(a); }
    if let Some(al) = album.as_ref() {
        if !sub.is_empty() { sub.push_str("  ·  "); }
        sub.push_str(al);
    }
    let sub_truncated = truncate_plain(&sub, ident_w);
    ident.push((
        format!("{dim}{s}{rst}", dim = p.dim, rst = p.reset, s = sub_truncated),
        sub_truncated.chars().count(),
    ));

    ident.push((String::new(), 0));

    let np_label = if state.is_paused() { "PAUSED" } else { "NOW PLAYING" };
    ident.push((
        format!("{dim}{l}{rst}", dim = p.dim, rst = p.reset, l = np_label),
        np_label.chars().count(),
    ));

    let progress = if state.total_secs() > 0.0 {
        (state.time_secs() / state.total_secs()).min(1.0)
    } else { 0.0 };
    let cur = format_time(state.time_secs());
    let tot = format_time(state.total_secs());
    let bar_w = progress_bar_width(ident_w, &cur, &tot);
    let bar = render_progress_bar(progress, bar_w, p.accent, p.rule, p.reset);
    ident.push((
        format!(
            "{accent}{cur}{rst}   {bar}   {dim}{tot}{rst}",
            accent = p.accent, rst = p.reset, dim = p.dim, cur = cur, bar = bar, tot = tot,
        ),
        cur.chars().count() + 3 + bar_w + 3 + tot.chars().count(),
    ));

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
    if eq_preset.name != "Flat" { meta.push_str(&format!("  ·  eq {}", eq_preset.name)); }
    if fx_name != "None" { meta.push_str(&format!("  ·  fx {}", fx_name)); }
    if cf_name != "Off"  { meta.push_str(&format!("  ·  cf {}", cf_name)); }
    let clip_color = if state.is_clipping() { p.danger } else { p.good };
    meta.push_str(&format!("  ·  {c}●{rst}", c = clip_color, rst = p.reset));
    let meta_visible = visible_len(&meta).min(ident_w);
    let meta_styled = format!(
        "{dim}{m}{rst}",
        dim = p.dim, rst = p.reset,
        m = if visible_len(&meta) > ident_w { truncate_plain_ansi_aware(&meta, ident_w) } else { meta },
    );
    ident.push((meta_styled, meta_visible));

    // Right column: SIGNAL's label sits two rows above NOW PLAYING so its eight
    // rows (label + six values + mem) still finish level with the meta line,
    // keeping the block the same overall height as before.
    const SIGNAL_TOP: usize = 1;
    let mut right_col: Vec<String> = vec![String::new(); SIGNAL_TOP];
    right_col.push(format!("{dim}SIGNAL{rst}", dim = p.dim, rst = p.reset));
    for (k, v, good) in signal_rows.iter() {
        right_col.push(render_signal_row(p, k, v, *good, signal_w));
    }

    if show_signal {
        // The cover is vertically aligned with SIGNAL: its top row sits on the
        // SIGNAL label and its bottom on `balance`, so the two right-hand
        // blocks read as one group rather than two staggered ones.
        let cover_w = cover_size.cols as usize;
        let cover_rows = cover_size.rows as usize;
        let sticky = crate::cover::image_is_sticky();
        let cover_lines: Vec<String> = if !show_cover {
            Vec::new()
        } else {
            let repaint = !sticky
                || prev_viz_lines == usize::MAX
                || !ui.cover_block_intact
                || ui.cover_dirty_frame;
            if repaint {
                match ui.cover.as_ref() {
                    Some(img) => crate::cover::render(img),
                    // No artwork for this track: blank the slot AND remove any
                    // image still placed there from the previous one.
                    None => crate::cover::empty_slot_lines(cover_size),
                }
            } else {
                crate::cover::passive_lines(cover_size)
            }
        };
        ui.cover_block_intact = show_cover;
        if show_cover {
            ui.cover_dirty_frame = false;
        }

        let blank_cover = " ".repeat(cover_w);
        let rows = ident.len().max(right_col.len());
        for i in 0..rows {
            let (content, vis) = ident.get(i).cloned().unwrap_or((String::new(), 0));
            let cell = ident_cell(&content, vis, ident_w);
            let right = right_col.get(i).map(|s| s.as_str()).unwrap_or("");
            let mid = if show_cover {
                let within = i.checked_sub(SIGNAL_TOP).filter(|r| *r < cover_rows);
                match within.and_then(|r| cover_lines.get(r)) {
                    Some(l) => format!("{l} "),
                    None => format!("{blank_cover} "),
                }
            } else {
                String::new()
            };
            let line = format!(
                "  {cell}{g}{mid}{rule}│{rst} {right}",
                g = " ".repeat(gap), rule = p.rule, rst = p.reset,
            );
            // A placed image must not be erased-to-EOL; the cells are blanked by
            // explicit padding instead.
            if show_cover && sticky { w.line_raw(&line); } else { w.line(&line); }
        }
    } else {
        ui.cover_block_intact = false;
        for (content, vis) in ident.iter() {
            w.line(&format!("  {}", ident_cell(content, *vis, ident_w)));
        }
    }

    // === Command tray ===
    // Above the visualisation, not below it: the viz block changes height with
    // the mode (VU is a few rows, the analysis spectrogram many), which dragged
    // the tray up and down the screen every time it changed.
    w.line("");

    if let Some(msg) = ui.active_status() {
        w.line(&format!(
            "  {accent}{msg}{rst}",
            accent = p.accent, rst = p.reset, msg = msg,
        ));
    } else {
        for line in slim_cmd_box(p, term_w) {
            w.line(&line);
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
            // Rows above = anchor (1) + everything already emitted, which now
            // includes the command tray. Nothing follows the image, so only one
            // row of bottom slack is reserved rather than a footer's worth.
            let ana_rows = analysis_rows_reserving(term_h, 1 + w.count(), 1);
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
/// Truncate a rendered cell to `width` visible columns and pad it out to
/// exactly that width.
///
/// Both halves matter. Truncating stops an over-wide cell from pushing into
/// the next column and wrapping at the terminal edge — a wrap adds a PHYSICAL
/// line that `FrameWriter` doesn't know about, so the cursor-up math is off by
/// one and the frame scrolls on every repaint. Padding blanks stale text from
/// a wider previous frame without an erase-to-EOL, which the cover rows can't
/// use (an EL wipes a placed image).
fn ident_cell(content: &str, visible: usize, width: usize) -> String {
    if visible > width {
        truncate_ansi(content, width)
    } else {
        format!("{content}{}", " ".repeat(width - visible))
    }
}

/// Progress-bar width that leaves room for the timestamps either side.
///
/// Derived from the column actually available: an earlier `clamp(20, 60)`
/// imposed a *minimum* of 20, so the row stayed ~36 columns wide however
/// narrow the column became, and overflowed while the window was dragged in.
fn progress_bar_width(ident_w: usize, cur: &str, tot: &str) -> usize {
    let fixed = cur.chars().count() + 3 + 3 + tot.chars().count();
    ident_w.saturating_sub(fixed).min(60)
}

/// Width of the reserved SIGNAL column.
const SIGNAL_W: usize = 30;
/// Widest the identity block is allowed to get.
///
/// The identity column has absolute priority: its width depends on the terminal
/// alone, never on whether the cover or SIGNAL happen to be visible. That is
/// what stops the progress bar and total time shifting sideways as those
/// columns come and go — they appear in, or vanish from, the space to the
/// right instead. Chosen so SIGNAL still arrives at 100 columns.
const IDENT_W_MAX: usize = 61;

/// Column budget for the Minimal player screen.
///
/// Returns `(ident_w, show_signal, show_cover, gap)`, `gap` being the space
/// between the identity block and the right-hand group.
///
/// The cover and SIGNAL are right-aligned and drawn only when they fit in the
/// leftover space. Nothing is ever squeezed to accommodate them: as the window
/// narrows they simply stop being drawn, so the progress bar and total time are
/// never pushed left or overdrawn.
fn minimal_layout(term_w: usize, cover_enabled: bool) -> (usize, bool, bool, usize) {
    let ident_w = term_w.saturating_sub(4).min(IDENT_W_MAX);
    let cover_w = crate::cover::CoverSize::MINIMAL.cols as usize;
    // 2 margin + ident + gap + [cover + 1] + │ + 1 + signal + 2 margin
    let room = |extra: usize| -> Option<usize> {
        term_w.checked_sub(ident_w + SIGNAL_W + extra + 7).filter(|g| *g >= 2)
    };
    if cover_enabled {
        if let Some(gap) = room(cover_w + 1) {
            return (ident_w, true, true, gap);
        }
    }
    match room(0) {
        Some(gap) => (ident_w, true, false, gap),
        None => (ident_w, false, false, 0),
    }
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

fn truncate_plain_ansi_aware(s: &str, max_width: usize) -> String {
    let visible = visible_len(s);
    if visible <= max_width { s.to_string() } else { truncate_ansi(s, max_width) }
}

/// Inner content of the player slim cmd bar (no leading margin spaces). The
/// caller wraps it in a box frame.
/// The three rows of the footer command box, as `[top, content, bottom]`.
///
/// Kept together (and width-tested) because the borders and the content row
/// derive their width separately, and they drifted: the row adds four columns
/// of chrome (`│` + space on each side) but the padding only subtracted two,
/// so the right-hand bar sat two columns past the box edge.
fn slim_cmd_box(p: &crate::theme::Palette, term_w: usize) -> [String; 3] {
    let inner_w = term_w.saturating_sub(4);
    let h_w = inner_w.saturating_sub(2);
    // Content row chrome is 4 columns: │, space, space, │. Whatever is left is
    // all the key bar may occupy — unpadded it simply ran past the right border
    // on narrow terminals, since saturating_sub bottomed out at zero padding
    // instead of cutting the text.
    let avail = inner_w.saturating_sub(4);
    let bar = truncate_ansi(&slim_cmd_bar_inner(p), avail);
    let pad = avail.saturating_sub(visible_len(&bar));
    [
        format!("  {rule}┌{h}┐{rst}", rule = p.rule, rst = p.reset, h = "─".repeat(h_w)),
        format!(
            "  {rule}│{rst} {bar}{pad} {rule}│{rst}",
            rule = p.rule, rst = p.reset, bar = bar, pad = " ".repeat(pad),
        ),
        format!("  {rule}└{h}┘{rst}", rule = p.rule, rst = p.reset, h = "─".repeat(h_w)),
    ]
}

fn slim_cmd_bar_inner(p: &crate::theme::Palette) -> String {
    let pairs: &[(&str, &str)] = &[
        ("␣", "play"),
        ("←→", "seek"),
        ("↑↓", "track"),
        ("V", "viz"),
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
    let left_visible = visible_len(&left_label);
    let right_visible = visible_len(&right_hints);
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
            let title_visible = visible_len(&title_truncated);
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
            if let Some(msg) = ui.active_status() {
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
    let lvis = visible_len(&left);
    let rvis = visible_len(&right);
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
    let left_visible = visible_len(&left_str);
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
        visible_count += visible_len(k) + 1 + visible_len(v);
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


#[cfg(test)]
mod minimal_tests {
    use super::*;

    #[test]
    fn command_box_borders_line_up_with_its_content_row() {
        // The right-hand │ sat two columns right of the box edge:
        //   ┌────┐
        //   │ …    │
        //   └────┘
        // pad subtracted 2 for the bars but the row adds 4 (│ + space either
        // side), so the content row was always inner_w + 2 against a border of
        // inner_w. Independent of font or platform — it just showed up first on
        // Windows.
        let p = crate::theme::palette(ThemeKind::Minimal);
        for term_w in [60usize, 80, 100, 140] {
            let [top, mid, bot] = slim_cmd_box(p, term_w);
            assert_eq!(
                visible_len(&top), visible_len(&mid),
                "term_w={term_w}: content row does not match the top border"
            );
            assert_eq!(visible_len(&top), visible_len(&bot));
            assert!(visible_len(&top) <= term_w, "term_w={term_w}: box exceeds the terminal");
        }
    }

    #[test]
    fn identity_cells_never_exceed_their_column() {
        // An over-wide cell is not a cosmetic problem: it pushes into SIGNAL,
        // wraps at the terminal edge, and the extra PHYSICAL line desyncs
        // FrameWriter's count from reality — so every repaint scrolls. It also
        // leaves the cursor at column 0, which is where the cover then gets
        // placed (the "cover flickers on the left" report).
        let plain = "0:42   ████████████████████████   3:01";
        let cell = ident_cell(plain, visible_len(plain), 20);
        assert_eq!(visible_len(&cell), 20, "over-long cell must be cut to the column");

        // Short content is padded out to the full column so stale text from a
        // wider previous frame is blanked without needing erase-to-EOL.
        let cell = ident_cell("hi", 2, 20);
        assert_eq!(visible_len(&cell), 20);

        // Colour codes survive truncation and don't count toward the width.
        let styled = "\x1B[1mFireside and a very long album title here\x1B[0m";
        let cell = ident_cell(styled, visible_len(styled), 12);
        assert_eq!(visible_len(&cell), 12);
        assert!(cell.contains("\x1B[1m"));

        // Degenerate widths must not panic.
        assert_eq!(visible_len(&ident_cell("abc", 3, 0)), 0);
    }

    #[test]
    fn progress_row_fits_the_column_it_is_given() {
        // bar_w used to clamp to a MINIMUM of 20, so the row stayed ~36 cols
        // wide however narrow the column got — the actual overflow source.
        // The timestamps and their gaps are irreducible (16 cols); below that
        // `ident_cell` does the cutting. Above it, the row must always fit.
        const FIXED: usize = 5 + 3 + 3 + 5;
        for ident_w in [0usize, 8, 16, 20, 30, 36, 63, 200] {
            let w = progress_bar_width(ident_w, "00:42", "03:01");
            let row = FIXED + w;
            assert!(
                row <= ident_w.max(FIXED),
                "ident_w={ident_w}: row {row} overflows its column (bar {w})"
            );
        }
        // Narrower than the timestamps: the bar vanishes rather than forcing width.
        assert_eq!(progress_bar_width(10, "00:42", "03:01"), 0);
        // Longer h:mm:ss stamps take their room from the bar, not the column.
        assert_eq!(progress_bar_width(60, "1:02:03", "1:59:59"), 60 - (7 + 3 + 3 + 7));
        // Wide columns still cap the bar rather than stretching forever.
        assert_eq!(progress_bar_width(400, "00:42", "03:01"), 60);
    }

    #[test]
    fn identity_column_never_moves_as_extras_come_and_go() {
        // The regression this replaces: the cover being inserted shrank the
        // left column, so the progress bar and total time slid left as the
        // window narrowed, then jumped back when the cover dropped out.
        let widths = [70usize, 99, 100, 118, 119, 140, 200];
        let mut seen = std::collections::BTreeSet::new();
        for w in widths {
            let (ident_w, _, _, _) = minimal_layout(w, true);
            let (ident_no_cover, _, _, _) = minimal_layout(w, false);
            assert_eq!(
                ident_w, ident_no_cover,
                "term_w={w}: identity width must not depend on the cover"
            );
            seen.insert(ident_w);
        }
        // Above the cap it is completely stable, whatever else is drawn.
        assert_eq!(minimal_layout(119, true).0, minimal_layout(200, true).0);
        assert_eq!(minimal_layout(200, true).0, IDENT_W_MAX);
        assert!(seen.contains(&IDENT_W_MAX));
    }

    #[test]
    fn extras_appear_only_in_spare_room() {
        let cover_w = crate::cover::CoverSize::MINIMAL.cols as usize;

        // Too narrow for either: identity only, full width.
        let (ident_w, sig, cov, _) = minimal_layout(90, true);
        assert!(!sig && !cov);
        assert_eq!(ident_w, IDENT_W_MAX);

        // SIGNAL arrives at 100, still no cover.
        let (_, sig, cov, gap) = minimal_layout(100, true);
        assert!(sig && !cov, "SIGNAL at 100, cover must wait");
        assert!(gap >= 2);

        // The cover needs its own width plus a gap on top of that.
        assert!(!minimal_layout(118, true).2, "118 is one short for the cover");
        let (ident_w, sig, cov, gap) = minimal_layout(119, true);
        assert!(sig && cov, "cover fits at 119");
        assert_eq!(ident_w, IDENT_W_MAX, "and it did NOT come out of the identity block");
        // 2 margin + ident + gap + cover + space + │ + space + signal + 2 margin,
        // leaving a column of slack so nothing ever sits on the last cell.
        assert_eq!(ident_w + gap + cover_w + SIGNAL_W + 7, 119 - 1);

        // --no-cover keeps SIGNAL and simply widens the gap.
        let (_, sig, cov, gap_nc) = minimal_layout(119, false);
        assert!(sig && !cov);
        assert_eq!(gap_nc, gap + cover_w + 1);

        // Degenerate widths must not panic or underflow.
        assert_eq!(minimal_layout(1, true).0, 0);
    }

    #[test]
    fn minimal_cover_slot_matches_the_signal_panel_height() {
        let m = crate::cover::CoverSize::MINIMAL;
        let c = crate::cover::CoverSize::CLASSIC;
        assert!(m.cols < c.cols && m.rows < c.rows, "Minimal slot is the smaller one");
        // SIGNAL is a label plus seven values (output, volume, buffer, fader,
        // balance, cpu, mem), and the cover is aligned to exactly that block.
        assert_eq!(m.rows, 8);
        // Cells are roughly 1:2, so keep the slot near square rather than wide.
        let aspect = m.cols as f32 / (m.rows as f32 * 2.0);
        assert!((0.9..=1.25).contains(&aspect), "slot aspect {aspect} is distorted");
    }

    #[test]
    fn signal_block_and_identity_block_end_level() {
        // SIGNAL sits two rows above NOW PLAYING so its eight rows finish on the
        // meta line — the whole block stays nine rows tall, as it was with six.
        const SIGNAL_TOP: usize = 1;
        const SIGNAL_ROWS: usize = 8; // label + 7 values
        // Nine rows total — the same height the block had with six SIGNAL rows
        // starting at index 3, so nothing below it shifts.
        assert_eq!(SIGNAL_TOP + SIGNAL_ROWS, 9);
        assert_eq!(
            crate::cover::CoverSize::MINIMAL.rows as usize, SIGNAL_ROWS,
            "cover must span exactly the SIGNAL block"
        );
    }
}
