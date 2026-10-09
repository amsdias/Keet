//! Classic theme renderer: the terminal's own ground and text, three colours
//! that each mean one thing (theme.rs), and a header drawn with the frame —
//! the cover beside what is playing, how it is playing and where.
//!
//! Every view shares the top block: header (cover + info), transport line,
//! status row. Below it, a titled rule names the section (the visualisation,
//! the playlist, the lyrics) with that section's hints, then the body, a
//! message line and a key bar for the view.

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use crossterm::terminal;

use crate::ansi::{fit_segments, truncate_plain, visible_len};
use crate::state::{InputMode, PlayerState, RepeatMode, RgMode, UiState, ViewMode, VizMode, VizStyle};
use crate::theme::{palette, Palette, ThemeKind};
use crate::ui::{format_time, list_scroll, render_tree_body, FrameWriter};
use crate::viz::{
    analysis_needs_raw_lines, render_lissajous, render_oscilloscope, render_spectrogram,
    render_spectrogram_analysis, render_spectrum_horizontal, render_spectrum_vertical,
    render_vu_meter, viz_body_rows, viz_rows_available, viz_top_pad, StatsMonitor, VizAnalyser,
};

/// Columns the cover needs beside a readable info column (2 + 20 + 2 + 36).
const COVER_MIN_W: usize = 60;
/// Rows a view's body needs below the header before the header may grow:
/// the player's transport + status + rule, four rows of visualisation and the
/// footer; the playlist and lyrics also want ten rows of list.
const PLAYER_BODY_MIN: usize = 2 + 1 + 4 + FOOTER_ROWS;
const LIST_BODY_MIN: usize = 2 + 1 + 1 + FOOTER_ROWS + 10;
/// Below this height the gap row under the header goes too.
const GAP_MIN_H: usize = 16;

/// Which info lines survive a header of `budget` rows without the cover, by
/// their index in `info_lines`: the title always; then, dropped first to
/// last, the next track, the play modes, the device, the format, the verdict
/// and the artist. Shown in their own order.
fn compact_info(info: Vec<String>, budget: usize) -> Vec<String> {
    const DROP_ORDER: [usize; 6] = [9, 7, 6, 3, 4, 1];
    let mut keep: Vec<bool> = info.iter().map(|l| !l.is_empty()).collect();
    keep[0] = true;
    for &i in &DROP_ORDER {
        if keep.iter().filter(|&&k| k).count() <= budget.max(1) {
            break;
        }
        keep[i] = false;
    }
    info.into_iter().zip(keep).filter(|(_, k)| *k).map(|(l, _)| l).collect()
}
/// Rows kept below a body: message line, key bar, one row of slack (a frame
/// ending on the window's last row scrolls on the next write).
const FOOTER_ROWS: usize = 3;

/// One frame row and whether it carries image cells (written without the
/// leading erase, see `FrameWriter::first_line_raw`).
struct Row {
    text: String,
    raw: bool,
}

impl Row {
    fn text(text: String) -> Self {
        Self { text, raw: false }
    }
}

/// Emit `rows`, the first as the frame's anchor.
fn emit(w: &mut FrameWriter, rows: Vec<Row>) {
    for (i, row) in rows.into_iter().enumerate() {
        match (i, row.raw) {
            (0, true) => w.first_line_raw(&row.text),
            (0, false) => w.first_line(&row.text),
            (_, true) => w.line_raw(&row.text),
            (_, false) => w.line(&row.text),
        }
    }
}

fn khz(rate: u32) -> String {
    let k = rate as f32 / 1000.0;
    if k.fract() == 0.0 { format!("{k:.0} kHz") } else { format!("{k:.1} kHz") }
}

/// "label value" with the label dim; the value dim too when it reads "off".
fn pair(p: &Palette, label: &str, value: &str) -> String {
    let v = if value == "off" { p.dim } else { p.fg };
    format!("{}{label}{} {v}{value}{}", p.dim, p.reset, p.reset)
}

/// A key bar: as many `key label` items as fit, whole.
fn key_bar(p: &Palette, keys: &[(&str, &str)], width: usize) -> String {
    let items: Vec<String> = keys
        .iter()
        .map(|(k, l)| format!("{}{}{k}{} {}{l}{}", p.accent, p.bold, p.reset, p.dim, p.reset))
        .collect();
    format!("  {}", fit_segments(&items, "  ", width.saturating_sub(2)))
}

