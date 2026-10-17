//! Hi-Fi theme — Now Playing renderer.
//!
//! Amber-CRT, laid out as a vintage hi-fi stack: units as wide as the
//! window, one above the other — the transport (what is playing), the
//! meter unit (a fixed-length segmented peak meter, the signal lamps, a
//! vertical volume gauge) and the graphic equalizer (`e` gives it the keys,
//! Esc hands them back; a docked panel takes its place) — then the
//! dot-marquee key bar. The units are ratatui blocks.

use std::sync::atomic::Ordering;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType};
use ratatui::Frame;

use crate::eq::EQ_BANDS;
use crate::state::{Hit, ViewMode};
use crate::theme::{palette, Palette, ThemeKind};
use crate::tui::{self, Cols, LineBuf};
use crate::state::UiState;
use crate::ui::Ctx;

/// HiFi's player view (the playlist and the lyrics are panels over it).
pub fn draw(f: &mut Frame, cx: &mut Ctx) {
    draw_player(f, cx)
}

/// Rows of the transport unit: its borders and five inside (the big clock
/// and the display window, both boxed).
const TRANSPORT_H: u16 = 7;
/// Rows of the meter unit: its borders and five inside.
const METER_H: u16 = 7;
/// The EQ unit's rows besides its sliders: its borders, the three label
/// rows, the band readout, the headroom row and the key row.
const EQ_FIXED: u16 = 2 + 3 + 3;
/// The peak meter's length in segments, whatever the window's width (only a
/// window too narrow for it shrinks it).
pub const METER_SEGMENTS: usize = 40;
/// Columns before the meter's segments ("  L ") and after them.
const METER_LEAD: u16 = 4;
const METER_TRAIL: u16 = 2;
/// The volume gauge's segments: two to unity gain, one past it.
const VOL_SEGMENTS: usize = 3;
/// Columns inside the volume gauge's section.
const VOL_W: u16 = 7;
/// The narrowest signal section worth drawing.
const SIGNAL_MIN_W: u16 = 22;
/// The volume's top (`state.volume_up` stops there); 100 is unity gain.
const VOL_MAX: u32 = 150;
/// The narrowest EQ unit: the scale and ten faders at their narrowest, and
/// its box. Narrower, the top bands fell off its edge (still selectable),
/// so the EQ is the dialog there.
const EQ_MIN_W: u16 = crate::eq_ui::SCALE_W as u16 + EQ_BANDS as u16 * 6 + 2;
/// The EQ unit's slider rows at most: ±12 dB at 3 dB a row. Taller, it was
/// mostly empty rail on a big window.
const EQ_MAX_SLIDERS: u16 = 9;

/// Where each unit of the stack goes in a window.
#[derive(Debug, PartialEq)]
pub struct Stack {
    pub transport: Rect,
    pub meters: Rect,
    /// The EQ unit, when there are the rows for it.
    pub eq: Option<Rect>,
    /// The key bar (or the status line), right under the last unit.
    pub keys: Rect,
    /// Where docked panels go: the rows under the key bar, when there are
    /// the `need`ed rows.
    pub dock: Option<Rect>,
}

/// The stack in `area`: the transport and the meters at the top, then the
/// EQ if at least its five slider rows (and `EQ_MIN_W`) fit with the key bar
/// under it — and
/// the `need`ed rows for the docked panels under that (0: none docked) —
/// growing with the window up to `EQ_MAX_SLIDERS`, then the key bar, then
/// the docked panels in every row left. Short of the rows for both, the EQ
/// gives way to the docked panels.
pub fn stack(area: Rect, need: u16) -> Stack {
    let transport = tui::rows(area, 0, TRANSPORT_H as usize);
    let meters = tui::rows(area, TRANSPORT_H as usize, METER_H as usize);
    let below = TRANSPORT_H + METER_H;
    let spare = area.height.saturating_sub(below + 1);
    let eq = (spare >= EQ_FIXED + 5 + need && area.width >= EQ_MIN_W).then(|| {
        let sliders = (spare - EQ_FIXED - need).min(EQ_MAX_SLIDERS);
        let h = EQ_FIXED + crate::eq_ui::slider_rows(sliders as usize) as u16;
        tui::rows(area, below as usize, h as usize)
    });
    let keys = tui::rows(area, (below + eq.map_or(0, |r| r.height)) as usize, 1);
    let top = keys.y + 1;
    let rows = area.bottom().saturating_sub(top);
    let dock = (need > 0 && rows >= need).then_some(Rect { x: area.x, y: top, width: area.width, height: rows });
    Stack { transport, meters, eq, keys, dock }
}

/// The rows a docked panel needs under the key bar: the library showing
/// five tracks, the lyrics their current line.
pub fn panel_rows(view: ViewMode) -> u16 {
    if view == ViewMode::Playlist { DOCK_ROWS } else { LYRICS_DOCK_ROWS }
}

/// The columns a docked panel needs (every theme's, `ui::dock_min_width`).
pub fn panel_cols(view: ViewMode) -> u16 {
    crate::ui::dock_min_width(Some(view))
}

/// The rows the stack must leave under the key bar for `docked` in a window
/// `width` wide (the most any of them needs: the others go beside it or give
/// way). A panel too narrow to place takes none: kept, the EQ gave way to a
/// dock area left blank.
pub fn dock_need(docked: &[ViewMode], width: u16) -> u16 {
    docked.iter().filter(|&&v| panel_cols(v) <= width).map(|&v| panel_rows(v)).max().unwrap_or(0)
}

/// Whether HiFi's EQ unit is in the stack in a window `area` as `ui` has it
/// (a docked panel needs its rows too).
pub fn eq_fits(area: Rect, ui: &UiState) -> bool {
    stack(area, dock_need(&ui.docked, area.width)).eq.is_some()
}

/// Whether the library docks by itself in `area`: the EQ and five tracks
/// under it, beside the panels docked by hand (`others`), which keep their
/// place (docked but covered by the lyrics, it took the EQ's rows for
/// nothing). It never takes the EQ's place.
pub fn library_fits(area: Rect, others: &[ViewMode]) -> bool {
    let s = stack(area, dock_need(others, area.width).max(DOCK_ROWS));
    let mut all = others.to_vec();
    all.push(ViewMode::Playlist);
    s.eq.is_some() && s.dock.is_some_and(|d| dock_panels(&all, &all, d).iter().any(|&(v, _)| v == ViewMode::Playlist))
}

/// The docked panels' places in `region` (the rows under the key bar), the
/// one used last first in `recent`: the lyrics beside the library (on the
/// right, as a receiver's display beside its tape deck), else under it with
/// the library back at its five tracks, else the panel used last alone.
pub fn dock_panels(docked: &[ViewMode], recent: &[ViewMode], region: Rect) -> Vec<(ViewMode, Rect)> {
    let fits = |v: ViewMode, r: Rect| r.height >= panel_rows(v) && r.width >= panel_cols(v);
    let order: Vec<ViewMode> = recent.iter().copied().filter(|v| docked.contains(v)).collect();
    if order.len() == 2 && fits(ViewMode::Playlist, region) && fits(ViewMode::Lyrics, region) {
        let lyrics_w = (region.width * 3 / 10).clamp(panel_cols(ViewMode::Lyrics), LYRICS_MAX_W);
        if region.width >= panel_cols(ViewMode::Playlist) + 1 + lyrics_w {
            let library = Rect { width: region.width - lyrics_w - 1, ..region };
            let lyrics = Rect { x: library.right() + 1, width: lyrics_w, ..region };
            return vec![(ViewMode::Playlist, library), (ViewMode::Lyrics, lyrics)];
        }
        if region.height >= DOCK_ROWS + LYRICS_DOCK_ROWS {
            let library = Rect { height: DOCK_ROWS, ..region };
            let lyrics = Rect { y: library.bottom(), height: region.height - DOCK_ROWS, ..region };
            return vec![(ViewMode::Playlist, library), (ViewMode::Lyrics, lyrics)];
        }
    }
    order.into_iter().find(|&v| fits(v, region)).map(|v| vec![(v, region)]).unwrap_or_default()
}

fn draw_player(f: &mut Frame, cx: &mut Ctx) {
    let p = palette(ThemeKind::HiFi);
    let area = f.area();
    let s = stack(area, dock_need(&cx.ui.docked, area.width));
    draw_transport(f, cx, s.transport, p);
    draw_meters(f, cx, s.meters, p);
    cx.ui.eq_in_stack = s.eq.is_some();
    if let Some(eq) = s.eq {
        draw_eq(f, cx, eq, p);
    }

    // === Marquee key bar ===
    if !crate::ui::draw_status_line(f, cx.ui, s.keys, p) && !s.keys.is_empty() {
        let mut keys = hifi_marquee_keys(p, area.width as usize);
        light_pressed(&mut keys, cx.ui, ViewMode::Player);
        keys.register(&mut cx.ui.hits, s.keys.x, s.keys.y);
        tui::draw_lines(f.buffer_mut(), s.keys, &[keys.line]);
    }

    // Docked panels go below the key bar, when the window has the rows.
    if let Some(region) = s.dock {
        let panels = dock_panels(&cx.ui.docked, &cx.ui.panel_recent, region);
        crate::ui::draw_docked(f, cx, &panels);
    }
}

