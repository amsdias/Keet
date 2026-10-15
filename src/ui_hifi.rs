//! Hi-Fi theme — Now Playing renderer.
//!
//! Amber-CRT studio-monitor aesthetic. Matches `variant-c-screens.jsx`:
//! double-line header strip + transport row with segmented time box +
//! single-bordered VU panel with dB scale + knob rack + dot-marquee key bar.
//! The boxes are ratatui blocks; the widest a screen grows is
//! `HIFI_MAX_INNER` columns inside its borders, so it stays poster-like.


use crate::tui::Cols;
use std::sync::atomic::Ordering;

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Padding, Paragraph};
use ratatui::Frame;

use crate::state::Hit;
use crate::theme::{palette, Palette, ThemeKind};
use crate::tui::{self, LineBuf};
use crate::ui::Ctx;

/// HiFi's renderer for the player, library and lyrics views.
/// HiFi's player view (the playlist and the lyrics are panels over it).
pub fn draw(f: &mut Frame, cx: &mut Ctx) {
    draw_player(f, cx)
}

/// Narrowest box interior HiFi lays out for (a 34-column window). Below that
/// the window edge cuts it.
const HIFI_MIN_INNER: usize = 30;
/// Widest box interior: the design stays poster-like on very wide windows.
const HIFI_MAX_INNER: usize = 110;

/// Columns inside the boxes for a window `term_w` wide.
fn inner_width(term_w: usize) -> usize {
    term_w.saturating_sub(4).clamp(HIFI_MIN_INNER, HIFI_MAX_INNER)
}

/// A box `inner_w` columns wide inside its borders, `inner_h` rows tall,
/// two columns in from the left and `y` rows down; cut to the window.
fn box_area(area: Rect, y: usize, inner_w: usize, inner_h: usize) -> Rect {
    Rect { x: area.x + 2, y: area.y + y as u16, width: inner_w as u16 + 2, height: inner_h as u16 + 2 }.intersection(area)
}

/// The double-line header strip: `left` at its start, `right` at its end.
fn header_strip(f: &mut Frame, area: Rect, inner_w: usize, p: &Palette, left: Line<'static>, right: Line<'static>) {
    let row_inner_w = inner_w.saturating_sub(2);
    let pad = row_inner_w.saturating_sub(left.cols() + right.cols());
    let mut row = LineBuf::new();
    row.append(left).reset().spaces(pad).append(right);
    let block = Block::bordered().border_type(BorderType::Double).border_style(p.fg).padding(Padding::horizontal(1));
    f.render_widget(Paragraph::new(row.into_line()).block(block), box_area(area, 0, inner_w, 1));
}

