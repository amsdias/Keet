//! Minimal theme — Player, Library, and Lyrics renderers.
//!
//! Editorial / monochrome layout. Single warm-cyan accent. Matches the
//! `variant-b-screens.jsx` mocks from the design handoff.


use crate::tui::Cols;
use std::sync::atomic::Ordering;

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Padding, Paragraph};
use ratatui::Frame;

use crate::ansi::truncate_plain;
use crate::state::{Hit, ViewMode, VizMode};
use crate::theme::{palette, Palette, ThemeKind};
use crate::tui::{self, LineBuf};
use crate::ui::Ctx;

/// Minimal's renderer for the player, library and lyrics views.
/// Minimal's player view (the playlist and the lyrics are panels over it).
pub fn draw(f: &mut Frame, cx: &mut Ctx) {
    draw_player(f, cx)
}

/// Render the Minimal Now Playing screen.
///
/// Layout. The identity block owns its width outright; the cover and SIGNAL are
/// right-aligned and drawn only when they fit in the space left over, so the
/// progress bar never shifts as the window is resized. The command tray sits
/// ABOVE the visualisation, whose height changes with the mode.
/// ```text
/// K E E T  music for terminals                             ← row 1
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
fn draw_player(f: &mut Frame, cx: &mut Ctx) {
    let p = palette(ThemeKind::Minimal);
    let area = f.area();
    let (term_w, term_h) = (area.width as usize, area.height as usize);
    let state = cx.state;
    let viz_mode = state.viz_mode();

    // Image emit-on-change bookkeeping (mirrors Classic): read whether we
    // owned the spectrogram block last frame, then clear — re-asserted below
    // only if the analysis spectrogram actually renders this frame.
    let block_was_intact = cx.ui.spectro_block_intact;
    cx.ui.spectro_block_intact = false;

    let idx = state.current_track.load(Ordering::Relaxed);
    let title = cx.ui.metadata_cache.title(idx).unwrap_or_else(|| cx.name.to_string());
    let (artist, album) = cx.ui.metadata_cache.artist_album(idx);

    let buf = state.buffer_level.load(Ordering::Relaxed);
    let ring_cap = state.ring_capacity.load(Ordering::Relaxed).max(1);
    cx.stats.update_buf(buf as f32 / ring_cap as f32 * 100.0);
    let buf_pct = cx.stats.smoothed_buf_pct as u32;

    // === Row 1 ===
    // Full window keeps exactly one line of chrome, so it carries the song
    // and transport instead of the wordmark, and everything between it and
    // the visualization is suppressed.
    let fullscreen = state.viz_fullscreen();
    let mut y = 0usize;
    if fullscreen {
        let mut head = LineBuf::new();
        head.push_str("  ").styled(cx.name, p.accent).reset().push_str("  ");
        let room = term_w.saturating_sub(head.width());
        let items: Vec<Line<'static>> = crate::ui::fullscreen_items(cx.track_info, state.time_secs(), state.total_secs())
            .into_iter()
            .map(|s| tui::line(s, p.dim))
            .collect();
        head.append(tui::fit_segments(&items, &tui::line("  •  ", p.dim), room));
        tui::draw_lines(f.buffer_mut(), area, &[head.into_line()]);
        y += 1;
    } else {
        // The wordmark, then a dim rule separating it from the song info.
        let rule = rule_line(p, term_w);
        tui::draw_lines(f.buffer_mut(), area, &[wordmark(p), rule]);
        // The lit tab is the view with the keys (`view_mode` is the player
        // while the player is drawn, under a panel too).
        let focus = cx.ui.focus;
        draw_tabs(f, &mut cx.ui.hits, p, focus);
        y += 2;
    }

    // === Width budget, decided before anything is drawn ===
    // The identity block wins: `ident_w` depends on the terminal alone, so the
    // progress bar and total time hold still while the cover and SIGNAL come
    // and go in the space to their right.
    let cover_size = crate::cover::CoverSize::MINIMAL;
    let (ident_w, show_signal, show_cover, gap) = minimal_layout(term_w, cx.ui.cover_enabled);

    // Build the SIGNAL pairs up front.
    let out_rate = state.output_rate.load(Ordering::Relaxed) as u32;
    let vol = state.volume.load(Ordering::Relaxed);
    let bal = state.balance_value();
    let bal_str = if bal == 0 { "centred".to_string() }
                  else if bal < 0 { format!("L{}%", -bal) }
                  else { format!("R{}%", bal) };
    let fader = if state.is_pre_fader() { "pre" } else { "post" };
    let signal_rows: Vec<(&'static str, String, bool /*good*/)> = vec![
        // Exclusive mode also shows the DAC's format ("96.0k · 24-bit").
        ("output",  match state.output_bits.load(Ordering::Relaxed) {
                        0 => format!("{:.1}k", out_rate as f32 / 1000.0),
                        b => format!("{:.1}k · {b}-bit", out_rate as f32 / 1000.0),
                    }, false),
        ("volume",  format!("{}%", vol), false),
        // Dropouts (cpal xruns) ride on the buffer row once there are any —
        // they are buffer events, and a new row would break the 8-row
        // alignment with the cover. Longest value "100% · 999 xruns" fits.
        ("buffer",  buffer_value(buf_pct, state.xrun_count.load(Ordering::Relaxed)),
                    buf_pct >= 60 && state.xrun_count.load(Ordering::Relaxed) == 0),
        ("fader",   fader.to_string(), false),
        ("balance", bal_str, false),
        // cpu/mem live here permanently rather than on the meta line, where the
        // column was too narrow to fit both and silently dropped `mem`.
        ("cpu",     format!("{:.1}%", cx.stats.cpu_usage), cx.stats.cpu_usage >= 25.0),
        ("mem",     format!("{:.0}M", cx.stats.memory_mb), false),
    ];

    // === Identity block: title → artist → NOW PLAYING → time → meta ===
    let mut ident: Vec<Line<'static>> = Vec::with_capacity(8);
    ident.push(tui::line(truncate_plain(&title, ident_w), p.bold.patch(p.fg)));

    let mut sub = String::new();
    if let Some(a) = artist.as_ref() { sub.push_str(a); }
    if let Some(al) = album.as_ref() {
        if !sub.is_empty() { sub.push_str("  ·  "); }
        sub.push_str(al);
    }
    ident.push(tui::line(truncate_plain(&sub, ident_w), p.dim));

    // Exclusive mode: the signal path verdict, in the row above NOW PLAYING.
    let verdict = crate::signal::verdict(&crate::signal::PathInputs::from_state(state, cx.fx_name, cx.cf_name));
    ident.push(match crate::signal::fitted(&verdict, "  ·  ", ident_w) {
        Some((ok, text)) => tui::line(text, if ok { p.accent } else { p.warn }),
        None => Line::default(),
    });

    ident.push(tui::line(if state.is_paused() { "PAUSED" } else { "NOW PLAYING" }, p.dim));

    let progress = if state.total_secs() > 0.0 {
        (state.time_secs() / state.total_secs()).min(1.0)
    } else { 0.0 };
    let cur = format_time(state.time_secs());
    let tot = format_time(state.total_secs());
    let bar_w = progress_bar_width(ident_w, &cur, &tot);
    // The bar's columns and its row in the identity block, for the mouse.
    let bar_x = 2 + tui::width(&cur) + 3;
    const BAR_ROW: usize = 4;
    let mut row = LineBuf::new();
    row.styled(&cur, p.accent).reset().push_str("   ");
    row.append(render_progress_bar(progress, bar_w, p));
    row.reset().push_str("   ").styled(&tot, p.dim);
    ident.push(row.into_line());

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
    let mut segments = vec![
        format!("track {track_n} of {track_total}"),
        // 0 = a lossy codec: no bit depth.
        if bits > 0 { format!("{bits}-bit {ch_label}") } else { ch_label.to_string() },
        rate_label,
    ];
    // Exclusive mode: the DAC's format, as on Classic's track info line. It
    // lives here, on the always-visible meta line, because the SIGNAL column
    // (whose `output` row also shows it) is only drawn from 100 columns up.
    // Right after the rate, ahead of eq/fx/cf, so it is the last item dropped.
    if let b @ 1.. = state.output_bits.load(Ordering::Relaxed) {
        segments.push(format!("out {b}-bit"));
    }
    if cx.eq_preset.name != "Flat" { segments.push(format!("eq {}", cx.eq_preset.name)); }
    if cx.fx_name != "None" { segments.push(format!("fx {}", cx.fx_name)); }
    if cx.cf_name != "Off"  { segments.push(format!("cf {}", cx.cf_name)); }
    // The clip lamp ends the line, always: it is a status light, not one more
    // item, so no separator before it, and a narrow window drops whole items
    // from the end before it (never the lamp, nor a "·" left dangling).
    // Shape as well as colour: ● clipping, ○ idle (NO_COLOR, colour blindness).
    let (clip_color, clip_glyph) = if state.is_clipping() { (p.danger, "●") } else { (p.good, "○") };
    let text = crate::ansi::fit_segments(&segments, "  ·  ", ident_w.saturating_sub(3));
    let mut meta = LineBuf::new();
    meta.styled(&text, p.dim).reset().push_str("  ").styled(clip_glyph, clip_color);
    ident.push(meta.into_line());

    // Right column: SIGNAL's label sits a row below the block's top so its
    // eight rows (label + six values + mem) finish level with the meta line.
    const SIGNAL_TOP: usize = 1;
    let mut right_col: Vec<Line<'static>> = vec![Line::default(); SIGNAL_TOP];
    right_col.push(tui::line("SIGNAL", p.dim));
    for (k, v, good) in signal_rows.iter() {
        right_col.push(render_signal_row(p, k, v, *good, SIGNAL_W));
    }

    let ident_area = |rows: usize| Rect { x: area.x + 2, y: area.y + y as u16, width: ident_w as u16, height: rows as u16 }.intersection(area);
    if !fullscreen && bar_w > 0 {
        let at = Rect { x: area.x + bar_x as u16, y: area.y + (y + BAR_ROW) as u16, width: bar_w as u16, height: 1 };
        cx.ui.hits.push((at.intersection(area), Hit::Seek));
    }
    if show_signal && !fullscreen {
        let rows = ident.len().max(right_col.len());
        tui::draw_lines(f.buffer_mut(), ident_area(rows), &ident);
        // The cover is vertically aligned with SIGNAL: its top row sits on the
        // SIGNAL label and its bottom on `mem`, so the two right-hand blocks
        // read as one group rather than two staggered ones.
        let mut x = 2 + ident_w + gap;
        if show_cover {
            let slot = Rect {
                x: area.x + x as u16,
                y: area.y + (y + SIGNAL_TOP) as u16,
                width: cover_size.cols as u16,
                height: cover_size.rows as u16,
            }
            .intersection(area);
            let ui = &mut *cx.ui;
            let repaint = cx.repaint || !ui.cover_block_intact || ui.cover_dirty_frame;
            match crate::cover::draw(ui.cover.as_ref(), cover_size, repaint) {
                crate::cover::CoverDraw::Cells(lines) => tui::draw_lines(f.buffer_mut(), slot, &lines),
                crate::cover::CoverDraw::Image(payload) => tui::place_image(tui::ImageSlot::Cover, slot, payload),
            }
            ui.cover_dirty_frame = false;
            x += cover_size.cols as usize + 1;
        }
        cx.ui.cover_block_intact = show_cover;
        let rule_col: Vec<Line<'static>> = (0..rows).map(|_| tui::line("│", p.rule)).collect();
        let rule_area = Rect { x: area.x + x as u16, y: area.y + y as u16, width: 1, height: rows as u16 }.intersection(area);
        tui::draw_lines(f.buffer_mut(), rule_area, &rule_col);
        let right_area = Rect {
            x: area.x + (x + 2) as u16,
            y: area.y + y as u16,
            width: term_w.saturating_sub(x + 2) as u16,
            height: rows as u16,
        }
        .intersection(area);
        tui::draw_lines(f.buffer_mut(), right_area, &right_col);
        y += rows;
    } else if !fullscreen {
        cx.ui.cover_block_intact = false;
        tui::draw_lines(f.buffer_mut(), ident_area(ident.len()), &ident);
        y += ident.len();
    } else {
        cx.ui.cover_block_intact = false;
    }

    // === Command tray ===
    // Above the visualisation, not below it: the viz block changes height with
    // the mode (VU is a few rows, the analysis spectrogram many), which dragged
    // the tray up and down the screen every time it changed.
    if cx.ui.frame_status.is_some() {
        if !fullscreen {
            y += 1;
        }
        crate::ui::draw_status_line(f, cx.ui, tui::rows(area, y, 1), p);
        y += 1;
    } else if !fullscreen {
        y += 1;
        let tray = Rect { x: area.x + 2, y: area.y + y as u16, width: term_w.saturating_sub(4) as u16, height: 3 }.intersection(area);
        let block = Block::bordered().border_style(p.rule).padding(Padding::horizontal(1));
        let keys = slim_cmd_bar_inner(p, term_w.saturating_sub(8));
        // Inside the tray's border and padding.
        keys.register(&mut cx.ui.hits, tray.x + 2, tray.y + 1);
        f.render_widget(Paragraph::new(keys.line).block(block), tray);
        y += 3;
    }

    // === SPECTRUM section ===
    // One budget for every mode: what the frame has spare, capped by the
    // mode's own ceiling. A resize grows the block on its own; full window
    // only lifts the ceiling. One row of slack stays below (an image whose
    // bottom reaches the last row would scroll the screen as the terminal
    // moves the cursor under it).
    let extras = state.viz_extras();
    let viz_on = viz_mode != VizMode::None;
    if fullscreen {
        if viz_on {
            let viz_avail = crate::viz::viz_rows_available(term_h, y, 1);
            let viz_body = crate::viz::viz_body_rows(viz_mode, viz_avail, true, extras);
            let viz_pad = crate::viz::viz_top_pad(viz_avail, viz_body, true);
            crate::ui::draw_viz(f, tui::rows(area, y + viz_pad, viz_body), cx, block_was_intact);
        }
        return;
    }
    // Below a blank row and the section label: the visualization's space,
    // shared with the docked panels (`ui::dock`).
    let label_y = y + 1;
    let top = label_y + 1;
    let region = tui::indent(tui::rows(area, top, crate::viz::viz_rows_available(term_h, top, 1)), 2);
    let dock = crate::ui::dock(cx.ui, viz_on, region);
    if let Some(r) = dock.viz {
        tui::draw_lines(f.buffer_mut(), tui::rows(area, label_y, 1), &[tui::line(format!("  {}", viz_section_label(viz_mode)), p.dim)]);
        let r = Rect { x: area.x, width: r.right() - area.x, ..r };
        let avail = r.height as usize;
        let body = crate::viz::viz_body_rows(viz_mode, avail, false, extras);
        // Centred in the space below the label, as Classic centres it in
        // its slot.
        let pad = crate::viz::viz_top_pad(avail, body, true);
        crate::ui::draw_viz(f, tui::rows(r, pad, body), cx, block_was_intact);
    }
    crate::ui::draw_docked(f, cx, &dock.panels);
}