/// The transport unit: the double-line box, KEET and the play modes in its
/// top border with the track counter, the segmented clock beside the title,
/// the meta line, the progress bar and the bit-perfect verdict.
fn draw_transport(f: &mut Frame, cx: &mut Ctx, r: Rect, p: &Palette) {
    let state = cx.state;
    let idx = state.current_track.load(Ordering::Relaxed);
    let title = cx.ui.metadata_cache.title(idx).unwrap_or_else(|| cx.name.to_string());
    let (artist, album) = cx.ui.metadata_cache.artist_album(idx);

    // Top border: K E E T │ ▶ PLAY · SHUF-ON · RPT-ALL  ...  TRACK 05 / 34
    let track_n = idx + 1;
    let track_total = state.total_tracks.load(Ordering::Relaxed);
    let shuffle_on = cx.ui.shuffle;
    let repeat_label = repeat_short(state.repeat_mode());
    // `‖`, not ⏸: that is also an emoji (a blue square, wider than its cell,
    // which ran into the word and pushed the modes right). PLAY padded to
    // PAUSE's width, so SHUF and RPT stay where they are.
    let play_label = if state.is_paused() { "‖ PAUSE" } else { "▶ PLAY " };
    let mut head = LineBuf::new();
    head.styled("═", p.fg).reset().push(' ').styled("K E E T", p.accent.patch(p.bold)).reset().push_str("  ").styled("│", p.dim).reset().push_str("  ");
    let head = head.into_line();
    // Spelled out, like RPT-OFF/ALL/ONE: brightness alone is colour alone.
    let modes = [
        tui::line(play_label, p.accent),
        tui::line(if shuffle_on { "SHUF-ON" } else { "SHUF-OFF" }, if shuffle_on { p.fg } else { p.dim }),
        tui::line(repeat_label, if repeat_label != "RPT-OFF" { p.fg } else { p.dim }),
    ];
    let mut sep = LineBuf::new();
    sep.push_str("  ").styled("·", p.dim).reset().push_str("  ");
    let sep = sep.into_line();
    let mut right = LineBuf::new();
    right.push(' ').styled("TRACK", p.dim).reset().push(' ').styled(&format!("{track_n:02}"), p.fg).reset().push(' ');
    right.styled(&format!("/ {track_total}"), p.dim).reset().push(' ').styled("═", p.fg);
    let right = right.into_line();
    // Narrow window: the track counter goes first, then the modes, whole,
    // from the end (`fit_segments`: cut, `SHUF-O` and a `·` dangled).
    let room = (r.width as usize).saturating_sub(4);
    let fit = |width: usize| {
        let modes = tui::fit_segments(&modes, &sep, width.saturating_sub(head.cols() + 1));
        let mut left = head.clone();
        left.spans.extend(modes.spans);
        left.spans.push(ratatui::text::Span::raw(" "));
        tui::truncate(left, width)
    };
    let mut block = Block::bordered().border_type(BorderType::Double).border_style(p.fg);
    let whole = fit(usize::MAX);
    if whole.cols() + 2 + right.cols() <= room {
        block = block.title_top(right.right_aligned());
    }
    block = block.title_top(fit(room));
    let inner = block.inner(r);
    f.render_widget(block, r);
    if inner.is_empty() {
        return;
    }

    let progress = if state.total_secs() > 0.0 { (state.time_secs() / state.total_secs()).min(1.0) } else { 0.0 };
    let cur = clock_text(state.time_secs(), state.total_secs());
    let tot = format_time(state.total_secs());

    // The clock: big digits in their box when the window has the columns
    // for them beside a display window worth reading, else one row.
    let big = big_clock(&cur, p);
    let big_w = big[0].cols() as u16;
    let small_w = tui::width(&cur) as u16 + 2;
    let (clock_w, big) = if inner.width >= big_w + 4 + 1 + DISPLAY_MIN_W + 2 { (big_w + 4, Some(big)) } else { (small_w + 2, None) };
    let clock = Rect { x: inner.x + 1, width: clock_w, ..inner }.intersection(inner);
    let clock_block = Block::bordered().border_style(p.fg);
    let clock_in = clock_block.inner(clock);
    f.render_widget(clock_block, clock);
    match big {
        Some(lines) => tui::draw_lines(f.buffer_mut(), tui::indent(clock_in, 1), &lines),
        None => {
            let row = tui::rows(clock_in, clock_in.height as usize / 2, 1);
            let text = format!(" {cur} ");
            tui::draw_lines(f.buffer_mut(), row, &[tui::line(text, p.accent.patch(p.bold))]);
        }
    }

    // The display window: the title, artist · album and the format, the
    // progress bar; the verdict (exclusive mode) in its bottom border.
    let x = clock.right() + 1;
    let window = Rect { x, width: inner.right().saturating_sub(x + 1), ..inner }.intersection(inner);
    let src_rate = state.sample_rate.load(Ordering::Relaxed) as u32;
    let out_rate = state.output_rate.load(Ordering::Relaxed) as u32;
    let bits = state.bits_per_sample.load(Ordering::Relaxed);
    // 0 = a lossy codec: no bit depth.
    let depth = if bits > 0 { format!("{bits}-BIT ") } else { String::new() };
    let format = if src_rate == out_rate {
        format!("{depth}{:.1}K", src_rate as f32 / 1000.0)
    } else {
        format!("{depth}{:.1}→{:.1}K", src_rate as f32 / 1000.0, out_rate as f32 / 1000.0)
    };
    let verdict = crate::signal::verdict(&crate::signal::PathInputs::from_state(state, cx.fx_name, cx.cf_name));
    let text_w = (window.width as usize).saturating_sub(4);
    let mut block = Block::bordered().border_style(p.fg);
    if let Some((ok, text)) = crate::signal::fitted(&verdict, "  ·  ", text_w.saturating_sub(2)) {
        let mut l = LineBuf::new();
        l.push(' ').styled(&text.to_uppercase(), if ok { p.good } else { p.warn }).reset().push(' ');
        block = block.title_bottom(l.into_line().right_aligned());
    }
    let shown = block.inner(window);
    f.render_widget(block, window);
    let text = tui::indent(shown, 1);

    // Brightness for hierarchy: the title in bold accent, artist and album
    // in the text colour, the format dim.
    // Each an item of its own: joined, a narrow window cut the album mid-word
    // instead of dropping it whole.
    let mut meta: Vec<Line<'static>> = [artist, album].into_iter().flatten().map(|s| tui::line(s.to_uppercase(), p.fg)).collect();
    meta.push(tui::line(format, p.dim));
    let mut sep = LineBuf::new();
    sep.push_str("  ").styled("·", p.dim).reset().push_str("  ");
    let bar_w = text_w.saturating_sub(tui::width(&tot) + 4).max(2);
    let mut bar_row = LineBuf::new();
    bar_row.append(render_solid_bar(progress, bar_w, p)).reset().push_str("  ").styled(&format!("/ {tot}"), p.dim);
    let lines = [
        tui::line(pad_or_truncate(&title.to_uppercase(), text_w), p.accent.patch(p.bold)),
        tui::fit_segments(&meta, &sep.into_line(), text_w),
        bar_row.into_line(),
    ];
    let bar = Rect { x: text.x, y: text.y + 2, width: bar_w as u16, height: 1 }.intersection(text);
    if !bar.is_empty() {
        cx.ui.hits.push((bar, Hit::Seek));
    }
    tui::draw_lines(f.buffer_mut(), text, &lines);
}

/// The display window's narrowest worth giving the big clock up for.
const DISPLAY_MIN_W: u16 = 40;

/// The clock's time `secs` into a track `total` long, in the total's shape:
/// h:mm:ss from the start in a track over an hour (a symphony, a mix), so
/// the clock does not widen at the hour and push the display window aside.
fn clock_text(secs: f64, total: f64) -> String {
    if total < 3600.0 {
        return format_time(secs);
    }
    let s = secs.max(0.0) as u64;
    // As many hour digits as the total has.
    let digits = (total as u64 / 3600).to_string().len();
    format!("{:0digits$}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// A clock's digits three rows tall, in half and full blocks (a seven-
/// segment display's shapes), a column between characters: "02:34" is 17
/// columns. Bold accent.
fn big_clock(time: &str, p: &Palette) -> [Line<'static>; 3] {
    fn glyph(c: char) -> [&'static str; 3] {
        match c {
            '0' => ["█▀█", "█ █", "█▄█"],
            '1' => ["  █", "  █", "  █"],
            '2' => ["▀▀█", "█▀▀", "█▄▄"],
            '3' => ["▀▀█", "▀▀█", "▄▄█"],
            '4' => ["█ █", "▀▀█", "  █"],
            '5' => ["█▀▀", "▀▀█", "▄▄█"],
            '6' => ["█▀▀", "█▀█", "█▄█"],
            '7' => ["▀▀█", "  █", "  █"],
            '8' => ["█▀█", "█▀█", "█▄█"],
            '9' => ["█▀█", "▀▀█", "▄▄█"],
            ':' => ["▄", " ", "▀"],
            _ => [" ", " ", " "],
        }
    }
    let style = p.accent.patch(p.bold);
    let rows: [String; 3] = std::array::from_fn(|r| time.chars().map(|c| glyph(c)[r]).collect::<Vec<_>>().join(" "));
    rows.map(|r| tui::line(r, style))
}

/// How the meter unit's inside, `w` columns, is shared: the peak meter
/// (fixed at `METER_SEGMENTS` while the window allows), the signal lamps
/// (what is left, dropped below `SIGNAL_MIN_W`) and the volume gauge, a
/// divider between each two.
#[derive(Debug, PartialEq)]
struct MeterLayout {
    segments: usize,
    vu_w: u16,
    signal_w: u16,
    vol_w: u16,
}

fn meter_layout(w: u16) -> MeterLayout {
    let vu_full = METER_LEAD + METER_SEGMENTS as u16 + METER_TRAIL;
    if w >= vu_full + 1 + SIGNAL_MIN_W + 1 + VOL_W {
        MeterLayout { segments: METER_SEGMENTS, vu_w: vu_full, signal_w: w - vu_full - VOL_W - 2, vol_w: VOL_W }
    } else if w >= vu_full + 1 + VOL_W {
        MeterLayout { segments: METER_SEGMENTS, vu_w: w - 1 - VOL_W, signal_w: 0, vol_w: VOL_W }
    } else {
        // Too narrow for the whole meter: it shrinks, and the gauge goes
        // when not even eight segments are left beside it.
        let vol_w = if w >= METER_LEAD + 8 + METER_TRAIL + 1 + VOL_W { VOL_W } else { 0 };
        let vu_w = w.saturating_sub(if vol_w > 0 { vol_w + 1 } else { 0 });
        MeterLayout { segments: (vu_w.saturating_sub(METER_LEAD + METER_TRAIL) as usize).max(1), vu_w, signal_w: 0, vol_w }
    }
}

/// A divider down a unit's inside at column `x`, joined to its borders.
fn divider(f: &mut Frame, r: Rect, x: u16, p: &Palette) {
    let buf = f.buffer_mut();
    for y in r.y..r.bottom() {
        let glyph = if y == r.y {
            "┬"
        } else if y + 1 == r.bottom() {
            "┴"
        } else {
            "│"
        };
        if let Some(c) = buf.cell_mut((x, y)) {
            c.set_symbol(glyph).set_style(p.fg);
        }
    }
}

/// A unit's name for its top border: a rule glyph, then the name.
pub(crate) fn border_title(rule: &str, rule_style: Style, name: &str, style: Style) -> Line<'static> {
    let mut l = LineBuf::new();
    l.styled(rule, rule_style).reset().styled(&format!(" {name} "), style);
    l.into_line()
}

/// A section's name in the top border of the unit `r`, from column `x`.
fn section_title(f: &mut Frame, r: Rect, x: u16, name: &str, p: &Palette) {
    let row = Rect { x, y: r.y, width: r.right().saturating_sub(x + 1), height: 1 }.intersection(r);
    tui::draw_lines(f.buffer_mut(), row, &[border_title("─", p.fg, name, p.dim)]);
}

/// The meter unit: the peak meter (L, the dB scale, R, the clip lamp), the
/// signal lamps and the volume gauge.
fn draw_meters(f: &mut Frame, cx: &mut Ctx, r: Rect, p: &Palette) {
    let state = cx.state;
    let block = Block::bordered().border_style(p.fg).title_top(border_title("─", p.fg, "PEAK METER", p.dim));
    let inner = block.inner(r);
    f.render_widget(block, r);
    if inner.is_empty() {
        return;
    }
    let m = meter_layout(inner.width);

    // Peak meter: the scale between the channels, the clip lamp under them.
    // The raw peaks: the VU's smoothed ones reach about half of a jump per
    // frame, and a transient read several dB low (its hold too).
    let (lp, rp) = state.take_raw_peaks();
    let meter = &mut cx.ui.hifi_meter;
    meter.update([lp, rp], std::time::Instant::now());
    let [(lp, lp_dot), (rp, rp_dot)] = [0, 1].map(|c| (meter_frac(meter.level_db[c]), meter_frac(meter.hold_db[c])));
    // `b`: the LED dot matrix (Dots, the default) or the segmented bars.
    let dots = state.viz_style() == crate::state::VizStyle::Dots;
    let bar = |peak: f32, hold: f32| if dots { vu_dots(peak, hold, m.segments, p) } else { vu_bar(peak, hold, m.segments, p) };
    let channel = |label: &str, bar: Line<'static>| {
        let mut l = LineBuf::new();
        l.push_str("  ").styled(label, p.dim).reset().push(' ').append(bar);
        l.into_line()
    };
    let mut scale = LineBuf::new();
    scale.spaces(METER_LEAD as usize).append(render_db_scale(m.segments, p));
    let mut clip = LineBuf::new();
    clip.spaces((METER_LEAD as usize + m.segments).saturating_sub(6)).append(clip_lamp(state.is_clipping(), p));
    let vu = [
        Line::default(),
        channel("L", bar(lp, lp_dot)),
        scale.into_line(),
        channel("R", bar(rp, rp_dot)),
        clip.into_line(),
    ];
    let vu_area = Rect { width: m.vu_w, ..inner }.intersection(inner);
    // HiFi's visualization is this meter (`viz_on_screen` asks where it was).
    if !vu_area.is_empty() {
        cx.ui.viz_drawn_at = Some(vu_area);
    }
    tui::draw_lines(f.buffer_mut(), vu_area, &vu);

    let mut x = inner.x + m.vu_w;
    if m.signal_w > 0 {
        divider(f, r, x, p);
        section_title(f, r, x + 1, "SIGNAL", p);
        let buf = state.buffer_level.load(Ordering::Relaxed);
        let ring_cap = state.ring_capacity.load(Ordering::Relaxed).max(1);
        cx.stats.update_buf(buf as f32 / ring_cap as f32 * 100.0);
        let (text_w, spectrum_w) = signal_split(m.signal_w);
        let lines = signal_lines(cx, cx.stats.smoothed_buf_pct as u32, text_w as usize, p);
        let area = Rect { x: x + 2, width: text_w, ..inner }.intersection(inner);
        tui::draw_lines(f.buffer_mut(), area, &lines);
        if spectrum_w > 0 {
            let (l, r) = (state.get_spectrum(), state.get_spectrum_r());
            let mono: [f32; crate::state::SPECTRUM_BANDS] = std::array::from_fn(|i| (l[i] + r[i]) / 2.0);
            let (w, h) = (spectrum_w as usize, inner.height as usize);
            let lines = if dots { spectrum_dots(&mono, &state.get_dots(), w, h, p) } else { spectrum_lines(&mono, &state.get_dots(), w, h, p) };
            let area = Rect { x: x + 2 + text_w + 2, width: spectrum_w, ..inner }.intersection(inner);
            tui::draw_lines(f.buffer_mut(), area, &lines);
        }
        x += 1 + m.signal_w;
    }
    if m.vol_w > 0 {
        divider(f, r, x, p);
        section_title(f, r, x + 1, "VOL", p);
        let vol = state.volume.load(Ordering::Relaxed);
        let lines = if dots { volume_dots(vol, inner.height as usize, m.vol_w as usize, p) } else { volume_gauge(vol, inner.height as usize, m.vol_w as usize, p) };
        let area = Rect { x: x + 1, width: m.vol_w, ..inner }.intersection(inner);
        tui::draw_lines(f.buffer_mut(), area, &lines);
    }
}