fn draw_player(f: &mut Frame, cx: &mut Ctx) {
    let p = palette(ThemeKind::HiFi);
    let area = f.area();
    let term_w = area.width as usize;
    let state = cx.state;

    // Pull metadata
    let idx = state.current_track.load(Ordering::Relaxed);
    let title = cx.ui.metadata_cache.title(idx).unwrap_or_else(|| cx.name.to_string());
    let (artist, album) = cx.ui.metadata_cache.artist_album(idx);

    // Buffer pct (smoothed)
    let buf = state.buffer_level.load(Ordering::Relaxed);
    let ring_cap = state.ring_capacity.load(Ordering::Relaxed).max(1);
    cx.stats.update_buf(buf as f32 / ring_cap as f32 * 100.0);
    let buf_pct = cx.stats.smoothed_buf_pct as u32;

    // Width budget. Keep the cap modest so the design stays poster-like
    // even on very wide terminals; below it the layout follows the window down
    // to HIFI_MIN_INNER.
    let inner_w = inner_width(term_w);

    // === Header strip: K E E T │ ▶ PLAY · SHUF-ON · RPT-ALL  ...  TRACK 05 / 34 ===
    let track_n = state.current_track.load(Ordering::Relaxed) + 1;
    let track_total = state.total_tracks.load(Ordering::Relaxed);
    let shuffle_on = cx.ui.shuffle;
    let repeat_label = repeat_short(state.repeat_mode());
    let play_label = if state.is_paused() { "⏸ PAUSE" } else { "▶ PLAY" };
    let mut left = LineBuf::new();
    left.styled("K E E T", p.accent.patch(p.bold)).reset().push_str("  ").styled("│", p.dim).reset().push_str("  ");
    left.styled(play_label, p.accent).reset().push_str("  ").styled("·", p.dim).reset().push_str("  ");
    // Spelled out, like RPT-OFF/ALL/ONE: brightness alone is colour alone.
    left.styled(if shuffle_on { "SHUF-ON" } else { "SHUF-OFF" }, if shuffle_on { p.fg } else { p.dim });
    left.reset().push_str("  ").styled("·", p.dim).reset().push_str("  ");
    left.styled(repeat_label, if repeat_label != "RPT-OFF" { p.fg } else { p.dim });
    let mut right = LineBuf::new();
    right.styled("TRACK", p.dim).reset().push(' ').styled(&format!("{track_n:02}"), p.fg).reset().push(' ').styled(&format!("/ {track_total}"), p.dim);
    // Narrow window: the track counter goes first, then the left side is
    // cut (by the box), so the row still ends on its border.
    let (left, right) = (left.into_line(), right.into_line());
    let row_inner_w = inner_w.saturating_sub(2);
    let right = if left.cols() + 2 + right.cols() > row_inner_w { Line::default() } else { right };
    header_strip(f, area, inner_w, p, left, right);

    // === Transport row: segmented time box (3 lines) + title/meta/progress on right ===
    let progress = if state.total_secs() > 0.0 {
        (state.time_secs() / state.total_secs()).min(1.0)
    } else { 0.0 };
    let cur = format_time(state.time_secs());
    let tot = format_time(state.total_secs());

    // Segmented time box (single border, per CSS in mock).
    const SEG_W: usize = 9; // inner width including spaces around time
    let seg = Rect { x: area.x + 2, y: area.y + 4, width: SEG_W as u16 + 2, height: 3 }.intersection(area);
    let seg_text = pad_or_truncate(&format!(" {cur} "), SEG_W);
    f.render_widget(
        Paragraph::new(tui::line(seg_text, p.accent.patch(p.bold))).block(Block::bordered().border_style(p.fg)),
        seg,
    );

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
    // 0 = a lossy codec: no bit depth.
    let depth = if bits > 0 { format!("{bits}-BIT ") } else { String::new() };
    let rate_part = if src_rate == out_rate {
        format!("{depth}{:.1}K", src_rate as f32 / 1000.0)
    } else {
        format!("{depth}{:.1}→{:.1}K", src_rate as f32 / 1000.0, out_rate as f32 / 1000.0)
    };
    if !meta.is_empty() { meta.push_str("  ·  "); }
    meta.push_str(&rate_part);

    // Right column width = inner_w - seg_box_total_w - gap
    let seg_total_w = SEG_W + 2; // borders included
    let gap_left = 4; // spacing between seg box and right column
    let right_w = inner_w.saturating_sub(seg_total_w + gap_left).max(8);
    let bar_w = right_w.saturating_sub(tui::width(&tot) + 4).max(2);
    let mut bar_row = LineBuf::new();
    bar_row.append(render_solid_bar(progress, bar_w, p)).reset().push_str("  ").styled(&format!("/ {tot}"), p.dim);
    let mut right_col = vec![
        tui::line(pad_or_truncate(&title_up, right_w), p.accent.patch(p.bold)),
        tui::line(pad_or_truncate(&meta, right_w), p.dim),
        bar_row.into_line(),
    ];
    // Exclusive mode: the signal path verdict under the progress bar, in the
    // spacer row (an empty row otherwise).
    let verdict = crate::signal::verdict(&crate::signal::PathInputs::from_state(state, cx.fx_name, cx.cf_name));
    if let Some((ok, text)) = crate::signal::fitted(&verdict, "  ·  ", right_w) {
        right_col.push(tui::line(text.to_uppercase(), if ok { p.good } else { p.warn }));
    }
    let right_x = 2 + seg_total_w + gap_left;
    let bar = Rect { x: area.x + right_x as u16, y: area.y + 6, width: bar_w as u16, height: 1 };
    cx.ui.hits.push((bar.intersection(area), Hit::Seek));
    let right_area = Rect { x: area.x + right_x as u16, y: area.y + 4, width: term_w.saturating_sub(right_x) as u16, height: 4 }.intersection(area);
    tui::draw_lines(f.buffer_mut(), right_area, &right_col);

    // === VU panel (single border) ===
    let (lp, rp) = state.get_peaks();
    let (lp_dot, rp_dot) = state.get_vu_dots();
    let meter_w = inner_w.saturating_sub(8).max(8);
    let l_pad = inner_w.saturating_sub(meter_w + 4);
    let channel = |label: &str, bar: Line<'static>| {
        let mut l = LineBuf::new();
        l.push(' ').styled(label, p.dim).reset().push(' ').append(bar).reset().spaces(l_pad + 1);
        l.into_line()
    };
    let mut scale = LineBuf::new();
    scale.push_str("   ").append(render_db_scale(meter_w, p));
    let vu = [
        vu_label_row(inner_w, state.is_clipping(), p),
        channel("L", vu_bar(lp, lp_dot, meter_w, p)),
        channel("R", vu_bar(rp, rp_dot, meter_w, p)),
        scale.into_line(),
    ];
    let vu_area = box_area(area, 8, inner_w, 4);
    // HiFi's visualization is this panel (`viz_on_screen` asks where it was).
    if !vu_area.is_empty() {
        cx.ui.viz_drawn_at = Some(vu_area);
    }
    f.render_widget(Paragraph::new(vu.to_vec()).block(Block::bordered().border_style(p.fg)), vu_area);

    // === Knob rack: a bordered cell per setting ===
    let vol = state.volume.load(Ordering::Relaxed);
    let bal_val = state.balance_value();
    let bal_str = if bal_val == 0 {
        "C".to_string()
    } else if bal_val < 0 {
        format!("L{}", -bal_val)
    } else {
        format!("R{}", bal_val)
    };
    let knobs: Vec<(&str, String, &str, bool)> = vec![
        ("VOL",   format!("{}", vol),                   "%", false),
        ("EQ",    cx.eq_preset.name.to_uppercase(),     "",  false),
        ("FX",    cx.fx_name.to_uppercase(),            "",  false),
        ("XFEED", cx.cf_name.to_uppercase(),            "",  false),
        ("BAL",   bal_str,                              "",  false),
        ("BUF",   format!("{}", buf_pct),               "%", buf_pct >= 60),
    ];
    // As many knobs as the width holds (minimum cell 8 + 1 gap), in this
    // order, rather than one cut at the window edge.
    let fit = ((inner_w + 1) / 9).clamp(1, knobs.len());
    let knobs = &knobs[..fit];
    let gap_w = 1usize;
    let cell_w = (inner_w.saturating_sub(gap_w * (knobs.len() - 1)) / knobs.len()).clamp(8, 14);
    let total_used = cell_w * knobs.len() + gap_w * (knobs.len() - 1);
    let lead = 2 + (inner_w.saturating_sub(total_used) / 2);
    let cell_inner = cell_w.saturating_sub(2);
    for (i, (label, value, unit, good)) in knobs.iter().enumerate() {
        let x = lead + i * (cell_w + gap_w);
        let knob = Rect { x: area.x + x as u16, y: area.y + 15, width: cell_w as u16, height: 4 }.intersection(area);
        // Label: dim, centred.
        let lw = tui::width(label);
        let mut label_row = LineBuf::new();
        label_row.spaces(cell_inner.saturating_sub(lw) / 2).styled(label, p.dim);
        // Value: bold, accent (or good), with an optional dim unit. A long
        // (custom) preset name is cut to the cell.
        let unit_w = if unit.is_empty() { 0 } else { 1 + tui::width(unit) };
        let value = crate::ansi::truncate_plain(value, cell_inner.saturating_sub(unit_w));
        let val_w = tui::width(&value) + unit_w;
        let mut value_row = LineBuf::new();
        value_row.spaces(cell_inner.saturating_sub(val_w) / 2).styled(&value, p.bold.patch(if *good { p.good } else { p.accent }));
        if !unit.is_empty() {
            value_row.reset().push(' ').styled(unit, p.dim);
        }
        let rows = vec![label_row.into_line(), value_row.into_line()];
        f.render_widget(Paragraph::new(rows).block(Block::bordered().border_style(p.fg)), knob);
    }

    // === Marquee key bar ===
    if !crate::ui::draw_status_line(f, cx.ui, tui::rows(area, 20, 1), p) {
        let keys = hifi_marquee_keys(p, term_w);
        keys.register(&mut cx.ui.hits, area.x, area.y + 20);
        tui::draw_lines(f.buffer_mut(), tui::rows(area, 20, 1), &[keys.line]);
    }

    // Docked panels go below the key bar, when the window has the rows.
    if let Some(region) = dock_region(area) {
        let dock = crate::ui::dock(cx.ui, false, region);
        crate::ui::draw_docked(f, cx, &dock.panels);
    }
}