/// The views as tabs on the wordmark's row, at its right end, the current
/// one lit; each is a click target. Left out when the row is too narrow to
/// hold them clear of the wordmark.
fn draw_tabs(f: &mut Frame, hits: &mut Vec<(Rect, Hit)>, p: &Palette, current: ViewMode) {
    use ratatui::widgets::Tabs;
    const TABS: [(&str, ViewMode); 3] =
        [("player", ViewMode::Player), ("library", ViewMode::Playlist), ("lyrics", ViewMode::Lyrics)];
    const DIVIDER: &str = "  ·  ";
    // Columns, not bytes: `·` is two bytes and one column, and counting
    // bytes put every tab after the first a column or two right of its hit.
    let div_w = tui::width(DIVIDER);
    let width: usize = TABS.iter().map(|(t, _)| tui::width(t)).sum::<usize>() + div_w * (TABS.len() - 1);
    let area = f.area();
    let wordmark_w = wordmark(p).cols();
    if (area.width as usize) < 2 + wordmark_w + 4 + width + 2 {
        return;
    }
    let mut x = area.x + area.width - 2 - width as u16;
    let at = Rect { x, y: area.y, width: width as u16, height: 1 };
    let selected = TABS.iter().position(|(_, v)| *v == current);
    f.render_widget(
        Tabs::new(TABS.map(|(t, _)| t))
            .select(selected)
            .style(p.dim)
            .highlight_style(p.accent)
            .divider(ratatui::text::Span::styled(DIVIDER, p.rule))
            .padding("", ""),
        at,
    );
    for (title, view) in TABS {
        let w = tui::width(title);
        hits.push((Rect { x, y: area.y, width: w as u16, height: 1 }, Hit::Tab(view)));
        x += (w + div_w) as u16;
    }
}