/// The narrowest the text gives way to the spectrum, and the spectrum's
/// two widths (ten bars a column or two wide, a gap between).
const SIGNAL_TEXT_MIN_W: u16 = 32;
const SPECTRUM_MIN_W: u16 = 2 * SPECTRUM_OCTAVES as u16 - 1;
const SPECTRUM_MAX_W: u16 = 3 * SPECTRUM_OCTAVES as u16 - 1;

/// How the signal section's `w` columns (a column of margin each side) are
/// shared: the text, and the spectrum beside it, 2 columns apart — the
/// spectrum only once the text can keep `SIGNAL_TEXT_MIN_W`.
fn signal_split(w: u16) -> (u16, u16) {
    let inside = w.saturating_sub(2);
    if inside < SIGNAL_TEXT_MIN_W + 2 + SPECTRUM_MIN_W {
        return (inside, 0);
    }
    let spectrum = if inside >= SIGNAL_TEXT_MIN_W + 2 + SPECTRUM_MAX_W { SPECTRUM_MAX_W } else { SPECTRUM_MIN_W };
    (inside - 2 - spectrum, spectrum)
}

/// The spectrum's bands: one per octave, on the EQ's centres (31 Hz to
/// 16 kHz), as a hi-fi's display had them — 31 third-octave bars read as
/// studio gear. Each takes the analyser's three third-octave bands around
/// its centre (the first also 20 Hz, the last 20 kHz).
const SPECTRUM_OCTAVES: usize = 10;

fn octave_bands(i: usize) -> std::ops::Range<usize> {
    let lo = if i == 0 { 0 } else { 3 * i + 1 };
    let hi = if i + 1 == SPECTRUM_OCTAVES { crate::state::SPECTRUM_BANDS } else { 3 * i + 4 };
    lo..hi
}

/// A small spectrum analyser: ten octave bars (`octave_bands`, the peak of
/// each), a column wide in 19 columns and two in 29, a gap between; `rows`
/// segments tall (`▆`: the quarter row above is the gap between segments),
/// amber, the peak hold a bright segment above, unlit segments ghosts.
/// Right-aligned in `w` columns.
fn spectrum_lines(bands: &[f32; crate::state::SPECTRUM_BANDS], peaks: &[f32; crate::state::SPECTRUM_BANDS], w: usize, rows: usize, p: &Palette) -> Vec<Line<'static>> {
    let bar_w = if w >= SPECTRUM_MAX_W as usize { 2 } else { 1 };
    let used = SPECTRUM_OCTAVES * (bar_w + 1) - 1;
    let segments = |v: &[f32; crate::state::SPECTRUM_BANDS], i: usize| {
        let level = v[octave_bands(i)].iter().copied().fold(0.0, f32::max);
        (level.clamp(0.0, 1.0) * rows as f32).round() as usize
    };
    let lit: Vec<usize> = (0..SPECTRUM_OCTAVES).map(|i| segments(bands, i)).collect();
    let hold: Vec<usize> = (0..SPECTRUM_OCTAVES).map(|i| segments(peaks, i)).collect();
    let mut out = Vec::with_capacity(rows);
    for row in 0..rows {
        let seg = rows - row; // 1 = the bottom segment
        let mut l = LineBuf::new();
        l.spaces(w.saturating_sub(used));
        for i in 0..SPECTRUM_OCTAVES {
            if i > 0 {
                l.reset().push(' ');
            }
            let (glyph, style) = if seg <= lit[i] {
                ('▆', p.accent)
            } else if seg == hold[i] {
                ('▆', p.fg)
            } else {
                ghost('▆', Zone::Normal, p)
            };
            l.styled(&glyph.to_string().repeat(bar_w), style);
        }
        out.push(l.into_line());
    }
    out
}

// --- The LED dot matrix (`b`: Dots) -----------------------------------
//
// A braille cell is a 2×4 grid of dots with gaps both ways, so a meter gets
// gaps between its segments AND between its channels — which a block,
// split one way only, cannot give. One colour per cell: a cell part lit
// shows its lit dots only, a cell with none lit is a ghost (every dot,
// dark).

/// A braille cell's dots as bits: `DOTS[row][column]`, row 0 on top.
const DOTS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];

fn braille(mask: u8) -> char {
    char::from_u32(0x2800 + mask as u32).unwrap_or(' ')
}

/// The left `n` (0 to 2) dot columns of a cell, all four rows.
fn dot_columns(n: usize) -> u8 {
    (0..4).map(|r| DOTS[r][..n.min(2)].iter().fold(0, |m, b| m | b)).fold(0, |m, b| m | b)
}

/// The bottom `n` (0 to 4) dot rows of a cell, both columns.
fn dot_rows(n: usize) -> u8 {
    (4 - n.min(4)..4).map(|r| DOTS[r][0] | DOTS[r][1]).fold(0, |m, b| m | b)
}

/// An unlit cell: every dot, in the zone's dark colour; blank without
/// colour, where a dark dot would read as lit.
fn ghost_dots(zone: Zone, p: &Palette) -> (char, Style) {
    if crate::term::no_color() {
        (' ', Style::new())
    } else {
        ('⣿', zone.ghost(p))
    }
}

/// The peak meter as dots: two dot columns a cell (80 steps in 40 cells),
/// in the zones' colours; the peak hold one bright dot column past the level.
fn vu_dots(peak: f32, peak_dot: f32, cells: usize, p: &Palette) -> Line<'static> {
    let n = cells.max(1);
    let lit = (peak.clamp(0.0, 1.0) * (2 * n) as f32) as usize;
    let hold = ((peak_dot.clamp(0.0, 1.0) * (2 * n) as f32) as usize).min(2 * n - 1);
    let mut bar = LineBuf::new();
    for i in 0..n {
        let zone = Zone::of(i as f32 / n as f32);
        let on = lit.saturating_sub(2 * i).min(2);
        let held = peak_dot > 0.0 && hold >= lit && hold / 2 == i;
        let (glyph, style) = if on > 0 {
            let mask = dot_columns(on) | if held { dot_columns(hold % 2 + 1) & !dot_columns(hold % 2) } else { 0 };
            (braille(mask), zone.lit(p))
        } else if held {
            (braille(dot_columns(hold % 2 + 1) & !dot_columns(hold % 2)), if zone == Zone::Normal { p.fg } else { zone.lit(p) })
        } else {
            ghost_dots(zone, p)
        };
        bar.style(style).push(glyph);
    }
    bar.into_line()
}

