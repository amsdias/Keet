//! Classic theme renderer: the terminal's own ground and text, three colours
//! that each mean one thing (theme.rs), and a header drawn with the frame —
//! the cover beside what is playing, how it is playing and where.
//!
//! Every view shares the top block: header (cover + info), transport line,
//! status row. Below it, a titled rule names the section (the visualisation,
//! the playlist, the lyrics) with that section's hints, then the body, a
//! message line and a key bar for the view.

use crate::tui::Cols;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;

use crate::ansi::truncate_plain;
use crate::state::{Hit, PlayerState, RepeatMode, RgMode, UiState, VizMode, VizStyle};
use crate::theme::{palette, Palette, ThemeKind};
use crate::tui::{self, fit_segments, LineBuf};
use crate::ui::{format_time, Ctx};
use crate::viz::{viz_body_rows, viz_rows_available, viz_top_pad, StatsMonitor};

/// Columns the cover needs beside a readable info column (2 + 20 + 2 + 36).
const COVER_MIN_W: usize = 60;
/// Rows the player's body needs below the header before the header may
/// grow: transport + status + rule, four rows of visualisation and the
/// footer.
const PLAYER_BODY_MIN: usize = 2 + 1 + 4 + FOOTER_ROWS;
/// Below this height the gap row under the header goes too.
const GAP_MIN_H: usize = 16;

/// Which info lines survive a header of `budget` rows without the cover, by
/// their index in `info_lines`: the title always; then, dropped first to
/// last, the next track, the play modes, the device, the format, the verdict
/// and the artist. Shown in their own order.
fn compact_info(info: Vec<Line<'static>>, budget: usize) -> Vec<Line<'static>> {
    const DROP_ORDER: [usize; 6] = [9, 7, 6, 3, 4, 1];
    let mut keep: Vec<bool> = info.iter().map(|l| l.cols() > 0).collect();
    keep[0] = true;
    for &i in &DROP_ORDER {
        if keep.iter().filter(|&&k| k).count() <= budget.max(1) {
            break;
        }
        keep[i] = false;
    }
    info.into_iter().zip(keep).filter(|(_, k)| *k).map(|(l, _)| l).collect()
}
/// Rows kept below a body: message line, key bar, one row of slack.
const FOOTER_ROWS: usize = 3;

fn khz(rate: u32) -> String {
    let k = rate as f32 / 1000.0;
    if k.fract() == 0.0 { format!("{k:.0} kHz") } else { format!("{k:.1} kHz") }
}

/// The ` · ` between items, dim.
fn dot_sep(p: &Palette) -> Line<'static> {
    tui::line(" · ", p.dim)
}

/// "label value" with the label dim; the value dim too when it reads "off".
fn pair(p: &Palette, label: &str, value: &str) -> Line<'static> {
    let v = if value == "off" { p.dim } else { p.fg };
    let mut l = LineBuf::new();
    l.styled(label, p.dim).reset().push(' ').styled(value, v);
    l.into_line()
}

/// A key bar: as many `key label` items as fit, whole.
/// (Each key clickable: register the bar where it is drawn.)
fn key_bar(p: &Palette, keys: &[(&str, &str)], width: usize) -> crate::hints::Bar {
    let look = crate::ui::key_look(crate::theme::ThemeKind::Classic, p);
    crate::hints::indent(crate::hints::bar(keys, &look, width.saturating_sub(2)), 2)
}

/// `── Title ───────── hint ──` across the window; the hint goes first when
/// the window is too narrow for both.
fn titled_rule(p: &Palette, title: &str, hint: &str, width: usize) -> Line<'static> {
    let fixed = 2 + 3 + tui::width(title) + 1 + 2;
    let with_hint = fixed + tui::width(hint) + 2;
    let (hint, fill) = if !hint.is_empty() && width >= with_hint + 3 {
        (Some(format!(" {hint} ")), width - with_hint)
    } else {
        (None, width.saturating_sub(fixed))
    };
    let mut l = LineBuf::new();
    l.push_str("  ").styled("── ", p.rule).styled(title, p.bold).styled(&format!(" {}", "─".repeat(fill)), p.rule);
    if let Some(h) = hint {
        l.styled(&h, p.dim);
    }
    l.styled("──", p.rule);
    l.into_line()
}