/// Rows below the key bar a panel needs before it can dock there.
pub const DOCK_ROWS: u16 = 10;

/// Where docked panels go in a window `area`: the rows below the key bar,
/// as wide as the boxes above; None when there are fewer than `DOCK_ROWS`.
pub fn dock_region(area: Rect) -> Option<Rect> {
    const TOP: u16 = 21;
    let rows = area.height.checked_sub(TOP)?;
    (rows >= DOCK_ROWS).then(|| {
        let w = inner_width(area.width as usize) as u16 + 2;
        Rect { x: area.x + 2, y: area.y + TOP, width: w.min(area.width.saturating_sub(2)), height: rows }
    })
}

fn repeat_short(mode: crate::state::RepeatMode) -> &'static str {
    use crate::state::RepeatMode::*;
    match mode {
        Off => "RPT-OFF",
        One => "RPT-ONE",
        All => "RPT-ALL",
    }
}

fn render_db_scale(meter_w: usize, p: &Palette) -> Line<'static> {
    let labels: &[(&str, f32, bool /*red*/)] = &[
        ("-∞", 0.00, false),
        ("-30", 0.20, false),
        ("-20", 0.40, false),
        ("-12", 0.60, false),
        ("-6",  0.75, false),
        ("-3",  0.85, false),
        ("0dB", 1.00, true),
    ];
    let mut out = LineBuf::new();
    let mut placed: usize = 0;
    for (i, (label, frac, red)) in labels.iter().enumerate() {
        let target = (frac * meter_w as f32) as usize;
        let lw = tui::width(label);
        // For first label, place at column 0; for last, anchor flush to meter_w end.
        let last = i == labels.len() - 1;
        let col = if last { meter_w.saturating_sub(lw) } else { target };
        // A label that would run into the next one, or past the meter (a
        // narrow window), is left out; the end labels always stay.
        let room_after = if last { 0 } else { tui::width(labels[labels.len() - 1].0) + 1 };
        let gap = usize::from(placed > 0);
        if !last && (col < placed + gap || col + lw + room_after > meter_w) {
            continue;
        }
        if col > placed {
            out.reset().spaces(col - placed);
            placed = col;
        }
        out.styled(label, if *red { p.danger } else { p.dim });
        placed += lw;
    }
    out.into_line()
}