/// A level (0 to 1) as `cells` of dots, two dot columns each, amber.
fn level_dots(level: f32, cells: usize, p: &Palette) -> Line<'static> {
    let lit = (level.clamp(0.0, 1.0) * (2 * cells) as f32).round() as usize;
    let mut l = LineBuf::new();
    for i in 0..cells {
        let on = lit.saturating_sub(2 * i).min(2);
        let (glyph, style) = if on > 0 { (braille(dot_columns(on)), p.accent) } else { ghost_dots(Zone::Normal, p) };
        l.style(style).push(glyph);
    }
    l.into_line()
}

/// A column of dots `rows` cells tall, `lit` dots lit from the bottom and
/// the dot `hold` (counted from the bottom, 1-based; 0: none) bright: the
/// cell `row` (0 on top) of it, as (dots, whether any is lit, whether the
/// hold is there).
fn column_cell(rows: usize, row: usize, lit: usize, hold: usize) -> (u8, bool, bool) {
    let below = (rows - 1 - row) * 4; // dots under this cell
    let on = lit.saturating_sub(below).min(4);
    let held = hold > lit && hold > below && hold <= below + 4;
    let hold_mask = if held { dot_rows(hold - below) & !dot_rows(hold - below - 1) } else { 0 };
    (dot_rows(on) | hold_mask, on > 0, held)
}

/// The spectrum as dots: the ten octave bars four dots a row (20 levels in
/// five rows), amber, the peak hold a bright dot row above; a cell or two
/// wide as `spectrum_lines`.
fn spectrum_dots(bands: &[f32; crate::state::SPECTRUM_BANDS], peaks: &[f32; crate::state::SPECTRUM_BANDS], w: usize, rows: usize, p: &Palette) -> Vec<Line<'static>> {
    let bar_w = if w >= SPECTRUM_MAX_W as usize { 2 } else { 1 };
    let used = SPECTRUM_OCTAVES * (bar_w + 1) - 1;
    let dots = |v: &[f32; crate::state::SPECTRUM_BANDS], i: usize| {
        let level = v[octave_bands(i)].iter().copied().fold(0.0, f32::max);
        (level.clamp(0.0, 1.0) * (rows * 4) as f32).round() as usize
    };
    let lit: Vec<usize> = (0..SPECTRUM_OCTAVES).map(|i| dots(bands, i)).collect();
    let hold: Vec<usize> = (0..SPECTRUM_OCTAVES).map(|i| dots(peaks, i)).collect();
    (0..rows)
        .map(|row| {
            let mut l = LineBuf::new();
            l.spaces(w.saturating_sub(used));
            for i in 0..SPECTRUM_OCTAVES {
                if i > 0 {
                    l.reset().push(' ');
                }
                let (mask, on, held) = column_cell(rows, row, lit[i], hold[i]);
                let (glyph, style) = if on {
                    (braille(mask), p.accent)
                } else if held {
                    (braille(mask), p.fg)
                } else {
                    ghost_dots(Zone::Normal, p)
                };
                l.styled(&glyph.to_string().repeat(bar_w), style);
            }
            l.into_line()
        })
        .collect()
}

/// The volume gauge as dots: three cells tall, four dots a row (100% is
/// two cells, eight dots), the top cell past unity in the warn colour; laid
/// out as `volume_gauge`.
fn volume_dots(vol: u32, rows: usize, w: usize, p: &Palette) -> Vec<Line<'static>> {
    const GAUGE_W: usize = 3;
    let total = VOL_SEGMENTS * 4;
    let lit = (vol.min(VOL_MAX) as f32 / VOL_MAX as f32 * total as f32).round() as usize;
    let pad = w.saturating_sub(GAUGE_W) / 2;
    let mut out = vec![Line::default()];
    for row in 0..VOL_SEGMENTS {
        let (mask, on, _) = column_cell(VOL_SEGMENTS, row, lit, 0);
        let (glyph, style) = if on { (braille(mask), if row == 0 { p.warn } else { p.accent }) } else { ghost_dots(Zone::Normal, p) };
        let mut l = LineBuf::new();
        l.spaces(pad).styled(&glyph.to_string().repeat(GAUGE_W), style);
        out.push(l.into_line());
    }
    while out.len() + 1 < rows {
        out.push(Line::default());
    }
    let value = vol.to_string();
    let mut l = LineBuf::new();
    l.spaces(w.saturating_sub(tui::width(&value)) / 2).styled(&value, p.bold.patch(if vol > 100 { p.warn } else { p.accent }));
    out.push(l.into_line());
    out
}

/// The clip lamp: `○` idle (good), `●` clipping (danger) — shape as well as
/// colour, so it still reads without colour.
fn clip_lamp(clipping: bool, p: &Palette) -> Line<'static> {
    let (lamp, glyph) = if clipping { (p.danger, "●") } else { (p.good, "○") };
    let mut l = LineBuf::new();
    l.styled("CLIP", p.dim).reset().push(' ').styled(glyph, lamp);
    l.into_line()
}

/// A peak meter channel: `segments` cells, a lit segment a `▊` (the quarter
/// cell it leaves blank is the gap between segments), an unlit one a ghost
/// segment (`ghost`); amber, orange from -6 dB, red from -3 dB (`Zone`);
/// the peak hold a lit segment past the level. `peak` and `peak_dot` are
/// positions on the scale (`meter_frac`).
fn vu_bar(peak: f32, peak_dot: f32, segments: usize, p: &Palette) -> Line<'static> {
    let n = segments.max(1);
    let lit = (peak.clamp(0.0, 1.0) * n as f32) as usize;
    // At full scale the hold is the last segment (n was past the end: it
    // vanished exactly when the track hit 0 dB).
    let dot = ((peak_dot.clamp(0.0, 1.0) * n as f32) as usize).min(n - 1);
    let mut bar = LineBuf::new();
    for i in 0..n {
        let zone = Zone::of(i as f32 / n as f32);
        if i < lit {
            bar.style(zone.lit(p)).push('▊');
        } else if i == dot && peak_dot > 0.0 {
            bar.style(if zone == Zone::Normal { p.fg } else { zone.lit(p) }).push('▊');
        } else {
            let (glyph, style) = ghost('▊', zone, p);
            bar.style(style).push(glyph);
        }
    }
    bar.into_line()
}

/// A stretch of the peak meter's scale: amber up to -6 dB, orange to -3 dB,
/// red above.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Zone {
    Normal,
    Warm,
    Hot,
}

impl Zone {
    /// The zone of a position on the scale (`meter_frac`).
    fn of(frac: f32) -> Zone {
        if frac >= meter_frac(-3.0) {
            Zone::Hot
        } else if frac >= meter_frac(-6.0) {
            Zone::Warm
        } else {
            Zone::Normal
        }
    }

    fn lit(self, p: &Palette) -> Style {
        match self {
            Zone::Normal => p.accent,
            Zone::Warm => p.warn,
            Zone::Hot => p.danger,
        }
    }

    /// The dark version of the zone's colour, for its unlit segments.
    fn ghost(self, p: &Palette) -> Style {
        match self {
            Zone::Normal => p.rule,
            Zone::Warm => Style::new().fg(GHOST_WARM),
            Zone::Hot => Style::new().fg(GHOST_HOT),
        }
    }
}

/// The dark orange and dark red of the meter's unlit segments.
const GHOST_WARM: Color = Color::Rgb(68, 34, 18);
const GHOST_HOT: Color = Color::Rgb(72, 22, 18);

/// An unlit segment: the segment's own glyph, dark, like an unlit element
/// of a fluorescent display, in its zone's dark colour. Without colour a
/// dark segment would read as lit, so it is a dot there.
fn ghost(glyph: char, zone: Zone, p: &Palette) -> (char, Style) {
    if crate::term::no_color() {
        ('·', p.rule)
    } else {
        (glyph, zone.ghost(p))
    }
}

/// Where a level (dBFS) sits on the meter, 0 to 1: the scale printed under
/// it (`render_db_scale`), -40 dB and below at the start. The meter used to
/// take the LINEAR peak against this dB scale: -6 dB lit half the bar, and
/// near the top every 50 ms of decay dropped three segments.
pub fn meter_frac(db: f32) -> f32 {
    const SCALE: [(f32, f32); 7] = [(-40.0, 0.0), (-30.0, 0.2), (-20.0, 0.4), (-12.0, 0.6), (-6.0, 0.75), (-3.0, 0.85), (0.0, 1.0)];
    if db <= SCALE[0].0 {
        return 0.0;
    }
    for w in SCALE.windows(2) {
        let ((d0, f0), (d1, f1)) = (w[0], w[1]);
        if db <= d1 {
            return f0 + (db - d0) / (d1 - d0) * (f1 - f0);
        }
    }
    1.0
}

/// The peak meter's own ballistics, in dB and real time: a level rises at
/// once and falls at `RELEASE_DB_S`; the peak hold stays `HOLD_SECS`, then
/// falls at `HOLD_FALL_DB_S` — a programme meter's pace, so the top segment
/// no longer flickers with every transient.
#[derive(Debug, Clone)]
pub struct PeakMeter {
    pub level_db: [f32; 2],
    pub hold_db: [f32; 2],
    hold_left: [f32; 2],
    last: Option<std::time::Instant>,
}

/// Silence, on the meter's terms.
const METER_FLOOR_DB: f32 = -60.0;
const RELEASE_DB_S: f32 = 12.0;
const HOLD_SECS: f32 = 1.0;
const HOLD_FALL_DB_S: f32 = 8.0;

impl Default for PeakMeter {
    fn default() -> Self {
        PeakMeter { level_db: [METER_FLOOR_DB; 2], hold_db: [METER_FLOOR_DB; 2], hold_left: [0.0; 2], last: None }
    }
}

impl PeakMeter {
    /// Whether it has fallen out of sight (nothing lit, nothing held: the
    /// scale starts at -40 dB, the ballistics' floor is lower).
    pub fn settled(&self) -> bool {
        self.level_db.iter().chain(&self.hold_db).all(|&d| meter_frac(d) == 0.0)
    }