/// The track that plays next, as the producer will be told (`ui::next_pick`):
/// files already found unplayable are passed over, so the line never names
/// one that has been reported as skipped.
fn next_index(ui: &UiState, state: &PlayerState, playlist: &[PathBuf]) -> Option<usize> {
    let len = playlist.len();
    if state.repeat_mode() == RepeatMode::One {
        return Some(ui.current);
    }
    let after = (ui.current + 1..len).find(|&i| !state.is_unplayable(&playlist[i]));
    match after {
        Some(i) => Some(i),
        None if state.repeat_mode() == RepeatMode::All => (0..len).find(|&i| !state.is_unplayable(&playlist[i])),
        None => None,
    }
}

fn track_title(ui: &UiState, idx: usize, playlist: &[PathBuf], fallback: &str) -> String {
    ui.metadata_cache
        .title(idx)
        .or_else(|| playlist.get(idx).map(|p| ui.metadata_cache.display_name(idx, p)))
        .unwrap_or_else(|| fallback.to_string())
}

/// The info column beside the cover: title, artist · album, format, the
/// bit-perfect verdict, device, play modes, what comes next. Blank lines are
/// the spacing rows the cover's height leaves room for.
#[allow(clippy::too_many_arguments)]
fn info_lines(
    state: &PlayerState,
    ui: &UiState,
    name: &str,
    ext: &str,
    fx_name: &str,
    cf_name: &str,
    playlist: &[PathBuf],
    p: &Palette,
    width: usize,
) -> Vec<Line<'static>> {
    let idx = state.current_track.load(Ordering::Relaxed);
    let title = track_title(ui, idx, playlist, name);
    let (artist, album) = ui.metadata_cache.artist_album(idx);

    let title_line = tui::line(truncate_plain(&title, width), p.bold);
    let mut who = Vec::new();
    if let Some(a) = artist {
        who.push(tui::line(a, p.accent));
    }
    if let Some(al) = album {
        who.push(tui::line(al, p.dim));
    }
    let who_line = fit_segments(&who, &dot_sep(p), width);

    // FLAC 24-bit · 96 kHz → 48 kHz · stereo · 6:12
    let src_rate = state.sample_rate.load(Ordering::Relaxed) as u32;
    let out_rate = state.output_rate.load(Ordering::Relaxed) as u32;
    let bits = state.bits_per_sample.load(Ordering::Relaxed);
    let ext_up = ext.to_uppercase();
    // 0 = a lossy codec (AAC in .m4a included): no bit depth to show.
    let codec = if bits > 0 { format!("{ext_up} {bits}-bit") } else { ext_up };
    let rate = if src_rate == out_rate || out_rate == 0 {
        khz(src_rate)
    } else {
        format!("{} → {}", khz(src_rate), khz(out_rate))
    };
    let channels = match state.channels.load(Ordering::Relaxed) {
        1 => "mono".to_string(),
        2 => "stereo".to_string(),
        n => format!("{n} ch"),
    };
    let format_items = vec![
        tui::line(codec, p.fg),
        tui::line(rate, p.dim),
        tui::line(channels, p.dim),
        tui::line(format_time(state.total_secs()), p.dim),
    ];
    let format_line = fit_segments(&format_items, &dot_sep(p), width);

    let verdict = crate::signal::verdict(&crate::signal::PathInputs::from_state(state, fx_name, cf_name));
    let verdict_line = match crate::signal::fitted(&verdict, " · ", width) {
        Some((ok, text)) => tui::line(text, if ok { p.accent } else { p.warn }),
        None => Line::default(),
    };

    // ● FiiO KA17 · exclusive · 96 kHz / 24-bit
    let exclusive = state.exclusive.load(Ordering::Relaxed);
    let out_bits = state.output_bits.load(Ordering::Relaxed);
    let mode = match (exclusive, out_bits) {
        (true, 0) => format!("exclusive · {}", khz(out_rate)),
        (true, b) => format!("exclusive · {} / {b}-bit", khz(out_rate)),
        (false, _) => format!("shared · {}", khz(out_rate)),
    };
    let device = if ui.device_name.is_empty() { "output" } else { ui.device_name.as_str() };
    // The mode goes whole or not at all: when it would leave the device name
    // under 8 columns, the line is the name alone (the window edge would
    // otherwise cut the mode text mid-word).
    let name_room = width.saturating_sub(4 + tui::width(&mode) + 3);
    let mut device_line = LineBuf::new();
    device_line.styled("●", p.accent).reset().push(' ');
    if name_room >= 8.min(tui::width(device)) && name_room > 0 {
        device_line.push_str(&truncate_plain(device, name_room)).styled(&format!(" · {mode}"), p.dim);
    } else {
        device_line.push_str(&truncate_plain(device, width.saturating_sub(4)));
    }

    // shuffle ○ off   repeat ● all   xfade 4 s   rg album   hq
    let lamp = |l: &mut LineBuf, on: bool| {
        if on { l.styled("●", p.accent) } else { l.styled("○", p.dim) };
    };
    let repeat = state.repeat_mode();
    let xfade = state.crossfade_secs.load(Ordering::Relaxed);
    let mut shuffle = LineBuf::new();
    shuffle.styled("shuffle", p.dim).reset().push(' ');
    lamp(&mut shuffle, ui.shuffle);
    shuffle.reset().push_str(if ui.shuffle { " on" } else { " off" });
    let mut rep = LineBuf::new();
    rep.styled("repeat", p.dim).reset().push(' ');
    lamp(&mut rep, repeat != RepeatMode::Off);
    rep.reset().push(' ').push_str(match repeat { RepeatMode::Off => "off", RepeatMode::All => "all", RepeatMode::One => "one" });
    let mut modes = vec![
        shuffle.into_line(),
        rep.into_line(),
        pair(p, "xfade", &if xfade == 0 { "off".to_string() } else { format!("{xfade} s") }),
        pair(p, "rg", match state.rg_mode() { RgMode::Track => "track", RgMode::Album => "album", RgMode::Off => "off" }),
    ];
    if ui.hq_resampler {
        modes.push(tui::line("hq resampler", p.dim));
    }
    let modes_line = fit_segments(&modes, &Line::raw("   "), width);

    // track 4 of 11 · next Glasswork — Ines Varela
    let total = state.total_tracks.load(Ordering::Relaxed);
    let mut next_items = vec![tui::line(format!("track {} of {total}", idx + 1), p.dim)];
    match next_index(ui, state, playlist) {
        Some(n) => {
            let mut l = LineBuf::new();
            l.styled("next", p.dim).reset().push(' ').push_str(&track_title(ui, n, playlist, ""));
            if let Some(a) = ui.metadata_cache.artist_album(n).0 {
                l.styled(&format!(" — {a}"), p.dim);
            }
            if ui.enqueue_count > 0 {
                l.styled(" (queued)", p.dim);
            }
            next_items.push(l.into_line());
        }
        None => next_items.push(tui::line("last track", p.dim)),
    }
    let next_line = fit_segments(&next_items, &dot_sep(p), width);

    vec![
        title_line, who_line, Line::default(), format_line, verdict_line, Line::default(),
        device_line.into_line(), modes_line, Line::default(), next_line,
    ]
}