fn hifi_marquee_keys(p: &Palette, term_w: usize) -> crate::hints::Bar {
    // Variant-c key bar. Deliberately NO "V VIZ": this theme's visualisation is
    // the fixed VU panel above, so V changed nothing visible. `V` is a no-op
    // while Hi-Fi is active (see the key handler in ui.rs); the panel is fed
    // whatever the setting (`state.shown_viz`).
    const KEYS: &[(&str, &str)] = &[
        ("␣",   "PLAY"),
        ("←→",  "SEEK"),
        ("↑↓",  "TRACK"),
        ("+/−", "VOL"),
        ("e",   "EQ"),
        ("x",   "FX"),
        ("c",   "XFEED"),
        ("l",   "LIB"),
        ("y",   "LYRICS"),
        ("t",   "THEME"),
    ];
    // As many keys as the window holds: a key cut in half reads as noise.
    crate::hints::indent(crate::hints::bar(KEYS, &crate::ui::key_look(crate::theme::ThemeKind::HiFi, p), term_w.saturating_sub(2)), 2)
}

/// Inner content of the VU panel label row: "VU METER" on the left, a
/// hardware-style CLIP lamp right-aligned (good-colored idle, danger on clip).
/// Spans exactly `inner_w` columns, the box's interior.
fn vu_label_row(inner_w: usize, clipping: bool, p: &Palette) -> Line<'static> {
    let label = "VU METER";
    // Shape as well as colour: ● clipping, ○ idle (NO_COLOR, colour blindness).
    let (lamp, glyph) = if clipping { (p.danger, "●") } else { (p.good, "○") };
    // 1 lead space + label + pad + "CLIP" + space + dot + 1 trail space.
    let pad = inner_w.saturating_sub(label.len() + 8);
    let mut l = LineBuf::new();
    l.push(' ').styled(label, p.dim).reset().spaces(pad).styled("CLIP", p.dim).reset().push(' ').styled(glyph, lamp).reset().push(' ');
    l.into_line()
}