    /// Take this frame's peaks (linear, L and R) at `now`.
    pub fn update(&mut self, peaks: [f32; 2], now: std::time::Instant) {
        // A long gap (the meter out of view: another theme, a dialog) starts
        // it afresh; carried on, a peak from minutes ago lit the red.
        // (Shown, it is drawn at least every `IDLE_FRAME`, 250 ms: a late
        // idle frame is a quarter second of fall, not a reset.)
        let gap = self.last.map(|t| now.saturating_duration_since(t).as_secs_f32());
        if gap.is_some_and(|g| g > 1.0) {
            *self = PeakMeter::default();
        }
        let dt = gap.filter(|&g| g <= 1.0).map_or(0.0, |g| g.min(0.25));
        self.last = Some(now);
        for (c, peak) in peaks.into_iter().enumerate() {
            let db = (20.0 * peak.max(1e-6).log10()).max(METER_FLOOR_DB);
            self.level_db[c] = db.max(self.level_db[c] - RELEASE_DB_S * dt);
            if self.level_db[c] >= self.hold_db[c] {
                self.hold_db[c] = self.level_db[c];
                self.hold_left[c] = HOLD_SECS;
            } else if self.hold_left[c] > 0.0 {
                self.hold_left[c] -= dt;
            } else {
                self.hold_db[c] = (self.hold_db[c] - HOLD_FALL_DB_S * dt).max(self.level_db[c]);
            }
        }
    }
}

/// A lamp of the signal section: lit, it is a window of reverse video
/// (which survives NO_COLOR); unlit, dim text.
fn lamp(l: &mut LineBuf, text: &str, lit: bool, p: &Palette) {
    let style = if lit { p.accent.patch(p.bold).add_modifier(Modifier::REVERSED) } else { p.dim };
    l.styled(&format!(" {text} "), style).reset();
}

/// "44.1", "48", "22.05": a rate in kHz as a front panel prints it.
fn khz(rate: u32) -> String {
    let s = format!("{:.2}", rate as f64 / 1000.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A row of lamps, the one equal to `lit` lit (added in its place when it
/// is not a standard value), cut to `width` by dropping the unlit lamps
/// farthest from the lit one, so it always shows.
fn lamp_row(values: &[u32], lit: u32, label: impl Fn(u32) -> String, width: usize, p: &Palette) -> Line<'static> {
    let mut values = values.to_vec();
    if lit > 0 && !values.contains(&lit) {
        values.push(lit);
        values.sort_unstable();
    }
    let w = |vs: &[u32]| vs.iter().map(|&v| tui::width(&label(v)) + 2).sum::<usize>();
    while values.len() > 1 && w(&values) > width {
        let at = values.iter().position(|&v| v == lit).unwrap_or(0);
        // The end farther from the lit lamp.
        if at < values.len() - 1 - at { values.pop() } else { Some(values.remove(0)) };
    }
    let mut l = LineBuf::new();
    for v in values {
        lamp(&mut l, &label(v), v == lit, p);
    }
    l.into_line()
}

/// The signal section's rows, `w` columns: the output rate (what the DAC
/// receives) and the source's bit depth as lamps; the effects, crossfeed
/// and ReplayGain; the balance; the buffer.
/// Whether the meter unit has its SIGNAL section in a window `window_w`
/// columns wide (the unit spans it, borders inside).
pub(crate) fn signal_shown(window_w: u16) -> bool {
    meter_layout(window_w.saturating_sub(2)).signal_w > 0
}

/// The balance as HiFi's panel reads it: `C`, `L20`, `R20`.
fn balance_short(bal: i32) -> String {
    match bal {
        0 => "C".to_string(),
        b if b < 0 => format!("L{}", -b),
        b => format!("R{b}"),
    }
}

/// `FX NONE · XFEED OFF · RG TRACK` (and `BAL C` with `balance`, which the
/// SIGNAL section draws as a gauge), whole items fitted to `w`: the SIGNAL
/// section's DSP row, and the EQ's when that section is gone.
pub(crate) fn dsp_line(cx: &Ctx, p: &Palette, w: usize, balance: bool) -> Line<'static> {
    let state = cx.state;
    let rg = match state.rg_mode() {
        crate::state::RgMode::Album => "ALBUM",
        crate::state::RgMode::Off => "OFF",
        crate::state::RgMode::Track => "TRACK",
    };
    let item = |name: &str, value: String, on: bool| {
        let mut l = LineBuf::new();
        l.styled(name, p.dim).reset().push(' ').styled(&value, if on { p.fg } else { p.dim });
        l.into_line()
    };
    let fx = cx.fx_name.to_uppercase();
    let cf = cx.cf_name.to_uppercase();
    let mut items = vec![
        item("FX", fx.clone(), fx != "NONE"),
        item("XFEED", cf.clone(), cf != "OFF"),
        item("RG", rg.to_string(), rg != "OFF"),
    ];
    if balance {
        let bal = state.balance_value();
        items.push(item("BAL", balance_short(bal), bal != 0));
    }
    let mut sep = LineBuf::new();
    sep.push(' ').styled("·", p.dim).reset().push(' ');
    tui::fit_segments(&items, &sep.into_line(), w)
}

fn signal_lines(cx: &Ctx, buf_pct: u32, w: usize, p: &Palette) -> Vec<Line<'static>> {
    let state = cx.state;
    let label = |l: &mut LineBuf, s: &str| {
        l.styled(&format!("{s:<4}"), p.dim).reset();
    };
    let row = |name: &str, body: Line<'static>| {
        let mut l = LineBuf::new();
        label(&mut l, name);
        l.append(body);
        tui::truncate(l.into_line(), w)
    };
    let lamps_w = w.saturating_sub(4);

    const RATES: [u32; 8] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000];
    let out_rate = state.output_rate.load(Ordering::Relaxed) as u32;
    let mut rates = lamp_row(&RATES, out_rate, khz, lamps_w.saturating_sub(4), p);
    rates.spans.push(ratatui::text::Span::styled(" kHz", p.dim));
    let bits = state.bits_per_sample.load(Ordering::Relaxed);
    let depths = if bits == 0 {
        let mut l = LineBuf::new();
        lamp(&mut l, "LOSSY", true, p);
        l.into_line()
    } else {
        lamp_row(&[16, 24, 32], bits as u32, |b| b.to_string(), lamps_w, p)
    };

    let dsp = dsp_line(cx, p, w, false);

    let bal = state.balance_value();
    let mut balance = LineBuf::new();
    balance.styled("L ", p.dim);
    let pos = ((bal + 100) as f32 / 200.0 * 8.0).round() as usize;
    for i in 0..9 {
        if i == pos {
            balance.style(p.accent).push('●');
        } else if i == 4 {
            balance.style(p.dim).push('┼');
        } else {
            balance.style(p.rule).push('─');
        }
    }
    let bal_str = balance_short(bal);
    balance.reset().styled(" R", p.dim).reset().push_str("  ").styled(&bal_str, p.fg);

    let mut buffer = LineBuf::new();
    if state.viz_style() == crate::state::VizStyle::Dots {
        buffer.append(level_dots(buf_pct.min(100) as f32 / 100.0, 10, p));
    } else {
        let lit = ((buf_pct.min(100) as f32 / 10.0).round()) as usize;
        for i in 0..10 {
            if i < lit {
                buffer.style(p.accent).push('▊');
            } else {
                let (glyph, style) = ghost('▊', Zone::Normal, p);
                buffer.style(style).push(glyph);
            }
        }
    }
    buffer.reset().push_str("  ").styled(&format!("{buf_pct}%"), p.fg);

    vec![
        row("OUT", rates),
        row("BIT", depths),
        dsp,
        row("BAL", balance.into_line()),
        row("BUF", buffer.into_line()),
    ]
}

/// The vertical volume gauge: `VOL_SEGMENTS` segments of half the unity
/// level each, so 100% is exactly two lit and the third is the boost past
/// it (warn colour). A segment fills by eighths (`▁` to `▆`: the quarter row
/// above it is the gap between segments), unlit ones are ghosts. One blank
/// row on top, the value on the last: `rows` lines in `w` columns, the
/// segments beside the meter's L, scale and R rows.
fn volume_gauge(vol: u32, rows: usize, w: usize, p: &Palette) -> Vec<Line<'static>> {
    const STEPS: [char; 6] = ['▁', '▂', '▃', '▄', '▅', '▆'];
    const GAUGE_W: usize = 3;
    let per = VOL_MAX as f32 / VOL_SEGMENTS as f32;
    let pad = w.saturating_sub(GAUGE_W) / 2;
    let mut out = vec![Line::default()];
    for seg in (0..VOL_SEGMENTS).rev() {
        let n = ((vol as f32 - seg as f32 * per) / per * 6.0).round().clamp(0.0, 6.0) as usize;
        let over = (seg as f32 * per) as u32 >= 100;
        let mut l = LineBuf::new();
        l.spaces(pad);
        if n == 0 {
            let (glyph, style) = ghost('▆', Zone::Normal, p);
            l.styled(&glyph.to_string().repeat(GAUGE_W), style);
        } else {
            l.styled(&STEPS[n - 1].to_string().repeat(GAUGE_W), if over { p.warn } else { p.accent });
        }
        out.push(l.into_line());
    }
    while out.len() + 1 < rows {
        out.push(Line::default());
    }
    let value = vol.to_string();
    let mut l = LineBuf::new();
    l.spaces(w.saturating_sub(tui::width(&value)) / 2).styled(&value, p.bold.patch(if vol > 100 { p.warn } else { p.accent }));
    out.push(l.into_line());
    out
}