/// The full-width dim rule under a heading row.
fn rule_line(p: &Palette, term_w: usize) -> Line<'static> {
    tui::line(format!("  {}", "─".repeat(term_w.saturating_sub(4))), p.rule)
}

/// The Minimal wordmark row. Bold for `K E E T` and dim for the subtitle so
/// the line is always visible (the dim attribute alone can render invisible
/// on some palette/background combos).
fn wordmark(p: &Palette) -> Line<'static> {
    let mut l = LineBuf::new();
    l.push_str("  ").styled("K  E  E  T", p.bold.patch(p.fg)).reset().push_str("   ").styled("music for terminals", p.dim);
    l.into_line()
}

/// Progress-bar width that leaves room for the timestamps either side.
///
/// Derived from the column actually available: an earlier `clamp(20, 60)`
/// imposed a *minimum* of 20, so the row stayed ~36 columns wide however
/// narrow the column became, and overflowed while the window was dragged in.
fn progress_bar_width(ident_w: usize, cur: &str, tot: &str) -> usize {
    let fixed = tui::width(cur) + 3 + 3 + tui::width(tot);
    ident_w.saturating_sub(fixed).min(60)
}

/// Width of the reserved SIGNAL column.
const SIGNAL_W: usize = 30;

/// The SIGNAL block's buffer value, with the dropout count once there is one.
fn buffer_value(buf_pct: u32, xruns: u64) -> String {
    match xruns {
        0 => format!("{buf_pct}%"),
        1 => format!("{buf_pct}% · 1 xrun"),
        n => format!("{buf_pct}% · {n} xruns"),
    }
}
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