/// `▶ 02:41  ━━━━━━━━●──────────  06:12  −3:31`
/// The line, and the progress bar's columns on it (`start..start + width`)
/// when there is room for one.
fn transport_line(state: &PlayerState, p: &Palette, width: usize) -> (Line<'static>, Option<(usize, usize)>) {
    let (t, total) = (state.time_secs(), state.total_secs());
    let (icon, icon_style) = if state.is_paused() { ("⏸", p.fg) } else { ("▶", p.accent) };
    let mut l = LineBuf::new();
    l.push_str("  ").styled(icon, icon_style).reset().push(' ').styled(&format_time(t), p.bold).reset();
    let right_w = 2 + format_time(total).len() + 2 + 1 + format_time((total - t).max(0.0)).len();
    let bar_w = width.saturating_sub(l.width() + 2 + right_w);
    if bar_w < 4 {
        l.push_str(&format!(" / {}", format_time(total)));
        return (l.into_line(), None);
    }
    l.push_str("  ");
    let bar_x = l.width();
    let progress = if total > 0.0 { (t / total).clamp(0.0, 1.0) } else { 0.0 };
    let filled = (progress * (bar_w - 1) as f64).round() as usize;
    l.styled(&format!("{}●", "━".repeat(filled)), p.accent).styled(&"─".repeat(bar_w - 1 - filled), p.rule);
    l.reset().push_str(&format!("  {}  ", format_time(total))).styled(&format!("−{}", format_time((total - t).max(0.0))), p.dim);
    (l.into_line(), Some((bar_x, bar_w)))
}