/// The EQ unit: the sliders with their frequency, type and gain rows, the
/// selected band's readout, the headroom row and its keys. Without the keys
/// no band is lit and the key row offers them (`e`); a click on a band
/// selects it and gives the EQ the keys.
fn draw_eq(f: &mut Frame, cx: &mut Ctx, r: Rect, p: &Palette) {
    let state = cx.state;
    // The keys are the unit's only while it is the EQ being edited: opened
    // over a floating panel (in Classic, then `t`), the dialog is.
    let editing = cx.ui.focus == ViewMode::Eq || (cx.ui.focus == ViewMode::Help && cx.ui.help_return == ViewMode::Eq);
    let focused = editing && crate::ui::eq_returns_to_stack(cx.ui);
    let name = if state.is_eq_custom() { "CUSTOM".to_string() } else { cx.eq_preset.name.to_uppercase() };
    let (border, title) = if focused { (p.accent, p.accent.patch(p.bold)) } else { (p.fg, p.dim) };
    let mut block = Block::bordered().border_style(border).title_top(border_title("─", border, &format!("GRAPHIC EQUALIZER · {name}"), title));
    // A window too narrow for the SIGNAL section shows FX, crossfeed,
    // ReplayGain and balance nowhere else: in the bottom border.
    if !signal_shown(f.area().width) {
        let mut l = LineBuf::new();
        l.styled("─", border).reset().push(' ').append(dsp_line(cx, p, (r.width as usize).saturating_sub(6), true)).push(' ');
        block = block.title_bottom(l.into_line());
    }
    let inner = block.inner(r);
    f.render_widget(block, r);
    cx.ui.hits.push((r, Hit::Panel(ViewMode::Eq)));
    let slider_h = inner.height.saturating_sub(EQ_FIXED - 2) as usize;
    if slider_h == 0 || inner.is_empty() {
        return;
    }
    let scale_w = crate::eq_ui::SCALE_W;
    // The preset list on the right, when the faders keep their narrowest.
    let list_w = if (inner.width as usize) >= scale_w + EQ_BANDS * 6 + 2 + PRESET_LIST_W && !cx.ui.eq_preset_names.is_empty() {
        PRESET_LIST_W as u16
    } else {
        0
    };
    let faders = Rect { width: inner.width - if list_w > 0 { list_w + 2 } else { 0 }, ..inner };
    if list_w > 0 {
        let x = faders.right() + 1;
        divider(f, r, x, p);
        section_title(f, r, x + 1, "PRESETS", p);
        let current = (!state.is_eq_custom()).then(|| state.eq_index());
        let list = Rect { x: x + 1, width: list_w, ..inner }.intersection(inner);
        let (first, lines) = preset_list(&cx.ui.eq_preset_names, current, list.width as usize, list.height as usize, p);
        for row in 0..lines.len() {
            cx.ui.hits.push((Rect { y: list.y + row as u16, height: 1, ..list }, Hit::EqPreset(first + row)));
        }
        tui::draw_lines(f.buffer_mut(), list, &lines);
    }
    let band_w = ((faders.width as usize).saturating_sub(scale_w) / EQ_BANDS).clamp(6, 12);
    let off = (faders.width as usize).saturating_sub(scale_w + EQ_BANDS * band_w) / 2;
    let area = tui::indent(faders, off as u16);
    let bands = state.eq_bands_array();
    let selected = focused.then_some(cx.ui.eq_band.min(EQ_BANDS - 1));
    let look = if state.viz_style() == crate::state::VizStyle::Dots { crate::eq_ui::Faders::Dots } else { crate::eq_ui::Faders::Rails };
    let mut lines = crate::eq_ui::eq_body(&bands, selected, '●', look, p, band_w, slider_h);
    lines.push(selected.map_or_else(Line::default, |b| crate::eq_ui::band_readout(b, &bands[b], p)));
    let rate = match state.output_rate.load(Ordering::Relaxed) {
        0 => 48_000.0,
        r => r as f32,
    };
    let pre_db = state.eq_preamp_db();
    let headroom = cx.ui.headroom_cache.get(&bands, pre_db, rate);
    lines.push(crate::eq_ui::headroom_line(&headroom, pre_db, focused, p));
    let mut keys = eq_keys(p, focused, area.width as usize);
    light_pressed(&mut keys, cx.ui, ViewMode::Eq);
    lines.push(keys.line.clone());
    for b in 0..EQ_BANDS {
        let x = area.x + (scale_w + b * band_w) as u16;
        let band = Rect { x, y: area.y, width: band_w as u16, height: slider_h as u16 + 3 }.intersection(faders);
        if !band.is_empty() {
            cx.ui.hits.push((band, Hit::EqBand(b)));
        }
    }
    let key_row = area.y + lines.len() as u16 - 1;
    if key_row < inner.bottom() {
        keys.register(&mut cx.ui.hits, area.x, key_row);
    }
    tui::draw_lines(f.buffer_mut(), area, &lines);
}

/// The preset list's columns.
const PRESET_LIST_W: usize = 24;

/// The preset list: as many names as `rows` holds, scrolled to keep the
/// `current` one (None: a custom EQ) in view, which is lit (`▸`, bold
/// accent); and the index of the first shown.
fn preset_list(names: &[String], current: Option<usize>, w: usize, rows: usize, p: &Palette) -> (usize, Vec<Line<'static>>) {
    let at = current.unwrap_or(0);
    let first = at.saturating_sub(rows / 2).min(names.len().saturating_sub(rows));
    let lines = names
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, name)| {
            let lit = current == Some(i);
            let mut l = LineBuf::new();
            l.styled(if lit { " ▸ " } else { "   " }, p.accent).reset();
            l.styled(&crate::ansi::truncate_plain(&name.to_uppercase(), w.saturating_sub(3)), if lit { p.accent.patch(p.bold) } else { p.dim });
            l.into_line()
        })
        .collect();
    (first, lines)
}

/// How long a key's hint stays lit after the key is pressed.
pub const KEY_FLASH: std::time::Duration = std::time::Duration::from_millis(300);

/// Light the hint of the key just pressed (reverse video), for `KEY_FLASH`,
/// on the bar of `view`, the view that took it.
fn light_pressed(bar: &mut crate::hints::Bar, ui: &UiState, view: ViewMode) {
    if let Some((key, _, _)) = ui.key_flash.filter(|&(_, v, at)| v == view && at.elapsed() < KEY_FLASH) {
        bar.light(key, Style::new().add_modifier(Modifier::REVERSED));
    }
}

/// The EQ unit's key row: with the keys, the editor's; without, the way in.
fn eq_keys(p: &Palette, focused: bool, width: usize) -> crate::hints::Bar {
    const EDITING: &[(&str, &str)] = &[
        ("←→", "BAND"),
        ("↑↓", "GAIN (⇧ FINE)"),
        ("t", "TYPE"),
        (",.", "Q"),
        ("<>", "FREQ"),
        ("[]", "PRESET"),
        ("0", "RESET"),
        ("Esc", "PLAYER"),
    ];
    let look = crate::ui::key_look(ThemeKind::HiFi, p);
    let bar = if focused {
        crate::hints::bar(EDITING, &look, width.saturating_sub(2))
    } else {
        // A click on the way in is the unit itself (it takes the keys); as
        // the `e` key, it gave them and then took them straight back.
        crate::hints::bar(&[("e", "EDIT".to_string(), Some(Hit::Panel(ViewMode::Eq)))], &look, width.saturating_sub(2))
    };
    crate::hints::indent(bar, 2)
}

/// Rows below the key bar the library needs before it can dock there: five
/// tracks under its column headings, and its key row.
pub const DOCK_ROWS: u16 = 10;
/// Rows the docked lyrics need: the current line and the key row.
pub const LYRICS_DOCK_ROWS: u16 = 4;
/// The docked lyrics' widest beside the library (their narrowest is
/// `panel_cols`).
const LYRICS_MAX_W: u16 = 64;

/// Where docked panels go in a window `area` with `docked` docked: under
/// the EQ unit when there are the rows for both, else in its place
/// (`stack`); None when there are not the rows they need.
pub fn dock_region(area: Rect, docked: &[ViewMode]) -> Option<Rect> {
    stack(area, dock_need(docked, area.width)).dock
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
        ("b",   "STYLE"),
    ];
    // As many keys as the window holds: a key cut in half reads as noise.
    crate::hints::indent(crate::hints::bar(KEYS, &crate::ui::key_look(crate::theme::ThemeKind::HiFi, p), term_w.saturating_sub(2)), 2)
}

/// The dim amber of the progress rail.
const RAIL: Color = Color::Rgb(122, 94, 58);