/// One SIGNAL row: "key" (dim, left) + value (fg or accent, right) padded
/// to `width` columns.
fn render_signal_row(p: &Palette, key: &str, value: &str, good: bool, width: usize) -> Line<'static> {
    let gap = width.saturating_sub(tui::width(key) + tui::width(value)).max(1);
    let mut l = LineBuf::new();
    l.styled(key, p.dim).reset().spaces(gap).styled(value, if good { p.accent } else { p.fg });
    l.into_line()
}

/// The player's key bar, inside the command tray, `width` columns at most.
fn slim_cmd_bar_inner(p: &Palette, width: usize) -> crate::hints::Bar {
    const KEYS: &[(&str, &str)] = &[
        ("␣", "play"),
        ("←→", "seek"),
        ("↑↓", "track"),
        ("v", "viz"),
        ("l", "library"),
        ("y", "lyrics"),
        ("e", "eq"),
        ("t", "theme"),
    ];
    crate::hints::bar(KEYS, &crate::ui::key_look(crate::theme::ThemeKind::Minimal, p), width)
}

fn viz_section_label(mode: VizMode) -> &'static str {
    match mode {
        VizMode::None => "",
        VizMode::VuMeter => "VU",
        VizMode::SpectrumHorizontal => "SPECTRUM",
        VizMode::SpectrumVertical => "SPECTRUM",
        VizMode::Oscilloscope => "OSCILLOSCOPE",
        VizMode::Lissajous => "VECTOR",
        VizMode::Polar => "POLAR",
        VizMode::Galaxy => "GALAXY",
        VizMode::Cymatics => "CYMATICS",
        VizMode::Spectrogram | VizMode::SpectrogramAnalysis => "SPECTROGRAM",
    }
}