/// VU bar without channel label (label is rendered outside the bar by the
/// caller). 0.85+ region renders in danger color, peak-hold dot in fg/danger.
fn vu_bar(peak: f32, peak_dot: f32, width: usize, p: &Palette) -> Line<'static> {
    let bar_w = width.max(8);
    let seg = (peak.clamp(0.0, 1.0) * bar_w as f32) as usize;
    let dot = (peak_dot.clamp(0.0, 1.0) * bar_w as f32) as usize;
    let hot_threshold = (bar_w as f32 * 0.85) as usize;

    let mut bar = LineBuf::new();
    for i in 0..bar_w {
        let in_hot = i >= hot_threshold;
        if i < seg {
            bar.style(if in_hot { p.danger } else { p.accent }).push('█');
        } else if i == dot && dot >= seg {
            bar.style(if in_hot { p.danger } else { p.fg }).push('█');
        } else {
            bar.style(p.rule).push('·');
        }
    }
    bar.into_line()
}

/// The dim amber of the progress rail.
const RAIL: Color = Color::Rgb(122, 94, 58);

/// Two-tone bar: bright accent fill meeting a solid dim-amber rail. The
/// rail's colour is used both as its own foreground AND as the boundary cell's
/// background, so the bright fill and dim rail always meet seamlessly — a
/// left-partial block only inks its left `frac/8`, so its empty right must be
/// painted the rail colour or the terminal background (black) shows through
/// as a notch that oscillates as the cell fills.
fn render_solid_bar(progress: f64, width: usize, p: &Palette) -> Line<'static> {
    const PARTIALS: &[char] = &['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let sub = (progress * width as f64).max(0.0);
    let full = sub as usize;
    let frac = ((sub - full as f64) * 8.0) as usize;
    let has_partial = full < width && frac > 0;
    let mut s = LineBuf::new();
    // Bright fill.
    s.styled(&"█".repeat(full.min(width)), p.accent);
    // Boundary cell: bright fill on the left, the rail on its right.
    if has_partial {
        s.style(p.accent.bg(RAIL)).push(PARTIALS[(frac - 1).min(PARTIALS.len() - 1)]);
    }
    // Dim rail: solid blocks in the rail colour (shrinks as the fill grows).
    // Without colour a solid rail is indistinguishable from the fill: a line.
    let tail = width.saturating_sub(full + if has_partial { 1 } else { 0 });
    let rail = if crate::term::no_color() { '─' } else { '█' };
    s.style(Style::new().fg(RAIL)).push_str(&rail.to_string().repeat(tail));
    s.into_line()
}