/// `vol 82%   bal C   eq Custom   fx off   …   clip ○` — the clip lamp ends
/// the row whatever the width (it is a status light, not an item).
fn status_line(
    state: &PlayerState,
    p: &Palette,
    eq_preset: &crate::eq::EqPreset,
    fx_name: &str,
    cf_name: &str,
    stats: &mut StatsMonitor,
    width: usize,
) -> Line<'static> {
    let buf = state.buffer_level.load(Ordering::Relaxed);
    let cap = state.ring_capacity.load(Ordering::Relaxed).max(1);
    stats.update_buf(buf as f32 / cap as f32 * 100.0);

    let bal = state.balance_value();
    let bal = match bal {
        0 => "C".to_string(),
        b if b < 0 => format!("L{}%", -b),
        b => format!("R{b}%"),
    };
    let eq = if state.is_eq_custom() {
        "Custom".to_string()
    } else if eq_preset.name == "Flat" && eq_preset.preamp.abs() < 0.01 {
        "off".to_string()
    } else {
        eq_preset.name.clone()
    };
    let off_if = |name: &str, none: &str| if name == none { "off".to_string() } else { name.to_string() };
    let mut items = vec![
        pair(p, "vol", &format!("{}%", state.volume.load(Ordering::Relaxed))),
        pair(p, "bal", &bal),
        pair(p, "eq", &eq),
        pair(p, "fx", &off_if(fx_name, "None")),
        pair(p, "xfeed", &off_if(cf_name, "Off")),
        pair(p, "fader", if state.is_pre_fader() { "pre" } else { "post" }),
        pair(p, "buf", &format!("{}%", stats.smoothed_buf_pct as u32)),
    ];
    if state.show_stats() {
        items.push(pair(p, "cpu", &format!("{:.1}%", stats.cpu_usage)));
        items.push(pair(p, "mem", &format!("{:.0}M", stats.memory_mb)));
    }
    let xruns = state.xrun_count.load(Ordering::Relaxed);
    if xruns > 0 {
        items.push(tui::line(format!("{xruns} dropouts"), p.warn));
    }
    // Shape as well as colour: ● clipping, ○ idle.
    let mut lamp = LineBuf::new();
    lamp.push_str("   ").styled("clip", p.dim).reset().push(' ');
    if state.is_clipping() {
        lamp.styled("●", p.danger);
    } else {
        lamp.styled("○", p.good);
    }
    let lamp = lamp.into_line();
    let room = width.saturating_sub(2 + lamp.cols());
    let mut l = LineBuf::new();
    l.push_str("  ").append(fit_segments(&items, &Line::raw("   "), room)).append(lamp);
    l.into_line()
}