/// `── Title ───────── hint ──` across the window; the hint goes first when
/// the window is too narrow for both.
fn titled_rule(p: &Palette, title: &str, hint: &str, width: usize) -> String {
    let fixed = 2 + 3 + visible_len(title) + 1 + 2;
    let with_hint = fixed + visible_len(hint) + 2;
    let (hint, fill) = if !hint.is_empty() && width >= with_hint + 3 {
        (format!("{} {hint} {}", p.dim, p.reset), width - with_hint)
    } else {
        (String::new(), width.saturating_sub(fixed))
    };
    format!(
        "  {r}── {rst}{b}{title}{rst}{r} {}{rst}{hint}{r}──{rst}",
        "─".repeat(fill),
        r = p.rule, b = p.bold, rst = p.reset,
    )
}

/// The track after the current one, as the playlist loop will pick it.
fn next_index(ui: &UiState, state: &PlayerState, len: usize) -> Option<usize> {
    match state.repeat_mode() {
        RepeatMode::One => Some(ui.current),
        _ if ui.current + 1 < len => Some(ui.current + 1),
        RepeatMode::All if len > 0 => Some(0),
        _ => None,
    }
}

fn track_title(ui: &UiState, idx: usize, playlist: &[PathBuf], fallback: &str) -> String {
    ui.metadata_cache
        .title(idx)
        .or_else(|| playlist.get(idx).map(|p| ui.metadata_cache.display_name(idx, p)))
        .unwrap_or_else(|| fallback.to_string())
}

/// The info column beside the cover: title, artist · album, format, the
/// bit-perfect verdict, device, play modes, what comes next. Blank strings are
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
) -> Vec<String> {
    let rst = p.reset;
    let idx = state.current_track.load(Ordering::Relaxed);
    let title = track_title(ui, idx, playlist, name);
    let (artist, album) = ui.metadata_cache.artist_album(idx);

    let title_line = format!("{}{}{rst}", p.bold, truncate_plain(&title, width));
    let mut who = Vec::new();
    if let Some(a) = artist {
        who.push(format!("{}{a}{rst}", p.accent));
    }
    if let Some(al) = album {
        who.push(format!("{}{al}{rst}", p.dim));
    }
    let who_line = fit_segments(&who, &format!("{} · {rst}", p.dim), width);

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
        format!("{}{codec}{rst}", p.fg),
        format!("{}{rate}{rst}", p.dim),
        format!("{}{channels}{rst}", p.dim),
        format!("{}{}{rst}", p.dim, format_time(state.total_secs())),
    ];
    let format_line = fit_segments(&format_items, &format!("{} · {rst}", p.dim), width);

    let verdict = crate::signal::verdict(&crate::signal::PathInputs::from_state(state, fx_name, cf_name));
    let verdict_line = match crate::signal::fitted(&verdict, " · ", width) {
        Some((ok, text)) => format!("{}{text}{rst}", if ok { p.accent } else { p.warn }),
        None => String::new(),
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
    let device_line = format!(
        "{}●{rst} {}{} · {mode}{rst}",
        p.accent,
        truncate_plain(device, width.saturating_sub(4 + visible_len(&mode) + 3)),
        p.dim,
    );

    // shuffle ○ off   repeat ● all   xfade 4 s   rg album   hq
    let lamp = |on: bool| if on { format!("{}●{rst}", p.accent) } else { format!("{}○{rst}", p.dim) };
    let repeat = state.repeat_mode();
    let xfade = state.crossfade_secs.load(Ordering::Relaxed);
    let mut modes = vec![
        format!("{}shuffle{rst} {} {}", p.dim, lamp(ui.shuffle), if ui.shuffle { "on" } else { "off" }),
        format!(
            "{}repeat{rst} {} {}",
            p.dim,
            lamp(repeat != RepeatMode::Off),
            match repeat { RepeatMode::Off => "off", RepeatMode::All => "all", RepeatMode::One => "one" },
        ),
        pair(p, "xfade", &if xfade == 0 { "off".to_string() } else { format!("{xfade} s") }),
        pair(p, "rg", match state.rg_mode() { RgMode::Track => "track", RgMode::Album => "album", RgMode::Off => "off" }),
    ];
    if ui.hq_resampler {
        modes.push(format!("{}hq resampler{rst}", p.dim));
    }
    let modes_line = fit_segments(&modes, "   ", width);

    // track 4 of 11 · next Glasswork — Ines Varela
    let total = state.total_tracks.load(Ordering::Relaxed);
    let mut next_items = vec![format!("{}track {} of {total}{rst}", p.dim, idx + 1)];
    match next_index(ui, state, playlist.len()) {
        Some(n) => {
            let t = track_title(ui, n, playlist, "");
            let by = ui.metadata_cache.artist_album(n).0.map(|a| format!("{} — {a}{rst}", p.dim)).unwrap_or_default();
            let queued = if ui.enqueue_count > 0 { format!("{} (queued){rst}", p.dim) } else { String::new() };
            next_items.push(format!("{}next{rst} {t}{by}{queued}", p.dim));
        }
        None => next_items.push(format!("{}last track{rst}", p.dim)),
    }
    let next_line = fit_segments(&next_items, &format!("{} · {rst}", p.dim), width);

    vec![
        title_line, who_line, String::new(), format_line, verdict_line, String::new(),
        device_line, modes_line, String::new(), next_line,
    ]
}