/// The progress bar: lit braille cells meeting a rail of unlit (dim) ones,
/// the cell being filled lit one dot column at a time — half a cell a step.
/// All glyphs, no background: the partial blocks it had, on the rail's
/// colour as background, dented the cell being filled in Windows Terminal
/// (a cell's background and a block glyph are not the same height there).
fn render_solid_bar(progress: f64, width: usize, p: &Palette) -> Line<'static> {
    let sub = (progress * width as f64).max(0.0);
    let full = (sub as usize).min(width);
    // Half a cell lit: its left column (one colour a cell, so the right one
    // is dark, as a part-lit cell of the dot meter is).
    let half = full < width && sub - full as f64 >= 0.5;
    let mut s = LineBuf::new();
    s.styled(&"⣿".repeat(full), p.accent);
    if half {
        s.styled("⡇", p.accent);
    }
    // Without colour, unlit dots would read as lit: a dotted line.
    let rail = if crate::term::no_color() { '⠤' } else { '⣿' };
    s.style(Style::new().fg(RAIL)).push_str(&rail.to_string().repeat(width - full - usize::from(half)));
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
    fn the_dotted_bar_steps_by_dot_columns_and_paints_no_background() {
        // No background (Windows Terminal drew a cell's background lower
        // than the block glyphs beside it: a dent in the cell being filled).
        let p = palette(ThemeKind::HiFi);
        let text = |l: &Line| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        let lit = |l: &Line| l.spans.iter().filter(|s| s.style.fg == p.accent.fg).map(|s| s.content.as_ref()).collect::<String>();
        for (progress, shown) in [(0.51, "⣿".repeat(10)), (0.53, "⣿".repeat(10) + "⡇"), (0.55, "⣿".repeat(11))] {
            let bar = render_solid_bar(progress, 20, p);
            assert_eq!(crate::tui::Cols::cols(&bar), 20, "{progress}");
            assert!(bar.spans.iter().all(|s| s.style.bg.is_none()), "{bar:?}");
            assert_eq!(lit(&bar), shown, "{progress}");
            assert!(text(&bar).chars().all(|c| matches!(c, '⣿' | '⡇')), "{}", text(&bar));
        }
        // The rail: dim dots, every cell not lit.
        let bar = render_solid_bar(0.53, 20, p);
        let rail: String = bar.spans.iter().filter(|s| s.style.fg == Some(RAIL)).map(|s| s.content.as_ref()).collect();
        assert_eq!(rail, "⣿".repeat(9));
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
    fn the_stack_spans_the_window_and_the_eq_takes_what_is_left() {
        let area = Rect::new(0, 0, 150, 40);
        let s = stack(area, 0);
        assert_eq!(s.transport, Rect::new(0, 0, 150, TRANSPORT_H));
        assert_eq!(s.meters, Rect::new(0, TRANSPORT_H, 150, METER_H));
        let top = TRANSPORT_H + METER_H;
        // The EQ capped at 9 slider rows, the key bar under it, no dock.
        assert_eq!(s.eq, Some(Rect::new(0, top, 150, EQ_FIXED + EQ_MAX_SLIDERS)));
        assert_eq!(s.keys, Rect::new(0, top + EQ_FIXED + EQ_MAX_SLIDERS, 150, 1));
        assert_eq!(s.dock, None);
        // Docked, with rows for both: the EQ, then the panel in the rest.
        let both = stack(area, DOCK_ROWS);
        let eq = both.eq.expect("room for both");
        assert_eq!(both.dock, Some(Rect::new(0, eq.bottom() + 1, 150, 40 - eq.bottom() - 1)));
        assert!(both.dock.unwrap().height >= DOCK_ROWS);
        assert_eq!(dock_region(area, &[ViewMode::Playlist]), both.dock);
        // Too short for both: the EQ gives way, the panel goes under the meters.
        let short = stack(Rect::new(0, 0, 150, top + 1 + EQ_FIXED + 5 + DOCK_ROWS - 1), DOCK_ROWS);
        assert_eq!((short.eq, short.keys.y), (None, top));
        assert_eq!(short.dock.map(|d| d.y), Some(top + 1));
        // Too narrow for its ten faders: no EQ unit (the dialog instead).
        assert_eq!(stack(Rect::new(0, 0, EQ_MIN_W - 1, 40), 0).eq, None);
        assert!(stack(Rect::new(0, 0, EQ_MIN_W, 40), 0).eq.is_some());
        // Too short for five slider rows: no EQ unit.
        assert_eq!(stack(Rect::new(0, 0, 150, top + EQ_FIXED + 5), 0).eq, None);
        assert!(stack(Rect::new(0, 0, 150, top + EQ_FIXED + 6), 0).eq.is_some());
        // The slider rows stay odd (a true 0 dB row), the key bar fits.
        for h in 0..80 {
            for need in [0, LYRICS_DOCK_ROWS, DOCK_ROWS] {
                let s = stack(Rect::new(0, 0, 100, h), need);
                if let Some(eq) = s.eq {
                    assert_eq!((eq.height - EQ_FIXED) % 2, 1, "{h} rows");
                    assert!(eq.bottom() < h, "{h} rows: the key bar fits under it");
                }
                if let Some(d) = s.dock {
                    assert!(d.bottom() <= h && d.y > s.keys.y, "{h} rows");
                }
            }
        }
    }

    #[test]
    fn the_peak_meter_keeps_its_length_however_wide_the_window() {
        for w in [80u16, 120, 200, 400] {
            assert_eq!(meter_layout(w).segments, METER_SEGMENTS, "{w} columns");
        }
        // Wide: the signal lamps get what is left.
        let wide = meter_layout(200);
        assert_eq!(wide.vu_w + 1 + wide.signal_w + 1 + wide.vol_w, 200);
        // Without room for the lamps, they go, not the meter.
        let mid = meter_layout(60);
        assert_eq!((mid.segments, mid.signal_w, mid.vol_w), (METER_SEGMENTS, 0, VOL_W));
        // HiFi's narrowest window: the meter shrinks, the gauge stays.
        let narrow = meter_layout(32);
        assert!(narrow.segments >= 8 && narrow.segments < METER_SEGMENTS, "{narrow:?}");
        assert_eq!(narrow.vu_w + 1 + narrow.vol_w, 32);
        for w in 0..300 {
            let m = meter_layout(w);
            let used = m.vu_w + if m.signal_w > 0 { 1 + m.signal_w } else { 0 } + if m.vol_w > 0 { 1 + m.vol_w } else { 0 };
            assert!(used <= w.max(m.vu_w), "{w}: {m:?}");
        }
    }

    #[test]
    fn the_meter_is_segmented_with_ghosts_for_the_unlit() {
        let p = palette(ThemeKind::HiFi);
        let bar = vu_bar(0.5, 0.75, 40, p);
        assert_eq!(bar.cols(), 40);
        // Every cell a segment: 20 lit, the peak hold at 30, the rest ghosts.
        assert!(tui::plain(&bar).chars().all(|c| c == '▊'), "{}", tui::plain(&bar));
        let styles: Vec<Style> = bar.spans.iter().flat_map(|s| s.content.chars().map(move |_| s.style)).collect();
        assert!(styles[..20].iter().all(|&s| s == p.accent));
        assert_eq!((styles[29], styles[30], styles[31]), (p.rule, p.warn, Style::new().fg(GHOST_WARM)));
        // Amber, orange from -6 dB (0.75 on the scale), red from -3 dB
        // (0.85); the ghosts in each zone's dark colour.
        let loud = vu_bar(1.0, 1.0, 40, p);
        let lit: Vec<Style> = loud.spans.iter().flat_map(|s| s.content.chars().map(move |_| s.style)).collect();
        assert_eq!((lit[29], lit[30], lit[33], lit[34], lit[39]), (p.accent, p.warn, p.warn, p.danger, p.danger));
        assert_eq!((styles[32], styles[39]), (Style::new().fg(GHOST_WARM), Style::new().fg(GHOST_HOT)));
        assert_ne!(p.warn.fg, p.danger.fg, "red is not the orange");
        // Silence lights nothing.
        let quiet = vu_bar(0.0, 0.0, 40, p);
        assert!(quiet.spans.iter().all(|s| [Zone::Normal, Zone::Warm, Zone::Hot].iter().any(|z| s.style == z.ghost(p))));
    }

    #[test]
    fn the_meter_reads_on_its_db_scale() {
        // The positions of the scale's labels.
        for (db, frac) in [(-60.0, 0.0), (-40.0, 0.0), (-30.0, 0.2), (-20.0, 0.4), (-12.0, 0.6), (-6.0, 0.75), (-3.0, 0.85), (0.0, 1.0), (3.0, 1.0)] {
            assert!((meter_frac(db) - frac).abs() < 1e-6, "{db} dB → {}", meter_frac(db));
        }
        // Between labels, linear in dB.
        assert!((meter_frac(-9.0) - 0.675).abs() < 1e-6);
    }

    #[test]
    fn the_meter_falls_at_its_own_pace_and_holds_its_peak() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut m = PeakMeter::default();
        m.update([0.5, 1.0], at(0));
        let half = 20.0 * 0.5f32.log10();
        assert!((m.level_db[0] - half).abs() < 1e-3 && m.level_db[1].abs() < 1e-3, "rises at once: {:?}", m.level_db);
        // A tenth of a second of silence: 1.2 dB down, not to the floor.
        m.update([0.0, 0.0], at(100));
        assert!((m.level_db[1] + RELEASE_DB_S * 0.1).abs() < 1e-3, "{:?}", m.level_db);
        // The peak hold stays a second…
        for ms in (200..=1000).step_by(100) {
            m.update([0.0, 0.0], at(ms));
        }
        assert!(m.hold_db[1].abs() < 1e-3, "held: {:?}", m.hold_db);
        // …then falls at its own rate, never below the level.
        for ms in (1100..=1500).step_by(100) {
            m.update([0.0, 0.0], at(ms));
        }
        assert!(m.hold_db[1] < -1.0 && m.hold_db[1] > -HOLD_FALL_DB_S * 0.6, "{:?}", m.hold_db);
        assert!(m.hold_db.iter().zip(m.level_db).all(|(&h, l)| h >= l));
        // Not settled while anything is lit (the loop keeps drawing it at
        // the viz rate after a pause until it is: at the idle rate it fell
        // in 3 dB steps).
        assert!(!m.settled());
        for ms in (1600..=9000).step_by(100) {
            m.update([0.0, 0.0], at(ms));
        }
        assert!(m.settled(), "{:?} {:?}", m.level_db, m.hold_db);
        // Settled once dark: below the scale's -40 dB nothing is lit, and
        // waiting for -60 kept the full frame rate seconds longer.
        let dark = PeakMeter { level_db: [-45.0; 2], hold_db: [-41.0; 2], hold_left: [0.0; 2], last: None };
        assert!(dark.settled());
        let lit = PeakMeter { hold_db: [-39.0, -50.0], ..dark };
        assert!(!lit.settled());
        // The meter out of view for a minute (another theme, a dialog):
        // it starts afresh on what plays now, no peak from back then.
        m.update([1.0, 1.0], at(9100));
        m.update([0.1, 0.1], at(69_100));
        assert!(m.level_db.iter().chain(&m.hold_db).all(|&d| (d + 20.0).abs() < 1e-3), "{:?} {:?}", m.level_db, m.hold_db);
    }

    #[test]
    fn the_volume_gauge_is_two_whole_segments_at_unity() {
        let p = palette(ThemeKind::HiFi);
        let rows = |v| volume_gauge(v, 5, 7, p).iter().map(tui::plain).collect::<Vec<_>>();
        // A blank row, the three segments top first, the value.
        assert_eq!(rows(100), ["", "  ▆▆▆", "  ▆▆▆", "  ▆▆▆", "  100"]);
        let unity = volume_gauge(100, 5, 7, p);
        assert_eq!(unity[1].spans.last().unwrap().style, p.rule, "the boost segment unlit (a ghost)");
        assert_eq!(unity[2].spans.last().unwrap().style, p.accent);
        assert_eq!(unity[3].spans.last().unwrap().style, p.accent);
        // Full: the third in the warn colour, and the value.
        let loud = volume_gauge(150, 5, 7, p);
        assert_eq!(loud[1].spans.last().unwrap().style, p.warn);
        assert_eq!(loud[4].spans.last().unwrap().style, p.bold.patch(p.warn));
        // In between, a segment fills by eighths.
        assert_eq!(rows(75)[2..4], ["  ▃▃▃", "  ▆▆▆"]);
        assert_eq!(rows(5)[3], "  ▁▁▁");
        assert!(volume_gauge(0, 5, 7, p)[1..4].iter().all(|l| l.spans.last().unwrap().style == p.rule));
    }

    #[test]
    fn a_narrow_lamp_row_keeps_the_lit_lamp() {
        let p = palette(ThemeKind::HiFi);
        const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
        let all = tui::plain(&lamp_row(&RATES, 192_000, khz, 100, p));
        assert_eq!(all, " 44.1  48  88.2  96  176.4  192 ");
        let cut = lamp_row(&RATES, 192_000, khz, 12, p);
        assert_eq!(tui::plain(&cut), " 176.4  192 ");
        let lit = cut.spans.iter().find(|s| s.content.contains("192")).unwrap();
        assert!(lit.style.add_modifier.contains(Modifier::REVERSED), "lit is reverse video, not colour alone");
        // A rate off the list is added where it belongs.
        assert_eq!(tui::plain(&lamp_row(&RATES[..2], 22_050, khz, 100, p)), " 22.05  44.1  48 ");
    }

    #[test]
    fn the_signal_panel_gives_its_spare_columns_to_a_spectrum() {
        // Narrow: text only. Then the narrow spectrum, then the wide one.
        assert_eq!(signal_split(40), (38, 0));
        assert_eq!(signal_split(2 + SIGNAL_TEXT_MIN_W + 2 + SPECTRUM_MIN_W), (SIGNAL_TEXT_MIN_W, SPECTRUM_MIN_W));
        assert_eq!(signal_split(2 + SIGNAL_TEXT_MIN_W + 2 + SPECTRUM_MAX_W), (SIGNAL_TEXT_MIN_W, SPECTRUM_MAX_W));
        assert_eq!(signal_split(300), (300 - 2 - 2 - SPECTRUM_MAX_W, SPECTRUM_MAX_W));
        for w in 0..300 {
            let (t, s) = signal_split(w);
            assert!(t + if s > 0 { 2 + s } else { 0 } <= w.saturating_sub(2), "{w}");
            assert!([0, SPECTRUM_MIN_W, SPECTRUM_MAX_W].contains(&s), "{w}");
        }
    }

    #[test]
    fn the_spectrum_is_ten_segmented_octaves_on_the_eqs_centres() {
        // Every analyser band in exactly one octave; the octaves' middle
        // bands are the EQ's centres.
        let mut seen = vec![0; crate::state::SPECTRUM_BANDS];
        for i in 0..SPECTRUM_OCTAVES {
            for b in octave_bands(i) {
                seen[b] += 1;
            }
            let mid = if i == 0 { 2 } else { octave_bands(i).start + 1 };
            assert_eq!(crate::viz::ISO_CENTERS[mid], [31.5, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0][i]);
        }
        assert!(seen.iter().all(|&n| n == 1), "{seen:?}");

        let p = palette(ThemeKind::HiFi);
        let mut bands = [0.0; crate::state::SPECTRUM_BANDS];
        let mut peaks = [0.0; crate::state::SPECTRUM_BANDS];
        bands[1] = 1.0; // 25 Hz: the first octave, full
        bands[29] = 0.4; // 16 kHz: two segments of five
        peaks[29] = 0.8; // its peak hold on the fourth
        let lines = spectrum_lines(&bands, &peaks, 29, 5, p);
        let rows: Vec<String> = lines.iter().map(tui::plain).collect();
        assert!(lines.iter().all(|l| l.cols() == 29));
        assert!(rows.iter().all(|r| r.chars().all(|c| c == '▆' || c == ' ')), "segments only: {rows:?}");
        let style = |row: usize, col: usize| lines[row].spans.iter().flat_map(|s| s.content.chars().map(move |_| s.style)).nth(col).unwrap();
        assert!((0..5).all(|r| style(r, 0) == p.accent && style(r, 1) == p.accent), "the first full, amber");
        assert_eq!((style(4, 27), style(3, 27), style(2, 27), style(1, 27), style(0, 27)), (p.accent, p.accent, p.rule, p.fg, p.rule));
        // Narrower: one column a bar.
        assert!(spectrum_lines(&bands, &peaks, 19, 5, p).iter().all(|l| l.cols() == 19));
    }

    #[test]
    fn the_big_clock_is_three_rows_of_seven_segment_digits() {
        let p = palette(ThemeKind::HiFi);
        let rows: Vec<String> = big_clock("02:34", p).iter().map(tui::plain).collect();
        assert_eq!(rows, ["█▀█ ▀▀█ ▄ ▀▀█ █ █", "█ █ █▀▀   ▀▀█ ▀▀█", "█▄█ █▄▄ ▀ ▄▄█   █"]);
        assert_eq!(big_clock("1:02:34", p)[0].cols(), 23);
        // Every digit three columns, every row as wide.
        let all: Vec<usize> = big_clock("0123456789", p).iter().map(|l| l.cols()).collect();
        assert_eq!(all, [39, 39, 39]);
    }

    #[test]
    fn the_preset_list_keeps_the_current_preset_in_view_and_lit() {
        let p = palette(ThemeKind::HiFi);
        let names: Vec<String> = (0..20).map(|i| format!("Preset {i}")).collect();
        let (first, lines) = preset_list(&names, Some(12), 24, 6, p);
        let rows: Vec<String> = lines.iter().map(tui::plain).collect();
        assert_eq!(first, 9);
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[3], " ▸ PRESET 12");
        assert_eq!(rows[2], "   PRESET 11");
        // At the ends it stays full; a custom EQ lights nothing.
        assert_eq!(preset_list(&names, Some(19), 24, 6, p).0, 14);
        assert_eq!(preset_list(&names, Some(0), 24, 6, p).0, 0);
        let (_, custom) = preset_list(&names, None, 24, 6, p);
        assert!(custom.iter().all(|l| !tui::plain(l).contains('▸')));
        // A short list in a tall box.
        assert_eq!(preset_list(&names[..3], Some(2), 24, 6, p).1.len(), 3);
    }

    #[test]
    fn the_key_just_pressed_lights_its_hint() {
        let p = palette(ThemeKind::HiFi);
        let mut ui = UiState::new(vec![], crate::metadata::MetadataCache::new(0));
        let lit = |bar: &crate::hints::Bar| -> String {
            bar.line.spans.iter().filter(|s| s.style.add_modifier.contains(Modifier::REVERSED)).map(|s| s.content.to_string()).collect()
        };
        let mut bar = hifi_marquee_keys(p, 200);
        light_pressed(&mut bar, &ui, ViewMode::Player);
        assert_eq!(lit(&bar), "", "nothing pressed");
        ui.key_flash = Some((crossterm::event::KeyCode::Char('e'), ViewMode::Player, std::time::Instant::now()));
        let mut bar = hifi_marquee_keys(p, 200);
        light_pressed(&mut bar, &ui, ViewMode::Player);
        assert_eq!(lit(&bar), "e EQ", "the key and its label");
        assert_eq!(bar.line.cols(), hifi_marquee_keys(p, 200).line.cols(), "nothing moves");
        // Two keys in one hint: only the one pressed.
        ui.key_flash = Some((crossterm::event::KeyCode::Right, ViewMode::Player, std::time::Instant::now()));
        let mut bar = hifi_marquee_keys(p, 200);
        light_pressed(&mut bar, &ui, ViewMode::Player);
        assert_eq!(lit(&bar), "→");
        // A key another view took (↓ in a focused list) lights nothing here.
        ui.key_flash = Some((crossterm::event::KeyCode::Down, ViewMode::Playlist, std::time::Instant::now()));
        let mut bar = hifi_marquee_keys(p, 200);
        light_pressed(&mut bar, &ui, ViewMode::Player);
        assert_eq!(lit(&bar), "");
        // Gone after its moment.
        ui.key_flash = Some((crossterm::event::KeyCode::Char('e'), ViewMode::Player, std::time::Instant::now() - KEY_FLASH));
        let mut bar = hifi_marquee_keys(p, 200);
        light_pressed(&mut bar, &ui, ViewMode::Player);
        assert_eq!(lit(&bar), "");
    }

    #[test]
    fn the_clock_takes_the_tracks_shape_from_the_start() {
        assert_eq!(clock_text(5.0, 300.0), "00:05");
        // Over an hour (a symphony): h:mm:ss from the first second, so the
        // clock is as wide at 0:05 as at 1:05:00.
        assert_eq!(clock_text(5.0, 99.0 * 60.0), "0:00:05");
        assert_eq!(clock_text(59.0 * 60.0 + 59.0, 5940.0), "0:59:59");
        assert_eq!(clock_text(3600.0, 5940.0), "1:00:00");
        let p = palette(ThemeKind::HiFi);
        let w = |t: f64| big_clock(&clock_text(t, 5940.0), p)[0].cols();
        assert_eq!(w(5.0), w(5000.0));
        // Ten hours and more: the hours padded to the total's digits.
        assert_eq!(clock_text(5.0, 11.0 * 3600.0), "00:00:05");
        assert_eq!(clock_text(10.0 * 3600.0, 11.0 * 3600.0), "10:00:00");
    }

    fn cells(l: &Line<'static>) -> Vec<(char, Style)> {
        l.spans.iter().flat_map(|s| s.content.chars().map(move |c| (c, s.style))).collect()
    }

    #[test]
    fn the_dot_meter_has_two_steps_a_cell() {
        let p = palette(ThemeKind::HiFi);
        // 41 of 80 dot columns: 20 cells full, the 21st its left column.
        let c = cells(&vu_dots(41.0 / 80.0, 0.0, 40, p));
        assert_eq!(c.len(), 40);
        assert!(c[..20].iter().all(|&(g, s)| g == '⣿' && s == p.accent));
        assert_eq!(c[20], ('⡇', p.accent));
        assert_eq!(c[21], ('⣿', p.rule), "a ghost");
        // The peak hold: one bright dot column; the hot zones keep their colours.
        let c = cells(&vu_dots(0.25, 61.0 / 80.0, 40, p));
        assert_eq!(c[30], ('⢸', p.warn));
        let c = cells(&vu_dots(1.0, 1.0, 40, p));
        assert_eq!((c[29].1, c[30].1, c[34].1), (p.accent, p.warn, p.danger));
        // A hold at full scale is the last dot column (it was past the end).
        let c = cells(&vu_dots(0.25, 1.0, 40, p));
        assert_eq!(c[39], ('⢸', p.danger));
        let c = cells(&vu_bar(0.25, 1.0, 40, p));
        assert_eq!(c[39], ('▊', p.danger), "the bars' hold too");
        // Silence: ghosts only.
        assert!(cells(&vu_dots(0.0, 0.0, 40, p)).iter().all(|&(g, _)| g == '⣿'));
    }

    #[test]
    fn dot_columns_light_from_the_bottom_with_a_bright_hold() {
        // Three cells, 12 dots: 5 lit, the hold at 10.
        let col: Vec<(u8, bool, bool)> = (0..3).map(|r| column_cell(3, r, 5, 10)).collect();
        assert_eq!(col[2], (0xFF, true, false), "the bottom cell full");
        assert_eq!(col[1], (dot_rows(1), true, false), "one dot row above it");
        assert_eq!(col[0], (dot_rows(2) & !dot_rows(1), false, true), "the hold: one dot row, alone");
        assert_eq!(braille(dot_rows(1)), '⣀');
        assert_eq!(braille(dot_columns(1)), '⡇');
        // The volume: 100% is two cells exactly, the top one past it warn.
        let p = palette(ThemeKind::HiFi);
        let v = volume_dots(100, 5, 7, p);
        assert_eq!(cells(&v[1]).last().copied(), Some(('⣿', p.rule)));
        assert_eq!(cells(&v[2]).last().copied(), Some(('⣿', p.accent)));
        assert_eq!(cells(&v[3]).last().copied(), Some(('⣿', p.accent)));
        assert_eq!(cells(&volume_dots(150, 5, 7, p)[1]).last().map(|c| c.1), Some(p.warn));
        assert_eq!(cells(&volume_dots(75, 5, 7, p)[2]).last().copied(), Some(('⣤', p.accent)), "75%: six dots");
        // The spectrum: twenty levels in five rows.
        let mut bands = [0.0; crate::state::SPECTRUM_BANDS];
        bands[1] = 0.5;
        let s = spectrum_dots(&bands, &[0.0; crate::state::SPECTRUM_BANDS], 29, 5, p);
        let first: Vec<char> = s.iter().map(|l| cells(l)[0].0).collect();
        assert_eq!(first, ['⣿', '⣿', '⣤', '⣿', '⣿']);
        assert_eq!(cells(&s[0])[0].1, p.rule, "above the level: a ghost");
    }
}