/// The block every Classic view starts with: header (cover + info), a gap,
/// the transport line and the status row. Draws it from the top of the
/// window and returns how many rows it took.
fn draw_top(f: &mut Frame, cx: &mut Ctx, p: &Palette, budget: usize) -> usize {
    let area = f.area();
    let (term_w, term_h) = (area.width as usize, area.height as usize);
    let state = cx.state;
    let size = crate::cover::CoverSize::CLASSIC;
    // The cover comes with its full ten rows or not at all: an image taller
    // than the space left would push the list and the key bar off the bottom.
    let cover_on = cx.ui.cover_enabled && term_w >= COVER_MIN_W && budget >= size.rows as usize;
    let mut y = 0;

    if cover_on {
        let info_w = term_w.saturating_sub(2 + size.cols as usize + 2 + 1);
        let info = info_lines(state, cx.ui, cx.name, cx.ext, cx.fx_name, cx.cf_name, cx.playlist, p, info_w);
        // Emit-on-change for images that stay put (Kitty, iTerm2, Sixel):
        // re-sending one 20×/s is the cost the spectrogram rule avoids.
        let ui = &mut *cx.ui;
        let repaint = cx.repaint || !ui.cover_block_intact || ui.cover_dirty_frame;
        let slot = Rect { x: area.x + 2, y: area.y, width: size.cols as u16, height: size.rows as u16 }.intersection(area);
        match crate::cover::draw(ui.cover.as_ref(), size, repaint) {
            crate::cover::CoverDraw::Cells(lines) => tui::draw_lines(f.buffer_mut(), slot, &lines),
            crate::cover::CoverDraw::Image(payload) => tui::place_image(tui::ImageSlot::Cover, slot, payload),
        }
        ui.cover_block_intact = true;
        ui.cover_dirty_frame = false;
        let info_area = Rect { x: area.x + 2 + size.cols as u16 + 2, y: area.y, width: info_w as u16, height: size.rows as u16 }.intersection(area);
        tui::draw_lines(f.buffer_mut(), info_area, &info);
        y += size.rows as usize;
    } else {
        if cx.ui.cover_block_intact
            && matches!(crate::cover::detect_protocol(), crate::cover::GraphicsProtocol::Kitty)
        {
            // A Kitty image is an overlay: drawing text over its cells leaves it.
            crate::term::out!("{}", crate::cover::kitty_clear_escape());
        }
        cx.ui.cover_block_intact = false;
        let info_w = term_w.saturating_sub(3);
        // Without the cover's height to fill, the spacing rows go, and a
        // short window keeps only what fits (compact_info).
        let info = info_lines(state, cx.ui, cx.name, cx.ext, cx.fx_name, cx.cf_name, cx.playlist, p, info_w);
        let info = compact_info(info, budget);
        tui::draw_lines(f.buffer_mut(), tui::indent(tui::rows(area, 0, info.len()), 2), &info);
        y += info.len();
    }
    if term_h >= GAP_MIN_H {
        y += 1;
    }
    let (transport, bar) = transport_line(state, p, term_w);
    if let Some((x, w)) = bar {
        let at = Rect { x: area.x + x as u16, y: area.y + y as u16, width: w as u16, height: 1 }.intersection(area);
        cx.ui.hits.push((at, Hit::Seek));
    }
    let status = status_line(state, p, cx.eq_preset, cx.fx_name, cx.cf_name, cx.stats, term_w);
    tui::draw_lines(f.buffer_mut(), tui::rows(area, y, 2), &[transport, status]);
    y + 2
}

fn viz_title(mode: VizMode) -> &'static str {
    match mode {
        VizMode::None => "",
        VizMode::VuMeter => "VU meter",
        VizMode::SpectrumHorizontal | VizMode::SpectrumVertical => "Spectrum",
        VizMode::Oscilloscope => "Oscilloscope",
        VizMode::Lissajous => "Vectorscope",
        VizMode::Polar => "Polar spectrum",
        VizMode::Galaxy => "Galaxy",
        VizMode::Cymatics => "Cymatics",
        VizMode::Spectrogram => "Spectrogram",
        VizMode::SpectrogramAnalysis => "Analysis",
    }
}

fn viz_next_name(mode: VizMode) -> &'static str {
    match mode.next() {
        VizMode::None => "off",
        VizMode::VuMeter => "VU meter",
        VizMode::SpectrumHorizontal => "spectrum",
        VizMode::SpectrumVertical => "spectrum ↕",
        VizMode::Oscilloscope => "oscilloscope",
        VizMode::Lissajous => "vectorscope",
        VizMode::Polar => "polar",
        VizMode::Galaxy => "galaxy",
        VizMode::Cymatics => "cymatics",
        VizMode::Spectrogram => "spectrogram",
        VizMode::SpectrogramAnalysis => "analysis",
    }
}