/// `▶ 02:41  ━━━━━━━━●──────────  06:12  −3:31`
fn transport_line(state: &PlayerState, p: &Palette, width: usize) -> String {
    let rst = p.reset;
    let (t, total) = (state.time_secs(), state.total_secs());
    let icon = if state.is_paused() { format!("{}⏸{rst}", p.fg) } else { format!("{}▶{rst}", p.accent) };
    let left = format!("  {icon} {}{}{rst}  ", p.bold, format_time(t));
    let right = format!("  {}  {}−{}{rst}", format_time(total), p.dim, format_time((total - t).max(0.0)));
    let bar_w = width.saturating_sub(visible_len(&left) + visible_len(&right));
    if bar_w < 4 {
        return format!("  {icon} {}{}{rst} / {}", p.bold, format_time(t), format_time(total));
    }
    let progress = if total > 0.0 { (t / total).clamp(0.0, 1.0) } else { 0.0 };
    let filled = (progress * (bar_w - 1) as f64).round() as usize;
    format!(
        "{left}{a}{}●{rst}{r}{}{rst}{right}",
        "━".repeat(filled),
        "─".repeat(bar_w - 1 - filled),
        a = p.accent, r = p.rule,
    )
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
) -> String {
    let rst = p.reset;
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
        items.push(format!("{}{xruns} dropouts{rst}", p.warn));
    }
    // Shape as well as colour: ● clipping, ○ idle.
    let lamp = if state.is_clipping() {
        format!("   {}clip{rst} {}●{rst}", p.dim, p.danger)
    } else {
        format!("   {}clip{rst} {}○{rst}", p.dim, p.good)
    };
    let room = width.saturating_sub(2 + visible_len(&lamp));
    format!("  {}{lamp}", fit_segments(&items, "   ", room))
}

/// The block every Classic view starts with: header (cover + info), a gap,
/// the transport line and the status row.
#[allow(clippy::too_many_arguments)]
fn top_rows(
    state: &PlayerState,
    ui: &mut UiState,
    name: &str,
    ext: &str,
    eq_preset: &crate::eq::EqPreset,
    fx_name: &str,
    cf_name: &str,
    stats: &mut StatsMonitor,
    prev_frame_lines: usize,
    playlist: &[PathBuf],
    p: &Palette,
    (term_w, term_h): (usize, usize),
    budget: usize,
) -> Vec<Row> {
    let size = crate::cover::CoverSize::CLASSIC;
    // The cover comes with its full ten rows or not at all: an image taller
    // than the space left scrolls the screen, and in a short window it pushed
    // the list and the key bar off the bottom.
    let cover_on = ui.cover_enabled && term_w >= COVER_MIN_W && budget >= size.rows as usize;
    let mut rows = Vec::with_capacity(16);

    if cover_on {
        let info_w = term_w.saturating_sub(2 + size.cols as usize + 2 + 1);
        let info = info_lines(state, ui, name, ext, fx_name, cf_name, playlist, p, info_w);
        // Emit-on-change for images that stay put (Kitty, iTerm2, Sixel):
        // re-sending one 20×/s is the cost the spectrogram rule avoids.
        let sticky = crate::cover::image_is_sticky();
        let repaint = !sticky
            || prev_frame_lines == usize::MAX
            || !ui.cover_block_intact
            || ui.cover_dirty_frame;
        let cover_lines = if repaint {
            match ui.cover.as_ref() {
                Some(img) => crate::cover::render(img),
                None => crate::cover::empty_slot_lines(size),
            }
        } else {
            crate::cover::passive_lines(size)
        };
        ui.cover_block_intact = true;
        ui.cover_dirty_frame = false;
        for (i, info_line) in info.into_iter().enumerate() {
            // Cut to the column: these rows are written raw (the image), so
            // the frame's own cut at the window edge does not reach them.
            let info_line = crate::ansi::truncate_ansi(&info_line, info_w);
            let cover = cover_lines.get(i).map(String::as_str).unwrap_or("");
            if sticky {
                // The image's cells come first: no erase before them, one
                // after (clears only what follows the image on this row).
                rows.push(Row { text: format!("  {cover}  {info_line}\x1B[K"), raw: true });
            } else {
                rows.push(Row::text(format!("  {cover}  {info_line}")));
            }
        }
    } else {
        if ui.cover_block_intact
            && matches!(crate::cover::detect_protocol(), crate::cover::GraphicsProtocol::Kitty)
        {
            // A Kitty image is an overlay: drawing text over its cells leaves it.
            crate::term::out!("{}", crate::cover::kitty_clear_escape());
        }
        ui.cover_block_intact = false;
        let info_w = term_w.saturating_sub(3);
        // Without the cover's height to fill, the spacing rows go, and a
        // short window keeps only what fits (compact_info).
        let info = info_lines(state, ui, name, ext, fx_name, cf_name, playlist, p, info_w);
        for line in compact_info(info, budget) {
            rows.push(Row::text(format!("  {line}")));
        }
    }
    if term_h >= GAP_MIN_H {
        rows.push(Row::text(String::new()));
    }
    rows.push(Row::text(transport_line(state, p, term_w)));
    rows.push(Row::text(status_line(state, p, eq_preset, fx_name, cf_name, stats, term_w)));
    rows
}