// The one clock format (h:mm:ss past an hour), shared by every theme.
use crate::ui::format_time;

/// Pad with spaces to `width` columns (or truncate with ellipsis if too long).
/// Measured in display columns: a wide (CJK) title padded by char count
/// overflowed its column.
fn pad_or_truncate(s: &str, width: usize) -> String {
    let cut = crate::ansi::truncate_plain(s, width);
    let visible = tui::width(&cut);
    format!("{cut}{}", " ".repeat(width.saturating_sub(visible)))
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
        let p = palette(ThemeKind::HiFi);
        let bar = render_solid_bar(0.51, 20, p);
        assert_eq!(crate::tui::Cols::cols(&bar), 20);
        let partial = bar
            .spans
            .iter()
            .find(|s| s.content.chars().any(|c| "▏▎▍▌▋▊▉".contains(c)))
            .expect("a partial block");
        // The rail is drawn solid in the rail colour…
        let rail = bar.spans.last().expect("a rail");
        assert_eq!(rail.style.fg, Some(RAIL));
        // …and the boundary cell's background is that SAME colour.
        assert_eq!(partial.style.bg, Some(RAIL), "the boundary must sit on the rail colour");
        // The rail's own cells carry no background.
        assert_eq!(rail.style.bg, None);
    }

    #[test]
    fn key_bar_does_not_advertise_a_viz_control() {
        // Hi-Fi draws a fixed VU panel and renders no switchable visualisation,
        // so a VIZ hint pointed at a key that changes nothing on screen.
        let p = palette(ThemeKind::HiFi);
        let bar = tui::plain(&hifi_marquee_keys(p, 200).line);
        assert!(!bar.contains("VIZ"), "Hi-Fi key bar still offers VIZ: {bar:?}");
        // The controls it does have must survive.
        for expected in ["PLAY", "SEEK", "TRACK", "VOL", "EQ", "FX", "LIB", "LYRICS", "THEME"] {
            assert!(bar.contains(expected), "key bar lost {expected}: {bar:?}");
        }
    }

    #[test]
    fn vu_label_row_spans_inner_width_with_clip_lamp() {
        // The row sits between the box borders, so it must be exactly inner_w.
        let p = palette(ThemeKind::HiFi);
        let idle = vu_label_row(60, false, p);
        let hot = vu_label_row(60, true, p);
        assert_eq!(crate::tui::Cols::cols(&idle), 60);
        assert_eq!(crate::tui::Cols::cols(&hot), 60);
        // Lamp is always present: a good-coloured ring idle, a danger dot on
        // clip — shape as well as colour, so it still reads without colour.
        let lamp = |l: &Line<'static>| l.spans.iter().find(|s| s.content == "○" || s.content == "●").cloned().unwrap();
        assert!(tui::plain(&idle).contains("CLIP"));
        assert_eq!((lamp(&idle).content.as_ref(), lamp(&idle).style), ("○", p.good));
        assert_eq!((lamp(&hot).content.as_ref(), lamp(&hot).style), ("●", p.danger));
    }
}