/// Most used first: a narrow window drops keys from the end. The play modes
/// and presets the header and status row show are all here.
const PLAYER_KEYS: &[(&str, &str)] = &[
    ("␣", "pause"), ("?", "keys"), ("←→", "seek"), ("↑↓", "track"), ("+−", "vol"),
    ("q", "quit"), ("v", "viz"), ("e", "eq"), ("l", "list"), ("y", "lyrics"), ("z", "shuffle"),
    ("⇧R", "repeat"), ("x", "fx"), ("c", "xfeed"), ("⇧F", "full"), ("t", "theme"),
];


/// Classic's renderer for the player, playlist and lyrics views.
pub fn draw(f: &mut Frame, cx: &mut Ctx) {
    let p = palette(ThemeKind::Classic);
    let area = f.area();
    let (term_w, term_h) = (area.width as usize, area.height as usize);
    // (Always the player view here: `ui::draw_view` draws it as such under
    // every panel and dialog.)
    let fullscreen = cx.state.viz_fullscreen();

    // Image emit-on-change bookkeeping: cleared every frame, re-asserted only
    // by the analysis branch. Any frame that does not reach it may draw over
    // the block, so the next analysis render must re-emit.
    let block_was_intact = cx.ui.spectro_block_intact;
    cx.ui.spectro_block_intact = false;

    let top_n = if fullscreen {
        // Full window: the header and its cover go; one line carries the
        // song, the clock and the way back out.
        if cx.ui.cover_block_intact
            && matches!(crate::cover::detect_protocol(), crate::cover::GraphicsProtocol::Kitty)
        {
            crate::term::out!("{}", crate::cover::kitty_clear_escape());
        }
        cx.ui.cover_block_intact = false;
        let state = cx.state;
        let track = state.current_track.load(Ordering::Relaxed) + 1;
        let total = state.total_tracks.load(Ordering::Relaxed);
        let (icon, icon_style) = if state.is_paused() { ("⏸", p.fg) } else { ("▶", p.accent) };
        let mut head = LineBuf::new();
        head.styled(&format!("[{track}/{total}]"), p.dim).reset().push(' ').styled(icon, icon_style).reset().push(' ');
        head.styled(&truncate_plain(cx.name, 35), p.bold).reset().push_str("  ");
        let room = term_w.saturating_sub(head.width());
        let items: Vec<Line<'static>> = crate::ui::fullscreen_items(cx.track_info, state.time_secs(), state.total_secs())
            .into_iter()
            .map(|s| tui::line(s, p.dim))
            .collect();
        head.append(fit_segments(&items, &tui::line("  •  ", p.dim), room));
        tui::draw_lines(f.buffer_mut(), area, &[head.into_line()]);
        1
    } else {
        // The header takes what the view's body leaves (see PLAYER_BODY_MIN).
        let gap = usize::from(term_h >= GAP_MIN_H);
        let budget = term_h.saturating_sub(PLAYER_BODY_MIN + gap);
        draw_top(f, cx, p, budget)
    };

    player_body(f, cx, p, block_was_intact, top_n, fullscreen);
}