fn viz_title(mode: VizMode) -> &'static str {
    match mode {
        VizMode::None => "",
        VizMode::VuMeter => "VU meter",
        VizMode::SpectrumHorizontal | VizMode::SpectrumVertical => "Spectrum",
        VizMode::Oscilloscope => "Oscilloscope",
        VizMode::Lissajous => "Vectorscope",
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

/// The message line: the active status, else empty (it keeps its row, so the
/// key bar does not jump when a message comes and goes).
fn message_line(ui: &mut UiState, p: &Palette) -> String {
    match ui.active_status() {
        Some(msg) => format!("  {}{msg}{}", p.accent, p.reset),
        None => String::new(),
    }
}

/// Classic's renderer for the player, playlist and lyrics views. Returns the
/// rows drawn below the anchor (FrameWriter's count).
#[allow(clippy::too_many_arguments)] // cohesive render context, as in ui.rs
pub fn print_status_classic(
    state: &PlayerState,
    ui: &mut UiState,
    name: &str,
    track_info: &str,
    ext: &str,
    eq_preset: &crate::eq::EqPreset,
    fx_name: &str,
    cf_name: &str,
    stats: &mut StatsMonitor,
    prev_frame_lines: usize,
    playlist: &[PathBuf],
    analyser: &VizAnalyser,
) -> usize {
    let p = palette(ThemeKind::Classic);
    let (term_w, term_h) = terminal::size()
        .map(|(w, h)| (w as usize, h as usize))
        .unwrap_or((120, 40));
    let fullscreen = state.viz_fullscreen() && ui.view_mode == ViewMode::Player;

    // Sixel emit-on-change bookkeeping: cleared every frame, re-asserted only
    // by the analysis branch. Any frame that does not reach it may paint over
    // the block, so the next analysis render must re-emit.
    let block_was_intact = ui.spectro_block_intact;
    ui.spectro_block_intact = false;

    if prev_frame_lines != usize::MAX && prev_frame_lines > 0 {
        crate::term::out!("\x1B[{}F", prev_frame_lines); // CPL: up N lines, column 1
    }
    let mut w = FrameWriter::fitted();

    let top = if fullscreen {
        // Full window: the header and its cover go; one line carries the
        // song, the clock and the way back out.
        if ui.cover_block_intact
            && matches!(crate::cover::detect_protocol(), crate::cover::GraphicsProtocol::Kitty)
        {
            crate::term::out!("{}", crate::cover::kitty_clear_escape());
        }
        ui.cover_block_intact = false;
        let track = state.current_track.load(Ordering::Relaxed) + 1;
        let total = state.total_tracks.load(Ordering::Relaxed);
        let icon = if state.is_paused() { format!("{}⏸", p.fg) } else { format!("{}▶", p.accent) };
        let head = format!(
            "{d}[{track}/{total}]{r} {icon}{r} {b}{n}{r}  ",
            d = p.dim, r = p.reset, b = p.bold, n = truncate_plain(name, 35),
        );
        let room = term_w.saturating_sub(visible_len(&head));
        let items = fit_segments(
            &crate::ui::fullscreen_items(track_info, state.time_secs(), state.total_secs()),
            "  •  ",
            room,
        );
        vec![Row::text(format!("{head}{}{items}{}", p.dim, p.reset))]
    } else {
        // The header takes what the view's body leaves (see PLAYER_BODY_MIN).
        let gap = usize::from(term_h >= GAP_MIN_H);
        let body_min = if ui.view_mode == ViewMode::Player { PLAYER_BODY_MIN } else { LIST_BODY_MIN };
        let budget = term_h.saturating_sub(body_min + gap);
        top_rows(
            state, ui, name, ext, eq_preset, fx_name, cf_name, stats, prev_frame_lines,
            playlist, p, (term_w, term_h), budget,
        )
    };
    let top_n = top.len();
    emit(&mut w, top);

    match ui.view_mode {
        ViewMode::Playlist => playlist_body(state, ui, &mut w, p, playlist, (term_w, term_h), top_n),
        ViewMode::Lyrics => lyrics_body(state, ui, &mut w, p, (term_w, term_h), top_n),
        _ => player_body(
            state, ui, &mut w, p, analyser, prev_frame_lines, block_was_intact,
            (term_w, term_h), top_n, fullscreen,
        ),
    }

    crate::term::out!("\x1B[J");
    crate::term::flush();
    w.count()
}

#[allow(clippy::too_many_arguments)]
fn player_body(
    state: &PlayerState,
    ui: &mut UiState,
    w: &mut FrameWriter,
    p: &Palette,
    analyser: &VizAnalyser,
    prev_frame_lines: usize,
    block_was_intact: bool,
    (term_w, term_h): (usize, usize),
    top_n: usize,
    fullscreen: bool,
) {
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
    // Outside full window the block gets a fixed slot: the height of the
    // tallest mode in this window, each mode centred in it, so the message
    // line and key bar stay put as `v` cycles (they used to jump with every
    // mode's own height). Full window centres in the whole space instead.
    let slot = if fullscreen { body } else { viz_slot_rows(avail, extras) };
    let pad = if fullscreen { viz_top_pad(avail, body, true) } else { slot.saturating_sub(body) / 2 };

    if ruled && viz_mode == VizMode::None {
        w.line(&titled_rule(p, "Visualization off", &format!("v {}", viz_next_name(viz_mode)), term_w));
    } else if ruled {
        let style = if viz_mode == VizMode::SpectrogramAnalysis {
            if matches!(viz_style, VizStyle::Dots) { "linear" } else { "log" }
        } else {
            match viz_style { VizStyle::Dots => "bars", VizStyle::Bars => "dots" }
        };
        let hint = format!("v {} · b {style}", viz_next_name(viz_mode));
        w.line(&titled_rule(p, viz_title(viz_mode), &hint, term_w));
    }
    for _ in 0..pad {
        w.line("");
    }

    match viz_mode {
        VizMode::None => {}
        VizMode::VuMeter => render_vu_meter(state, viz_style, term_w, body, extras).iter().for_each(|l| w.line(l)),
        VizMode::SpectrumHorizontal => {
            render_spectrum_horizontal(state, viz_style, term_w, body, extras).iter().for_each(|l| w.line(l))
        }
        VizMode::SpectrumVertical => {
            render_spectrum_vertical(state, viz_style, term_w, body, extras).iter().for_each(|l| w.line(l))
        }
        VizMode::Oscilloscope => render_oscilloscope(analyser, viz_style, term_w, body).iter().for_each(|l| w.line(l)),
        VizMode::Lissajous => render_lissajous(analyser, viz_style, term_w, body).iter().for_each(|l| w.line(l)),
        VizMode::Spectrogram => render_spectrogram(analyser, viz_style, term_w, body).iter().for_each(|l| w.line(l)),
        VizMode::SpectrogramAnalysis => {
            // viz_style selects the frequency axis here: Dots = log, Bars = linear.
            let log_axis = matches!(viz_style, VizStyle::Dots);
            // Sixel: no erase-to-EOL, the block erases itself (viz.rs).
            let raw = analysis_needs_raw_lines();
            // Re-emit on a full repaint, when another view painted over the
            // block, or when the block moved or resized (keyed on its own
            // first row and height, not the frame's: see CLAUDE.md).
            let block = (w.count(), body);
            let force = prev_frame_lines == usize::MAX || !block_was_intact || block != ui.last_viz_block;
            ui.last_viz_block = block;
            ui.spectro_block_intact = true;
            for line in render_spectrogram_analysis(analyser, term_w, log_axis, state.is_paused(), body, force) {
                if raw { w.line_raw(&line); } else { w.line(&line); }
            }
        }
    }

    for _ in (pad + body)..slot {
        w.line("");
    }

    if fullscreen {
        if let Some(msg) = ui.active_status() {
            w.line(&format!("  {}{msg}{}", p.accent, p.reset));
        }
        return;
    }
    w.line(&message_line(ui, p));
    w.line(&key_bar(p, PLAYER_KEYS, term_w));
}

/// Every visualisation mode, for sizing the shared slot.
const VIZ_MODES: [VizMode; 7] = [
    VizMode::VuMeter, VizMode::SpectrumHorizontal, VizMode::SpectrumVertical, VizMode::Oscilloscope,
    VizMode::Lissajous, VizMode::Spectrogram, VizMode::SpectrogramAnalysis,
];

/// Rows of the visualisation slot: what the tallest mode takes in `avail`.
fn viz_slot_rows(avail: usize, extras: bool) -> usize {
    VIZ_MODES.iter().map(|&m| viz_body_rows(m, avail, false, extras)).max().unwrap_or(0)
}

fn format_total(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn playlist_body(
    state: &PlayerState,
    ui: &mut UiState,
    w: &mut FrameWriter,
    p: &Palette,
    playlist: &[PathBuf],
    (term_w, term_h): (usize, usize),
    top_n: usize,
) {
    let rst = p.reset;
    let tree = ui.library_tree_mode;
    // One pass over the durations for the total and the time column's width.
    let (total_secs, longest) = (0..playlist.len())
        .filter_map(|i| ui.metadata_cache.duration(i))
        .fold((0.0f64, 0.0f64), |(sum, max), d| (sum + d, max.max(d)));
    let hint = if tree {
        format!("{} tracks · {} · Tab list · / filter", playlist.len(), format_total(total_secs))
    } else {
        format!("{} tracks · {} · Tab tree · / search", playlist.len(), format_total(total_secs))
    };
    w.line(&titled_rule(p, "Playlist", &hint, term_w));

    let header_rows = usize::from(!tree);
    let visible_rows = term_h.saturating_sub(top_n + 1 + header_rows + FOOTER_ROWS).max(1);
    ui.last_visible_rows = visible_rows;

    if tree {
        let lines = render_tree_body(ui, visible_rows, term_w, p);
        let n = lines.len();
        lines.iter().for_each(|l| w.line(l));
        (n..visible_rows).for_each(|_| w.line(""));
    } else {
        // Columns: marker · number · title · album · time.
        let num_w = playlist.len().to_string().len().max(2);
        // Wide enough for the longest duration listed (10 h+ is "10:00:00").
        let time_w = 1 + format_time(longest).len().max(5);
        let content = term_w.saturating_sub(2 + 1 + 3 + num_w + 3 + time_w + 1);
        let album_w = if content >= 50 { (content * 30 / 100).clamp(12, 32) } else { 0 };
        let title_w = content.saturating_sub(album_w);
        let cell = |s: &str, width: usize| {
            let t = truncate_plain(s, width.saturating_sub(1));
            let pad = width.saturating_sub(visible_len(&t));
            format!("{t}{}", " ".repeat(pad))
        };
        w.line(&format!(
            "{}  {}   {:>num_w$}   {}{}{:>time_w$}{rst}",
            p.dim, " ", "#", cell("Title", title_w), cell(if album_w > 0 { "Album" } else { "" }, album_w), "Time",
        ));

        let search_active = matches!(&ui.input_mode, InputMode::Search(q) if !q.is_empty());
        let items_len = if search_active && ui.filtered_indices.is_empty() {
            0
        } else if ui.filtered_indices.is_empty() {
            playlist.len()
        } else {
            ui.filtered_indices.len()
        };
        ui.scroll_offset = list_scroll(ui.cursor, ui.scroll_offset, items_len, visible_rows);
        let queued = (ui.current + 1)..=(ui.current + ui.enqueue_count);

        if items_len == 0 && search_active {
            w.line(&format!("  {}(no matches){rst}", p.dim));
            (1..visible_rows).for_each(|_| w.line(""));
        } else {
            let shown = visible_rows.min(items_len.saturating_sub(ui.scroll_offset));
            for row in 0..shown {
                let pos = ui.scroll_offset + row;
                let idx = if ui.filtered_indices.is_empty() { pos } else { ui.filtered_indices[pos] };
                let playing = idx == ui.current;
                let cursor = pos == ui.cursor;
                let is_queued = ui.enqueue_count > 0 && queued.contains(&idx);
                let title = ui.metadata_cache.display_name(idx, &playlist[idx]);
                let album = ui.metadata_cache.album(idx).unwrap_or_default();
                let dur = ui.metadata_cache.duration(idx).map(format_time).unwrap_or_default();
                let num = format!("{:0>num_w$}", idx + 1);
                let marker = if playing { "♪" } else if cursor { "▌" } else if is_queued { "+" } else { " " };
                let line = if cursor {
                    let plain = format!(
                        "  {marker}   {num}   {}{}{dur:>time_w$}",
                        cell(&title, title_w), cell(&album, album_w),
                    );
                    crate::theme::cursor_row(p, &plain, term_w)
                } else if playing {
                    format!(
                        "  {a}{marker}{rst}   {a}{b}{num}   {}{rst}{d}{}{rst}{dur:>time_w$}",
                        cell(&title, title_w), cell(&album, album_w),
                        a = p.accent, b = p.bold, d = p.dim,
                    )
                } else {
                    format!(
                        "  {a}{marker}{rst}   {d}{num}{rst}   {}{d}{}{rst}{dur:>time_w$}",
                        cell(&title, title_w), cell(&album, album_w),
                        a = p.accent, d = p.dim,
                    )
                };
                w.line(&line);
            }
            (shown..visible_rows).for_each(|_| w.line(""));
        }
    }

    // The message row doubles as the prompt while typing.
    let message = match &ui.input_mode {
        InputMode::Search(q) => format!("  {}/{rst} {q}{}_{rst}", p.accent, p.dim),
        InputMode::SavePlaylist(n) => format!("  {}save as{rst} {n}{}_{rst}", p.accent, p.dim),
        InputMode::Normal => message_line(ui, p),
    };
    w.line(&message);
    let keys: &[(&str, &str)] = if tree {
        &[("↵", "play"), ("?", "keys"), ("←→", "fold"), ("/", "filter"), ("d", "remove"), ("Tab", "list"), ("l", "close")]
    } else {
        &[
            ("↵", "play"), ("?", "keys"), ("a", "queue"), ("/", "search"), ("d", "remove"), ("s", "save"),
            ("⇧s", "sort"), ("Tab", "tree"), ("l", "close"),
        ]
    };
    w.line(&key_bar(p, keys, term_w));
    let _ = state;
}

fn lyrics_body(
    state: &PlayerState,
    ui: &mut UiState,
    w: &mut FrameWriter,
    p: &Palette,
    (term_w, term_h): (usize, usize),
    top_n: usize,
) {
    let rst = p.reset;
    let synced = ui.lyrics.as_ref().is_some_and(|l| l.is_synced());
    let mut hint = match &ui.lyrics {
        Some(l) => crate::lyrics::source_line(l, ui.lyrics_source),
        None => String::new(),
    };
    if synced && ui.lyrics_offset != 0.0 {
        hint.push_str(&format!(" · offset {:+.1} s", ui.lyrics_offset));
    }
    w.line(&titled_rule(p, "Lyrics", &hint, term_w));

    let visible_rows = term_h.saturating_sub(top_n + 1 + FOOTER_ROWS).max(1);
    match ui.lyrics.as_ref() {
        Some(lyrics) => {
            let total = lyrics.line_count();
            let current = lyrics.current_line(state.time_secs() + ui.lyrics_offset);
            // Follow the song: keep the current line centred until the user scrolls.
            if synced && ui.lyrics_auto_scroll {
                if let Some(cur) = current {
                    ui.lyrics_scroll = cur.saturating_sub(visible_rows / 2);
                }
            }
            ui.lyrics_scroll = if total > visible_rows { ui.lyrics_scroll.min(total - visible_rows) } else { 0 };
            for row in 0..visible_rows {
                let i = ui.lyrics_scroll + row;
                if i >= total {
                    w.line("");
                    continue;
                }
                let text = lyrics.line_text(i);
                let line = match lyrics.line_time(i) {
                    Some(t) => {
                        let ts = format_time(t);
                        match current {
                            Some(c) if c == i => format!("    {a}{ts}{rst} {a}{b}▸ {text}{rst}", a = p.accent, b = p.bold),
                            Some(c) if i < c => format!("    {d}{ts}   {text}{rst}", d = p.dim),
                            _ => format!("    {d}{ts}{rst}   {text}", d = p.dim),
                        }
                    }
                    None => format!("    {text}"),
                };
                w.line(&line);
            }
        }
        None => {
            let what = if ui.lyrics_receiver.is_some() { "looking for lyrics…" } else { "no lyrics for this track" };
            w.line(&format!("    {}{what}{rst}", p.dim));
            (1..visible_rows).for_each(|_| w.line(""));
        }
    }

    w.line(&message_line(ui, p));
    let keys: &[(&str, &str)] = if synced {
        &[("a d", "sync ±0.5 s"), ("0", "reset"), ("w s", "scroll"), ("y", "close"), ("q", "quit")]
    } else {
        &[("w s", "scroll"), ("y", "close"), ("q", "quit")]
    };
    w.line(&key_bar(p, keys, term_w));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pal() -> &'static Palette {
        palette(ThemeKind::Classic)
    }

    #[test]
    fn titled_rules_span_the_window_and_drop_the_hint_first() {
        for w in [40, 80, 100, 160] {
            let r = titled_rule(pal(), "Spectrum", "v next · b dots", w);
            assert_eq!(visible_len(&r), w, "{w}: {r:?}");
        }
        let narrow = titled_rule(pal(), "Spectrum", "a very long hint that cannot fit", 40);
        assert!(!crate::ansi::strip_ansi(&narrow).contains("hint"), "{narrow:?}");
        assert_eq!(visible_len(&narrow), 40);
    }

    #[test]
    fn key_bars_end_on_a_whole_key() {
        let bar = crate::ansi::strip_ansi(&key_bar(pal(), PLAYER_KEYS, 30));
        assert!(visible_len(&bar) <= 30, "{bar:?}");
        assert!(bar.trim_end().ends_with("seek") || bar.trim_end().ends_with("track"), "{bar:?}");
    }

    #[test]
    fn the_transport_line_fills_the_window() {
        let st = PlayerState::new();
        for w in [20, 60, 100, 200] {
            let line = transport_line(&st, pal(), w);
            assert!(visible_len(&line) <= w, "{w}: {line:?}");
        }
        assert_eq!(visible_len(&transport_line(&st, pal(), 100)), 100);
    }

    #[test]
    fn the_status_row_keeps_its_clip_lamp_at_any_width() {
        let st = PlayerState::new();
        let eq = crate::eq::builtin_presets().remove(0);
        let mut stats = StatsMonitor::new();
        for w in [30, 60, 120] {
            let line = status_line(&st, pal(), &eq, "None", "Off", &mut stats, w);
            let plain = crate::ansi::strip_ansi(&line);
            assert!(visible_len(&plain) <= w, "{w}: {plain:?}");
            assert!(plain.trim_end().ends_with("clip ○"), "{w}: {plain:?}");
        }
    }

    #[test]
    fn the_viz_slot_fits_every_mode_and_never_the_window_twice() {
        for avail in [0, 3, 10, 20, 40, 80] {
            for extras in [false, true] {
                let slot = viz_slot_rows(avail, extras);
                assert!(slot <= avail, "{avail}: slot {slot} past the window");
                for m in VIZ_MODES {
                    assert!(viz_body_rows(m, avail, false, extras) <= slot, "{m:?} taller than the slot at {avail}");
                }
            }
        }
    }

    #[test]
    fn a_short_header_keeps_the_title_and_drops_the_rest_in_order() {
        let info: Vec<String> = ["title", "artist", "", "format", "verdict", "", "device", "modes", "", "next"]
            .map(String::from)
            .to_vec();
        assert_eq!(compact_info(info.clone(), 10), ["title", "artist", "format", "verdict", "device", "modes", "next"]);
        assert_eq!(compact_info(info.clone(), 5), ["title", "artist", "format", "verdict", "device"]);
        assert_eq!(compact_info(info.clone(), 2), ["title", "artist"]);
        assert_eq!(compact_info(info.clone(), 0), ["title"], "the title always stays");
        // Shared mode has no verdict: one fewer line to begin with.
        let mut shared = info;
        shared[4].clear();
        assert_eq!(compact_info(shared, 4), ["title", "artist", "format", "device"]);
    }

    #[test]
    fn the_next_track_follows_the_repeat_mode() {
        let st = PlayerState::new();
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(3));
        ui.current = 2;
        assert_eq!(next_index(&ui, &st, 3), None, "last track, no repeat");
        st.repeat_mode.store(RepeatMode::All as u8, Ordering::Relaxed);
        assert_eq!(next_index(&ui, &st, 3), Some(0));
        st.repeat_mode.store(RepeatMode::One as u8, Ordering::Relaxed);
        assert_eq!(next_index(&ui, &st, 3), Some(2));
        ui.current = 0;
        st.repeat_mode.store(RepeatMode::Off as u8, Ordering::Relaxed);
        assert_eq!(next_index(&ui, &st, 3), Some(1));
    }
}