// The one clock format (h:mm:ss past an hour), shared by every theme.
use crate::ui::format_time;

/// Editorial progress bar: solid accent fill + 1/8 partial + dotted rule rail.
fn render_progress_bar(progress: f64, width: usize, p: &Palette) -> Line<'static> {
    const PARTIALS: &[char] = &['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let sub = (progress * width as f64).max(0.0);
    let full = sub as usize;
    let frac = ((sub - full as f64) * 8.0) as usize;
    let mut fill = "█".repeat(full.min(width));
    if full < width && frac > 0 {
        fill.push(PARTIALS[(frac - 1).min(PARTIALS.len() - 1)]);
    }
    let tail = width.saturating_sub(full + if frac > 0 { 1 } else { 0 });
    let mut l = LineBuf::new();
    l.styled(&fill, p.accent).styled(&"─".repeat(tail), p.rule);
    l.into_line()
}



#[cfg(test)]
mod minimal_tests {
    use super::*;

    #[test]
    fn progress_row_fits_the_column_it_is_given() {
        // bar_w used to clamp to a MINIMUM of 20, so the row stayed ~36 cols
        // wide however narrow the column got — the actual overflow source.
        // The timestamps and their gaps are irreducible (16 cols); below that
        // the block's edge does the cutting. Above it, the row must always fit.
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

    #[test]
    fn the_xrun_count_fits_the_signal_column() {
        assert_eq!(buffer_value(62, 0), "62%");
        assert_eq!(buffer_value(62, 1), "62% · 1 xrun");
        for (pct, n) in [(100u32, 999u64), (100, 99_999)] {
            let v = buffer_value(pct, n);
            let row = render_signal_row(crate::theme::palette(crate::theme::ThemeKind::Minimal), "buffer", &v, false, SIGNAL_W);
            assert!(crate::tui::Cols::cols(&row) <= SIGNAL_W, "{v:?} makes a {}-col row", crate::tui::Cols::cols(&row));
        }
    }
}