fn player_body(f: &mut Frame, cx: &mut Ctx, p: &Palette, block_was_intact: bool, top_n: usize, fullscreen: bool) {
    let area = f.area();
    let (term_w, term_h) = (area.width as usize, area.height as usize);
    let state = cx.state;
    let viz_mode = state.viz_mode();
    let viz_style = state.viz_style();
    let extras = state.viz_extras();
    // The section rule stays even with the viz off, so nothing below moves.
    let ruled = !fullscreen;
    let rows_above = top_n + usize::from(ruled);
    // Full window keeps only the message row and a row of slack below.
    let below = if fullscreen { 2 } else { FOOTER_ROWS };
    let avail = viz_rows_available(term_h, rows_above, below);
    let body = viz_body_rows(viz_mode, avail, fullscreen, extras);
    // Outside full window the block gets every row the window has spare,
    // each mode centred in it: the message line and key bar sit at the
    // window's bottom (as in the other themes) and stay put as `v` cycles.
    // Full window centres in the whole space instead.
    let slot = if fullscreen { body } else { avail };
    let pad = if fullscreen { viz_top_pad(avail, body, true) } else { slot.saturating_sub(body) / 2 };

    let mut y = top_n;
    if ruled {
        let rule = if viz_mode == VizMode::None {
            titled_rule(p, "Visualization off", &format!("v {}", viz_next_name(viz_mode)), term_w)
        } else {
            let style = if viz_mode == VizMode::SpectrogramAnalysis {
                if matches!(viz_style, VizStyle::Dots) { "linear" } else { "log" }
            } else {
                match viz_style { VizStyle::Dots => "bars", VizStyle::Bars => "dots" }
            };
            let hint = format!("v {} · b {style}", viz_next_name(viz_mode));
            titled_rule(p, viz_title(viz_mode), &hint, term_w)
        };
        tui::draw_lines(f.buffer_mut(), tui::rows(area, y, 1), &[rule]);
        y += 1;
    }
    if fullscreen {
        let block = tui::rows(area, y + pad, body);
        crate::ui::draw_viz(f, block, cx, block_was_intact);
    } else {
        // The slot is shared with the docked panels (`ui::dock`); the
        // visualization is centred in its part of it.
        let region = tui::indent(tui::rows(area, y, slot), 2);
        let dock = crate::ui::dock(cx.ui, viz_mode != VizMode::None, region);
        if let Some(r) = dock.viz {
            let r = Rect { x: area.x, width: r.right() - area.x, ..r };
            let body = viz_body_rows(viz_mode, r.height as usize, false, extras);
            let block = tui::rows(r, (r.height as usize).saturating_sub(body) / 2, body);
            crate::ui::draw_viz(f, block, cx, block_was_intact);
        }
        crate::ui::draw_docked(f, cx, &dock.panels);
    }
    if fullscreen {
        crate::ui::draw_status_line(f, cx.ui, tui::rows(area, y + pad + body, 1), p);
        return;
    }
    y += slot;
    let keys = key_bar(p, PLAYER_KEYS, term_w);
    keys.register(&mut cx.ui.hits, area.x, area.y + y as u16 + 1);
    // The message line keeps its row when empty, so the key bar does not
    // jump when a message comes and goes.
    crate::ui::draw_status_line(f, cx.ui, tui::rows(area, y, 1), p);
    tui::draw_lines(f.buffer_mut(), tui::rows(area, y + 1, 1), &[keys.line]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pal() -> &'static Palette {
        palette(ThemeKind::Classic)
    }

    fn visible_len(l: &Line) -> usize {
        crate::tui::Cols::cols(l)
    }

    #[test]
    fn titled_rules_span_the_window_and_drop_the_hint_first() {
        for w in [40, 80, 100, 160] {
            let r = titled_rule(pal(), "Spectrum", "v next · b dots", w);
            assert_eq!(visible_len(&r), w, "{w}: {r:?}");
        }
        let narrow = titled_rule(pal(), "Spectrum", "a very long hint that cannot fit", 40);
        assert!(!tui::plain(&narrow).contains("hint"), "{narrow:?}");
        assert_eq!(visible_len(&narrow), 40);
    }

    #[test]
    fn key_bars_end_on_a_whole_key() {
        let bar = tui::plain(&key_bar(pal(), PLAYER_KEYS, 30).line);
        assert!(tui::width(&bar) <= 30, "{bar:?}");
        assert!(bar.trim_end().ends_with("seek") || bar.trim_end().ends_with("track"), "{bar:?}");
    }

    #[test]
    fn the_transport_line_fills_the_window() {
        let st = PlayerState::new();
        for w in [20, 60, 100, 200] {
            let (line, _) = transport_line(&st, pal(), w);
            assert!(visible_len(&line) <= w, "{w}: {line:?}");
        }
        assert_eq!(visible_len(&transport_line(&st, pal(), 100).0), 100);
    }

    #[test]
    fn the_status_row_keeps_its_clip_lamp_at_any_width() {
        let st = PlayerState::new();
        let eq = crate::eq::builtin_presets().remove(0);
        let mut stats = StatsMonitor::new();
        for w in [30, 60, 120] {
            let line = status_line(&st, pal(), &eq, "None", "Off", &mut stats, w);
            let plain = tui::plain(&line);
            assert!(tui::width(&plain) <= w, "{w}: {plain:?}");
            assert!(plain.trim_end().ends_with("clip ○"), "{w}: {plain:?}");
        }
    }

    const VIZ_MODES: [VizMode; 10] = [
        VizMode::VuMeter, VizMode::SpectrumHorizontal, VizMode::SpectrumVertical, VizMode::Oscilloscope,
        VizMode::Lissajous, VizMode::Polar, VizMode::Spectrogram, VizMode::SpectrogramAnalysis, VizMode::Galaxy,
        VizMode::Cymatics,
    ];

    #[test]
    fn every_mode_fits_the_rows_the_window_has_spare() {
        for avail in [0, 3, 10, 20, 40, 80] {
            for extras in [false, true] {
                for m in VIZ_MODES {
                    assert!(viz_body_rows(m, avail, false, extras) <= avail, "{m:?} past the window at {avail}");
                }
            }
        }
    }

    #[test]
    fn a_short_header_keeps_the_title_and_drops_the_rest_in_order() {
        let info: Vec<Line<'static>> = ["title", "artist", "", "format", "verdict", "", "device", "modes", "", "next"]
            .map(Line::raw)
            .to_vec();
        let kept = |info: Vec<Line<'static>>, budget| compact_info(info, budget).iter().map(tui::plain).collect::<Vec<_>>();
        assert_eq!(kept(info.clone(), 10), ["title", "artist", "format", "verdict", "device", "modes", "next"]);
        assert_eq!(kept(info.clone(), 5), ["title", "artist", "format", "verdict", "device"]);
        assert_eq!(kept(info.clone(), 2), ["title", "artist"]);
        assert_eq!(kept(info.clone(), 0), ["title"], "the title always stays");
        // Shared mode has no verdict: one fewer line to begin with.
        let mut shared = info;
        shared[4] = Line::default();
        assert_eq!(kept(shared, 4), ["title", "artist", "format", "device"]);
    }

    #[test]
    fn the_device_line_shows_its_mode_whole_or_not_at_all() {
        let st = PlayerState::new();
        st.exclusive.store(true, Ordering::Relaxed);
        st.output_bits.store(24, Ordering::Relaxed);
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(1));
        ui.device_name = "FiiO KA17 USB Audio Device".to_string();
        let playlist = [PathBuf::from("/a.flac")];
        for w in [20, 30, 40, 45, 60, 100] {
            let lines = info_lines(&st, &ui, "a", "flac", "", "", &playlist, pal(), w);
            let device = lines.iter().map(tui::plain).find(|l| l.contains("FiiO")).unwrap();
            assert!(tui::width(&device) <= w, "{w}: {device:?}");
            if device.contains("exclusive") {
                assert!(device.ends_with("-bit"), "{w}: the mode cut short: {device:?}");
            }
        }
    }

    #[test]
    fn the_next_track_follows_the_repeat_mode() {
        let st = PlayerState::new();
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(3));
        let three = [PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from("/c")];
        ui.current = 2;
        assert_eq!(next_index(&ui, &st, &three), None, "last track, no repeat");
        st.repeat_mode.store(RepeatMode::All as u8, Ordering::Relaxed);
        assert_eq!(next_index(&ui, &st, &three), Some(0));
        st.repeat_mode.store(RepeatMode::One as u8, Ordering::Relaxed);
        assert_eq!(next_index(&ui, &st, &three), Some(2));
        ui.current = 0;
        st.repeat_mode.store(RepeatMode::Off as u8, Ordering::Relaxed);
        assert_eq!(next_index(&ui, &st, &three), Some(1));
        // A file already found unplayable is passed over, as the producer
        // passes over it.
        st.mark_unplayable(&three[1], false);
        assert_eq!(next_index(&ui, &st, &three), Some(2));
    }
}
