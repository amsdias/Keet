
use crate::tui::Cols;
use std::path::PathBuf;
use std::time::Duration;
use std::sync::atomic::Ordering;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::Frame;

use crate::state::{
    Hit, Pick, PlayerState, VizMode, RepeatMode,
    ViewMode, InputMode, UiState,
};
use crate::text_input::{Edit, TextInput};
use crate::tui::{self, LineBuf};
use crate::viz::{StatsMonitor, VizAnalyser};

pub fn format_time(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    if total >= 3600 {
        // Audiobooks and long mixes: h:mm:ss instead of rolling minutes past 60.
        format!("{}:{:02}:{:02}", total / 3600, (total % 3600) / 60, total % 60)
    } else {
        format!("{:02}:{:02}", total / 60, total % 60)
    }
}

use crate::ansi::truncate_plain;

/// Everything a frame is drawn from.
pub struct Ctx<'a> {
    pub state: &'a PlayerState,
    pub ui: &'a mut UiState,
    pub name: &'a str,
    pub track_info: &'a str,
    pub ext: &'a str,
    pub eq_preset: &'a crate::eq::EqPreset,
    pub fx_name: &'a str,
    pub cf_name: &'a str,
    pub stats: &'a mut StatsMonitor,
    pub playlist: &'a [PathBuf],
    pub analyser: &'a VizAnalyser,
    /// The screen was cleared before this draw: every image is sent again.
    pub repaint: bool,
}

/// What the full-window anchor line shows after the title (joined with the
/// same `•` the format uses inside itself, so every dot on the line matches),
/// most important
/// first (a narrow window drops from the end, see `tui::fit_segments`): the
/// format without its leading duration — the clock right after shows it, and
/// repeating it read as noise — then the clock, then the way out of full
/// window, which is otherwise unguessable with the rest of the UI gone.
pub(crate) fn fullscreen_items(track_info: &str, time_secs: f64, total_secs: f64) -> Vec<String> {
    let format = track_info.split_once(" • ").map_or(track_info, |(_, rest)| rest);
    vec![
        format.to_string(),
        format!("{}/{}", format_time(time_secs), format_time(total_secs)),
        "{⇧F} exit".to_string(),
    ]
}

/// The list's scroll offset after the cursor moved, Vim-style: keep a margin
/// of up to 4 rows (scrolloff) between the cursor and the window's edges, and
/// never scroll past the end (no empty padding below the last item). Every
/// theme's list uses it.
pub(crate) fn list_scroll(cursor: usize, offset: usize, items_len: usize, visible_rows: usize) -> usize {
    let margin = 4.min(visible_rows / 2);
    let mut offset = offset;
    if cursor >= offset + visible_rows.saturating_sub(margin) {
        offset = cursor.saturating_sub(visible_rows.saturating_sub(margin + 1));
    }
    if cursor < offset + margin {
        offset = cursor.saturating_sub(margin);
    }
    offset.min(items_len.saturating_sub(visible_rows))
}

/// A prompt row: `prefix`, then `field` scrolled to fit in `width` columns;
/// and the column the cursor goes in.
pub(crate) fn field_line(prefix: Line<'static>, field: &TextInput, width: usize) -> (Line<'static>, usize) {
    let pw = prefix.cols();
    let (shown, col) = field.view(width.saturating_sub(pw));
    // Drawn without bidi marks (the field's text keeps them: a real name may
    // have them; drawn, a right-to-left override reversed the row in a
    // bidi-aware terminal). They take no columns: the cursor stays put.
    let shown = crate::ansi::sanitize_display(&shown);
    let mut l = LineBuf::new();
    l.append(prefix).reset().push_str(&shown);
    (l.into_line(), pw + col)
}

/// Show the terminal's cursor at column `x` of row `y` of the window (the
/// field being typed in), when that is on screen.
pub(crate) fn show_cursor(f: &mut Frame, x: usize, y: usize) {
    let area = f.area();
    if x < area.width as usize && y < area.height as usize {
        f.set_cursor_position((area.x + x as u16, area.y + y as u16));
    }
}

/// The visualisation `mode` drawn into `area` (its rows exactly): the shared
/// body of Classic's and Minimal's player view. The analysis spectrogram may
/// place an image instead of cells; it is sent again when the block moved,
/// another view drew over it, or the screen was cleared.
pub(crate) fn draw_viz(f: &mut Frame, area: Rect, cx: &mut Ctx, block_was_intact: bool) {
    use crate::viz::{
        render_lissajous, render_oscilloscope, render_spectrogram, render_spectrogram_analysis,
        render_spectrum_horizontal, render_spectrum_vertical, render_vu_meter, AnalysisOut,
    };
    let state = cx.state;
    let (w, rows) = (area.width as usize, area.height as usize);
    if !area.is_empty() {
        cx.ui.viz_drawn_at = Some(area);
    }
    let style = state.viz_style();
    let extras = state.viz_extras();
    let analyser = cx.analyser;
    let lines = match state.viz_mode() {
        VizMode::None => Vec::new(),
        VizMode::VuMeter => render_vu_meter(state, style, w, rows, extras),
        VizMode::SpectrumHorizontal => render_spectrum_horizontal(state, style, w, rows, extras),
        VizMode::SpectrumVertical => render_spectrum_vertical(state, style, w, rows, extras),
        VizMode::Oscilloscope => render_oscilloscope(analyser, style, w, rows),
        VizMode::Lissajous => render_lissajous(analyser, style, w, rows),
        VizMode::Spectrogram => render_spectrogram(analyser, style, w, rows),
        VizMode::Polar => crate::viz_polar::render_polar(state, &analyser.polar_trail, style, w, rows, extras),
        VizMode::Galaxy => crate::viz_galaxy::render_galaxy(&analyser.galaxy, state.theme_kind(), style, w, rows),
        VizMode::Cymatics => {
            crate::viz_cymatics::render_cymatics(&analyser.cymatics, state.theme_kind(), style, w, rows, extras)
        }
        VizMode::SpectrogramAnalysis => {
            // viz_style selects the frequency axis here: Dots = log, Bars = linear.
            let log_axis = matches!(style, crate::state::VizStyle::Dots);
            let force = cx.repaint || !block_was_intact || area != cx.ui.last_viz_block;
            cx.ui.last_viz_block = area;
            cx.ui.spectro_block_intact = true;
            match render_spectrogram_analysis(analyser, w, log_axis, state.is_paused(), rows, force) {
                AnalysisOut::Lines(lines) => lines,
                AnalysisOut::Image { x, cols, rows, payload } => {
                    let at = Rect { x: area.x + x as u16, y: area.y, width: cols as u16, height: rows as u16 };
                    tui::place_image(tui::ImageSlot::Viz, at.intersection(area), payload);
                    Vec::new()
                }
            }
        }
    };
    tui::draw_lines(f.buffer_mut(), area, &lines);
}

/// A natural track change: the list's cursor goes with the playing track
/// while the list is on screen — its own view, or docked without the keys
/// (it stayed on the old row while ▶ moved on) — and not filtered.
pub fn follow_playing(ui: &mut UiState) {
    let shown = base_view(ui) == ViewMode::Playlist || ui.docked_shown.contains(&ViewMode::Playlist);
    if shown && !list_filtered(ui) {
        ui.cursor = ui.current;
    }
}

/// Whether the synced lyric line shown changed since the last call (and so
/// a frame is due now): with no visualization moving, frames come every
/// `IDLE_FRAME`, and the lyrics panel following the song moved its line up
/// to 250 ms after the singer. Only while the panel shows (floating, under
/// a dialog or docked).
pub fn lyrics_line_moved(ui: &mut UiState, state: &PlayerState) -> bool {
    let shown = base_view(ui) == ViewMode::Lyrics || ui.docked_shown.contains(&ViewMode::Lyrics);
    let line = if shown {
        let clock = state.time_secs() + ui.lyrics_offset;
        ui.lyrics().filter(|l| l.is_synced()).and_then(|l| l.current_line(clock))
    } else {
        None
    };
    let moved = shown && line != ui.lyrics_line_seen;
    ui.lyrics_line_seen = line;
    moved
}

/// What a draw takes once (the cursor a click put, which keeps its list's
/// scroll for one frame). A pass drawn again — the stale-image retry in
/// `Player::draw_status` — gets it back first: the first pass took the click
/// and the second scrolled the row from under the pointer. (The lyrics step
/// needs no saving: the first pass stores the scroll it reached.)
pub struct DrawOnce {
    list_clicked: Option<usize>,
    tree_clicked: Option<usize>,
}

impl DrawOnce {
    pub fn of(ui: &UiState) -> Self {
        DrawOnce { list_clicked: ui.list_clicked, tree_clicked: ui.tree_clicked }
    }

    pub fn restore(&self, ui: &mut UiState) {
        ui.list_clicked = self.list_clicked;
        ui.tree_clicked = self.tree_clicked;
    }
}

/// Top-level renderer dispatcher. Routes by the active theme: each theme owns
/// its view renderers (`ui_classic`, `ui_minimal`, `ui_hifi`); the EQ editor
/// and the key list are shared screens.
pub fn draw(f: &mut Frame, cx: &mut Ctx) {
    cx.ui.viz_drawn_at = None;
    // Not on the too-small screen, which cannot show it: reading takes the
    // next queued notice and starts its time, and it ran out unseen.
    cx.ui.frame_status = if cx.ui.too_small { None } else { cx.ui.active_status() };
    cx.ui.status_drawn_at = None;
    // The too-small screen shows no panel, but it is not a layout: what was
    // on screen before it stays the record (`open_view`), or a key pressed
    // meanwhile reset the panel it only focuses.
    if !cx.ui.too_small {
        cx.ui.floating_shown = None;
    }
    draw_frame(f, cx);
    let ui = &mut *cx.ui;
    // Whether the visualization showed: drawn, and at least a quarter of it
    // left in view by the dialogs (a floating playlist leaves only a margin
    // of it — not worth an FFT and 30 frames a second).
    let covered = |v: Rect| {
        ui.hits.iter().filter(|(_, h)| *h == Hit::Dialog).map(|(r, _)| r.intersection(v).area()).max().unwrap_or(0)
    };
    ui.viz_shown = ui.viz_drawn_at.is_some_and(|v| (v.area() - covered(v)) * 4 >= v.area());
    // Scrolling the lyrics panel did not draw (the too-small screen, a panel
    // too small for a row) is dropped: kept, held keys piled up and landed
    // all at once when it came back.
    ui.lyrics_step = 0;
    // Judged after the draw: whether the open panel floats or docked is
    // settled while drawing (`docked_shown`). Judged before, the frame a
    // panel docked (or lost its room) went by the last frame's layout.
    // Images a dialog covers are held back (see `tui::withhold_images_under`);
    // the dialogs' boxes are their `Hit::Dialog`s.
    let withheld = if overlay_open(ui) && !ui.too_small {
        let boxes: Vec<Rect> = ui.hits.iter().filter(|(_, h)| *h == Hit::Dialog).map(|(r, _)| *r).collect();
        tui::withhold_images_under(&boxes)
    } else {
        Vec::new()
    };
    let kitty = matches!(crate::cover::detect_protocol(), crate::cover::GraphicsProtocol::Kitty);
    note_withheld(ui, withheld, kitty);
}

/// Whether a dialog opening or closing must repaint the whole screen: only
/// when an image is involved — one on screen that may go under it, or one
/// held back that comes back. Every open and close repainted, and with no
/// image anywhere (half-block, no cover) "Quit?" or `l` sent a whole screen
/// through ConPTY for nothing.
pub fn overlay_change_repaints(ui: &UiState) -> bool {
    !ui.images_withheld.is_empty() || tui::images_on_screen()
}

/// This frame's held-back images (`withheld`) against the last frame's.
fn note_withheld(ui: &mut UiState, withheld: Vec<tui::ImageSlot>, kitty: bool) {
    for &slot in withheld.iter().filter(|s| !ui.images_withheld.contains(s)) {
        // Newly covered, once: a Kitty image is drawn ABOVE text, so it is
        // deleted (by id) or it covers the dialog; and its block is marked
        // to be sent again (every frame, it was rebuilt only to be
        // withheld). Closing the dialog repaints, which sends it.
        match slot {
            tui::ImageSlot::Cover => {
                if kitty {
                    crate::term::out!("{}", crate::cover::kitty_clear_escape());
                }
                ui.cover_block_intact = false;
            }
            tui::ImageSlot::Viz => {
                if kitty {
                    crate::term::out!("{}", crate::cover::viz_image_clear_escape());
                }
                ui.spectro_block_intact = false;
            }
        }
    }
    ui.images_withheld = withheld;
}

fn draw_frame(f: &mut Frame, cx: &mut Ctx) {
    use crate::theme::ThemeKind;
    let state = cx.state;
    // Keep the live EQ bands mirroring the selected preset while not editing, so
    // the curve/editor show the active shape and edits start from it.
    if !state.is_eq_custom() {
        state.set_eq_bands(&cx.eq_preset.bands_10());
        state.set_eq_preamp_db(cx.eq_preset.preamp);
    }
    cx.ui.hits.clear();
    // The view on screen: the key list and the EQ editor float over the
    // view they were opened from.
    let base = under_help(cx.ui);
    // A frame without the player's cover (HiFi has none, the too-small
    // screen) has to take it away. A Kitty image is an OVERLAY bound to an
    // image id, not cell content, so drawing text over its cells does not
    // remove it. (Sixel and iTerm2 go when the frame sees their cells change
    // hands, `tui::stale_image_cells`; half-block is cells.) Every view draws
    // the player under its panels and dialogs, so the view does not matter
    // here — judging by it deleted and re-sent Minimal's cover every frame a
    // panel had the keys. Under a dialog, `draw` holds the images back.
    let ui = &mut *cx.ui;
    let cover_view = !ui.too_small && state.theme_kind() != ThemeKind::HiFi;
    if !cover_view && ui.cover_block_intact {
        if matches!(crate::cover::detect_protocol(), crate::cover::GraphicsProtocol::Kitty) {
            crate::term::out!("{}", crate::cover::kitty_clear_escape());
        }
        ui.cover_block_intact = false;
        ui.cover_dirty_frame = true;
    }

    // The panel with the keys is the one used last (it keeps its room first).
    if matches!(ui.view_mode, ViewMode::Playlist | ViewMode::Lyrics) && ui.panel_recent.first() != Some(&ui.view_mode) {
        let v = ui.view_mode;
        ui.panel_recent.retain(|&r| r != v);
        ui.panel_recent.insert(0, v);
    }
    if ui.too_small {
        // The last real frame's record stays (`open_view`), but not for a
        // panel undocked since (`k`, `Shift+K` here): kept, the tree went on
        // rebuilding for a playlist no longer docked, and `overlay_open`
        // took a floating panel for a docked one.
        let UiState { docked, docked_shown, .. } = &mut *ui;
        docked_shown.retain(|v| docked.contains(v));
        draw_too_small(f, cx);
        return;
    }
    ui.docked_shown.clear();
    if base == ViewMode::Eq {
        draw_eq_panel(f, cx);
    } else {
        draw_view(f, cx, base);
    }

    // Dialogs, over whatever is drawn.
    let p = crate::theme::palette(state.theme_kind());
    if cx.ui.view_mode == ViewMode::Help {
        let area = f.area();
        // Laid out for the dialog's inside first: its title says when the
        // window cuts the list (the layout's own title row, which said so,
        // is the dialog's border here).
        let (w, h) = (area.width.saturating_sub(4), area.height.saturating_sub(2));
        let (lines, cut) = help_layout(p, w.min(area.width).saturating_sub(3) as usize, h.min(area.height) as usize);
        let title = if cut { "Keys · cut: widen the window · ? or Esc closes" } else { "Keys · ? or Esc closes" };
        let inner = dialog(f, &mut cx.ui.hits, w, h, title, p);
        // (Its title row is the dialog's border here. A window too narrow
        // for any of it lays out nothing at all.)
        tui::draw_lines(f.buffer_mut(), inner, lines.get(1..).unwrap_or_default());
    }
    if let InputMode::OpenPath(field) = &cx.ui.input_mode {
        let field = field.clone();
        let error = cx.ui.open_error.clone();
        draw_open_dialog(f, &mut cx.ui.hits, &field, state.theme_kind(), p, error);
    }
    if let Some((label, paths)) = &cx.ui.tree_pending_remove {
        let keys = key_hints(state.theme_kind(), p, &[("y", "remove"), ("any other key", "keep them")]);
        let lines = [
            tui::line(label.clone(), p.bold.patch(p.fg)),
            tui::line(
                match paths.len() {
                    1 => "1 track leaves the playlist".to_string(),
                    n => format!("{n} tracks leave the playlist"),
                },
                p.fg,
            ),
            Line::default(),
            keys.line.clone(),
        ];
        let inner = dialog(f, &mut cx.ui.hits, 56, 6, "Remove", p);
        keys.register(&mut cx.ui.hits, inner.x, inner.y + 3);
        tui::draw_lines(f.buffer_mut(), inner, &lines);
    }
    if quit_armed(cx.ui) {
        let keys = key_hints(state.theme_kind(), p, &[("Esc", "or q quits"), ("any other key", "stays")]);
        let lines = [tui::line("Quit Keet?", p.bold.patch(p.fg)), Line::default(), keys.line.clone()];
        let inner = dialog(f, &mut cx.ui.hits, 48, 5, "Quit", p);
        keys.register(&mut cx.ui.hits, inner.x, inner.y + 2);
        tui::draw_lines(f.buffer_mut(), inner, &lines);
    }
}

/// How a theme draws key hints (`hints::Look`): Classic's keys bold in
/// its signal colour, Minimal's plain, HiFi's in the accent with a dim dot
/// between them.
pub(crate) fn key_look(kind: crate::theme::ThemeKind, p: &crate::theme::Palette) -> crate::hints::Look {
    use crate::theme::ThemeKind;
    match kind {
        ThemeKind::Classic => crate::hints::Look { key: p.accent.patch(p.bold), label: p.dim, sep: Line::raw("  ") },
        ThemeKind::Minimal => crate::hints::Look { key: p.fg, label: p.dim, sep: Line::raw("   ") },
        ThemeKind::HiFi => {
            let mut sep = LineBuf::new();
            sep.push(' ').styled("·", p.dim).reset().push(' ');
            crate::hints::Look { key: p.accent, label: p.dim, sep: sep.into_line() }
        }
    }
}

/// `key label` pairs for a dialog's last line, in the theme's key-bar look
/// (`key_look`: its own copy lost Classic's bold keys and HiFi's dots), and
/// clickable once registered.
fn key_hints(kind: crate::theme::ThemeKind, p: &crate::theme::Palette, keys: &[(&str, &str)]) -> crate::hints::Bar {
    crate::hints::bar(keys, &key_look(kind, p), usize::MAX)
}

/// The first Esc in the player asked "Quit?" and the second has not come.
fn quit_armed(ui: &UiState) -> bool {
    ui.esc_quit_until.is_some_and(|t| std::time::Instant::now() < t)
}

/// Whether a dialog floats over the frame (no image goes under one, see
/// `tui::withhold_images`). The frame loop repaints when this changes, so the
/// images go when one opens and come back when it closes.
pub(crate) fn overlay_open(ui: &UiState) -> bool {
    let floating_panel = matches!(ui.view_mode, ViewMode::Playlist | ViewMode::Lyrics) && !ui.docked_shown.contains(&ui.view_mode);
    ui.input_mode.is_overlay()
        || floating_panel
        || matches!(ui.view_mode, ViewMode::Help | ViewMode::Eq)
        || ui.tree_pending_remove.is_some()
        || quit_armed(ui)
}

/// Where the visualization and the docked panels go in the space below now
/// playing (`region`): side by side, the visualization on the left.
pub(crate) struct Dock {
    pub viz: Option<Rect>,
    pub panels: Vec<(ViewMode, Rect)>,
}

/// The fewest columns each needs to be worth drawing.
/// Whether `view` can dock in a `window`: in HiFi, below the knob rack, a
/// dock area at least the panel's `dock_min_width` wide (only its existence
/// was checked: `k` said "docked" for a panel `dock` could never place).
/// Classic and Minimal dock into the visualization slot, where `dock` hides
/// what does not fit for now.
fn dock_room(state: &PlayerState, view: ViewMode, window: Rect) -> bool {
    if state.theme_kind() != crate::theme::ThemeKind::HiFi {
        return true;
    }
    crate::ui_hifi::dock_region(window).is_some_and(|r| r.width >= dock_min_width(Some(view)))
}

fn dock_min_width(item: Option<ViewMode>) -> u16 {
    match item {
        None => 40, // the visualization
        Some(ViewMode::Playlist) => 50,
        _ => 36,
    }
}

/// The panels `config.json`'s `docked` names, each once (`library` is the
/// playlist's other name: listing both docked it twice, and `k` then had to
/// be pressed twice to float it); and a problem for each name that is not a
/// panel.
pub fn docked_from_config(names: &[String]) -> (Vec<ViewMode>, Vec<String>) {
    let mut docked = Vec::new();
    let mut problems = Vec::new();
    for name in names {
        let view = match name.to_ascii_lowercase().as_str() {
            "playlist" | "library" => ViewMode::Playlist,
            "lyrics" => ViewMode::Lyrics,
            _ => {
                problems.push(format!("docked ({name:?} is not a panel)"));
                continue;
            }
        };
        if !docked.contains(&view) {
            docked.push(view);
        }
    }
    (docked, problems)
}

/// The fewest rows a docked panel needs.
const DOCK_MIN_ROWS: u16 = 6;

/// Share `region` between the visualization (when `viz_on`) and the docked
/// panels. Each gets its minimum width in priority order — the panel used
/// last, then the visualization, then the other panel — until the row is
/// full; what does not fit is left out (a docked panel left out floats when
/// opened). The spare width is shared in proportion to the minimums.
pub(crate) fn dock(ui: &UiState, viz_on: bool, region: Rect) -> Dock {
    let docked: Vec<ViewMode> = ui.panel_recent.iter().copied().filter(|v| ui.docked.contains(v)).collect();
    if docked.is_empty() || region.height < DOCK_MIN_ROWS {
        return Dock { viz: viz_on.then_some(region), panels: Vec::new() };
    }
    let mut priority: Vec<Option<ViewMode>> = vec![Some(docked[0])];
    if viz_on {
        priority.push(None);
    }
    priority.extend(docked[1..].iter().map(|&v| Some(v)));
    let mut chosen = Vec::new();
    let mut used = 0u16;
    for item in priority {
        let need = dock_min_width(item) + u16::from(!chosen.is_empty());
        if used + need <= region.width {
            used += need;
            chosen.push(item);
        }
    }
    // Nothing fits (a narrow window): the slot stays the visualization's —
    // the panels open floating — rather than going blank.
    if chosen.is_empty() {
        return Dock { viz: viz_on.then_some(region), panels: Vec::new() };
    }
    // Left to right: the visualization, the lyrics, the playlist.
    let rank = |i: &Option<ViewMode>| match i {
        None => 0,
        Some(ViewMode::Lyrics) => 1,
        _ => 2,
    };
    chosen.sort_by_key(rank);
    let gaps = chosen.len().saturating_sub(1) as u16;
    let min_total: u16 = chosen.iter().map(|&i| dock_min_width(i)).sum();
    let spare = region.width.saturating_sub(min_total + gaps);
    let mut x = region.x;
    let mut out = Dock { viz: None, panels: Vec::new() };
    for (n, item) in chosen.iter().enumerate() {
        let min = dock_min_width(*item);
        let w = if n + 1 == chosen.len() {
            region.right().saturating_sub(x)
        } else {
            min + (spare as u32 * min as u32 / min_total.max(1) as u32) as u16
        };
        let r = Rect { x, width: w, ..region };
        match item {
            None => out.viz = Some(r),
            Some(v) => out.panels.push((*v, r)),
        }
        x += w + 1;
    }
    out
}

/// Draw the docked panels `dock` placed: each in a box, its border lit when
/// it has the keys, clickable to give them to it.
pub(crate) fn draw_docked(f: &mut Frame, cx: &mut Ctx, panels: &[(ViewMode, Rect)]) {
    use ratatui::widgets::{Block, Clear};
    let kind = cx.state.theme_kind();
    let p = crate::theme::palette(kind);
    for &(view, at) in panels {
        let focused = cx.ui.focus == view;
        let title = if view == ViewMode::Playlist {
            crate::playlist_panel::title(kind, cx.ui, cx.playlist.len())
        } else {
            crate::lyrics_panel::title(kind, cx.ui)
        };
        let border = if focused { p.accent } else { p.rule };
        let block = Block::bordered()
            .border_style(border)
            .title(ratatui::text::Span::styled(format!(" {title} "), if focused { p.bold.patch(p.accent) } else { p.dim }));
        let inner = tui::indent(block.inner(at), 1);
        f.render_widget(Clear, at);
        f.render_widget(block, at);
        cx.ui.hits.push((at, Hit::Panel(view)));
        cx.ui.docked_shown.push(view);
        if view == ViewMode::Playlist {
            crate::playlist_panel::draw(f, cx, inner);
        } else {
            crate::lyrics_panel::draw(f, cx, inner);
        }
    }
}

/// A theme's status line: the frame's status message in `row` (after a
/// 2-column indent), and where it landed on screen, for `border_status`.
/// One function for every theme: four hand-kept copies of "draw it, then
/// say where" let one record a row off the screen (HiFi's 21st in a short
/// window) and the message showed nowhere. Returns whether there was one.
pub(crate) fn draw_status_line(f: &mut Frame, ui: &mut UiState, row: Rect, p: &crate::theme::Palette) -> bool {
    let Some(msg) = ui.frame_status.as_deref() else { return false };
    // Cut to the row with an ellipsis (a long one, a device name and an
    // explanation, must not wrap or end mid-word unmarked).
    let line = tui::truncate_ellipsis(tui::line(format!("  {msg}"), p.accent), row.width as usize);
    // The caller's row, cut to the window (a row clamped to nothing stays
    // nothing: never the row below the caller's area).
    let shown = Rect { width: line.cols().min(row.width as usize) as u16, height: row.height.min(1), ..row }.intersection(f.area());
    tui::draw_lines(f.buffer_mut(), shown, &[line]);
    ui.status_drawn_at = (!shown.is_empty()).then_some(shown);
    true
}

/// A status message in the bottom border of a FLOATING panel (`inner`: the
/// box's inside), when the box covers the player's own status line (some of
/// it at least: a message cut by the box does not read). Beside a docked
/// panel, or a floating one narrower than the window, that line is in view
/// and shows it: both showed it twice. Drawn in the body it hid a row — the
/// list's cursor track, which a click on it then played, or a short panel's
/// only lyric — and given a row of its own it shrank the body for 2 s, so
/// the list and the lyrics jumped. Also while a field is open (the border is
/// not its row; skipped, a notice ran out unseen while a search was typed).
/// Not in a box with no inside (its bottom border is not below it).
fn border_status(f: &mut Frame, ui: &UiState, boxed: Rect, inner: Rect, p: &crate::theme::Palette) {
    let Some(msg) = ui.frame_status.as_deref() else { return };
    if inner.width < 4 || inner.height == 0 {
        return;
    }
    if ui.status_drawn_at.is_some_and(|line| line.intersection(boxed).is_empty()) {
        return;
    }
    let row = Rect { y: inner.bottom(), height: 1, ..inner };
    if row.y >= f.area().bottom() {
        return;
    }
    let line = tui::truncate_ellipsis(tui::line(format!(" {msg} "), p.bold.patch(p.accent)), row.width as usize);
    tui::draw_lines(f.buffer_mut(), row, &[line]);
}

/// The theme's renderer for `view`, as if it were the current view (the key
/// list draws over the view it was opened from).
fn draw_view(f: &mut Frame, cx: &mut Ctx, view: ViewMode) {
    use crate::theme::ThemeKind;
    // The playlist and the lyrics: docked into the player's layout when
    // there is room for them there, else floating over it.
    if matches!(view, ViewMode::Playlist | ViewMode::Lyrics) {
        draw_view(f, cx, ViewMode::Player);
        if cx.ui.docked_shown.contains(&view) {
            return;
        }
        // Set where it is drawn: under the key list or the EQ editor the
        // view is theirs, and the panel is still up.
        cx.ui.floating_shown = Some(view);
        let kind = cx.state.theme_kind();
        let p = crate::theme::palette(kind);
        let area = f.area();
        let h = area.height.saturating_sub(2);
        if view == ViewMode::Playlist {
            let title = crate::playlist_panel::title(kind, cx.ui, cx.playlist.len());
            let (boxed, inner) = dialog_box(f, &mut cx.ui.hits, area.width.saturating_sub(4).min(160), h, &title, p);
            crate::playlist_panel::draw(f, cx, inner);
            border_status(f, cx.ui, boxed, inner, p);
        } else {
            let title = crate::lyrics_panel::title(kind, cx.ui);
            let (boxed, inner) = dialog_box(f, &mut cx.ui.hits, area.width.saturating_sub(4).min(90), h, &title, p);
            crate::lyrics_panel::draw(f, cx, inner);
            border_status(f, cx.ui, boxed, inner, p);
        }
        return;
    }
    // The theme draws the player; docked panels read who has the keys from
    // `focus` (the real view).
    let current = std::mem::replace(&mut cx.ui.view_mode, view);
    cx.ui.focus = current;
    match cx.state.theme_kind() {
        ThemeKind::Minimal => crate::ui_minimal::draw(f, cx),
        ThemeKind::HiFi => crate::ui_hifi::draw(f, cx),
        ThemeKind::Classic => crate::ui_classic::draw(f, cx),
    }
    cx.ui.view_mode = current;
    // After the theme: whether it drew its status line is known only now.
    let p = crate::theme::palette(cx.state.theme_kind());
    docked_status(f, cx.ui, p);
}

/// The status message in the border of the docked panel with the keys, when
/// nothing else showed it (the theme's status line not on the screen). The
/// footers used to carry it; without this a notice could run out unseen.
fn docked_status(f: &mut Frame, ui: &UiState, p: &crate::theme::Palette) {
    if ui.status_drawn_at.is_some() || !ui.docked_shown.contains(&ui.view_mode) {
        return;
    }
    let v = ui.view_mode;
    let Some(boxed) = ui.hits.iter().rev().find_map(|(r, h)| (*h == Hit::Panel(v)).then_some(*r)) else { return };
    // `draw_docked`'s inside: the border, then a column of margin.
    let inside = Rect { x: boxed.x + 1, y: boxed.y + 1, width: boxed.width.saturating_sub(2), height: boxed.height.saturating_sub(2) };
    border_status(f, ui, boxed, tui::indent(inside, 1), p);
}

/// A box centred over the view, `w`×`h` cells at most, cut to the window:
/// cleared, framed in the theme's accent, titled. Returns its inside.
pub(crate) fn dialog(f: &mut Frame, hits: &mut Vec<(Rect, Hit)>, w: u16, h: u16, title: &str, p: &crate::theme::Palette) -> Rect {
    dialog_box(f, hits, w, h, title, p).1
}

/// `dialog`, returning the box itself too (a floating panel's border
/// status needs it).
pub(crate) fn dialog_box(f: &mut Frame, hits: &mut Vec<(Rect, Hit)>, w: u16, h: u16, title: &str, p: &crate::theme::Palette) -> (Rect, Rect) {
    use ratatui::widgets::{Block, BorderType, Clear};
    let area = f.area();
    let (w, h) = (w.min(area.width), h.min(area.height));
    let at = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(p.accent)
        .title(ratatui::text::Span::styled(format!(" {title} "), p.bold.patch(p.accent)));
    let inner = block.inner(at);
    hits.push((at, Hit::Dialog));
    f.render_widget(Clear, at);
    f.render_widget(block, at);
    (at, tui::indent(inner, 1))
}
/// The `o` dialog: a folder, file or M3U to open. A path that does not
/// exist keeps it open with the reason under the field.
fn draw_open_dialog(f: &mut Frame, hits: &mut Vec<(Rect, Hit)>, field: &TextInput, kind: crate::theme::ThemeKind, p: &crate::theme::Palette, error: Option<String>) {
    let inner = dialog(f, hits, 72, 7, "Open", p);
    let width = inner.width.saturating_sub(1) as usize;
    let (line, col) = field_line(tui::line("› ", p.accent), field, width);
    let note = match error {
        Some(msg) => tui::truncate_ellipsis(tui::line(msg, p.warn), inner.width as usize),
        None => {
            // A key bar, not text: its keys are clickable (written by hand,
            // they were not).
            let keys = key_hints(kind, p, &[("Enter", "open"), ("Esc", "cancel")]);
            keys.register(hits, inner.x, inner.y + 3);
            let mut l = LineBuf::new();
            l.append(keys.line).styled("  drop a folder here", p.dim);
            l.into_line()
        }
    };
    let lines = [tui::line("A folder, a file or an M3U playlist:", p.dim), line, Line::default(), note];
    tui::draw_lines(f.buffer_mut(), inner, &lines);
    let area = f.area();
    show_cursor(f, (inner.x - area.x) as usize + col, (inner.y - area.y) as usize + 1);
}

/// The smallest window Keet lays out in. Below it every theme shows one calm
/// message instead of a frame cut to pieces; playback and keys keep working.
pub const MIN_WINDOW: (usize, usize) = (40, 10);

/// Whether a `w`×`h` window is below [`MIN_WINDOW`]. 0 = size unknown (some
/// ptys report 0×0), which is never "too small".
pub fn window_too_small(w: usize, h: usize) -> bool {
    (w != 0 && w < MIN_WINDOW.0) || (h != 0 && h < MIN_WINDOW.1)
}

/// The "window too small" screen: what is wrong, the size needed, what is
/// playing, and the keys that still matter. Centred, cut to `w`×`h`.
pub(crate) fn too_small_lines(
    w: usize,
    h: usize,
    paused: bool,
    title: &str,
    time: (f64, f64),
    p: &crate::theme::Palette,
) -> Vec<Line<'static>> {
    let icon = if paused { "⏸" } else { "▶" };
    let rows: [(String, ratatui::style::Style); 7] = [
        ("▲ window too small".into(), p.warn),
        (format!("{w}×{h} · Keet needs {}×{}", MIN_WINDOW.0, MIN_WINDOW.1), p.dim),
        (String::new(), p.fg),
        (format!("{icon} {}", truncate_plain(title, w.saturating_sub(4))), p.fg),
        (format!("{} / {}", format_time(time.0), format_time(time.1)), p.accent),
        (String::new(), p.fg),
        ("space pause · ↑↓ track · q quit".into(), p.dim),
    ];
    let top = h.saturating_sub(rows.len()) / 2;
    let mut out = vec![Line::default(); top];
    for (text, colour) in rows {
        let text = truncate_plain(&text, w);
        let pad = w.saturating_sub(tui::width(&text)) / 2;
        let mut line = LineBuf::new();
        line.spaces(pad).styled(&text, colour);
        out.push(line.into_line());
    }
    out.truncate(h.saturating_sub(1).max(1));
    out
}

/// The too-small screen, from the top of the window.
fn draw_too_small(f: &mut Frame, cx: &Ctx) {
    let area = f.area();
    let (w, h) = (area.width as usize, area.height as usize);
    let idx = cx.state.current_track.load(Ordering::Relaxed);
    let title = cx.ui.metadata_cache.title(idx).unwrap_or_else(|| cx.name.to_string());
    let p = crate::theme::palette(cx.state.theme_kind());
    let lines = too_small_lines(w, h, cx.state.is_paused(), &title, (cx.state.time_secs(), cx.state.total_secs()), p);
    tui::draw_lines(f.buffer_mut(), area, &lines);
}

/// The EQ+FX editor: one shared, palette-driven dialog for all themes,
/// centred over the view it was opened from, which keeps running under it
/// (the visualization included) — the editor is the focus.
fn draw_eq_panel(f: &mut Frame, cx: &mut Ctx) {
    let p = crate::theme::palette(cx.state.theme_kind());
    draw_view(f, cx, cx.ui.eq_return);
    let area = f.area();
    let inner = dialog(f, &mut cx.ui.hits, area.width.saturating_sub(4).min(104), area.height.saturating_sub(2).min(28), "EQ · e or Esc closes", p);
    let (lines, keys) = eq_lines(cx, p, inner.width as usize, inner.height as usize);
    if let Some(row) = lines.len().checked_sub(1) {
        keys.register(&mut cx.ui.hits, inner.x, inner.y + row as u16);
    }
    tui::draw_lines(f.buffer_mut(), inner, &lines);
}

/// The EQ editor's lines for a `w`×`h` space: sliders, labels, the selected
/// band, the readouts, the headroom row and the key line (last; its keys
/// returned to register where it is drawn).
fn eq_lines(cx: &Ctx, p: &crate::theme::Palette, w: usize, h: usize) -> (Vec<Line<'static>>, crate::hints::Bar) {
    let state = cx.state;
    let knob = match state.theme_kind() {
        crate::theme::ThemeKind::Minimal => '●',
        crate::theme::ThemeKind::HiFi => '◆',
        crate::theme::ThemeKind::Classic => '█',
    };
    let title = if state.is_eq_custom() { "Custom".to_string() } else { cx.eq_preset.name.clone() };
    let bal = state.balance_value();
    let bal_str = if bal == 0 {
        "centred".to_string()
    } else if bal < 0 {
        format!("L{}%", -bal)
    } else {
        format!("R{}%", bal)
    };
    let rg_str = match state.rg_mode() {
        crate::state::RgMode::Album => "album",
        crate::state::RgMode::Off => "off",
        crate::state::RgMode::Track => "track",
    };
    let pre_db = state.eq_preamp_db();
    // The preamp is on the headroom row, beside the boost it has to cover.
    let readouts = vec![("FX", cx.fx_name), ("XFEED", cx.cf_name), ("BAL", bal_str.as_str()), ("RG", rg_str)];
    let bands = state.eq_bands_array();
    let mut body = crate::eq_ui::render_eq_screen(&bands, cx.ui.eq_band, &title, &readouts, knob, p, w, h.saturating_sub(2));
    let out_rate = match state.output_rate.load(Ordering::Relaxed) {
        0 => 48_000.0,
        r => r as f32,
    };
    let headroom = crate::eq::headroom(&bands, pre_db, out_rate);
    body.push(crate::eq_ui::headroom_line(&headroom, pre_db, p));
    const KEYS: &[(&str, &str)] = &[
        ("←→", "band"),
        ("↑↓", "gain (⇧ fine)"),
        ("t", "type"),
        (",.", "Q"),
        ("<>", "freq"),
        ("[]", "preset"),
        ("0", "reset"),
        ("e", "close"),
    ];
    let look = key_look(cx.state.theme_kind(), p);
    let keys = crate::hints::indent(crate::hints::bar(KEYS, &look, w.saturating_sub(2)), 2);
    (fit_eq_screen(body, keys.line.clone(), w, h), keys)
}

/// The `?` screen: every key (`cli::KEYS`), sections flowed into as many
/// columns as the window holds, cut to its height; and whether that cut
/// anything (the dialog's title then says so).
fn help_layout(p: &crate::theme::Palette, term_w: usize, term_h: usize) -> (Vec<Line<'static>>, bool) {
    const KEY_W: usize = 12;
    const GAP: usize = 2;
    let widest = crate::cli::KEYS
        .iter()
        .flat_map(|(_, keys)| keys.iter())
        .map(|(_, what)| KEY_W + 1 + tui::width(what))
        .max()
        .unwrap_or(40);
    // Each section as its lines: title, then one row per key.
    let sections: Vec<Vec<Line<'static>>> = crate::cli::KEYS
        .iter()
        .map(|(title, keys)| {
            let mut v = vec![tui::line(*title, p.accent.patch(p.bold))];
            v.extend(keys.iter().map(|(k, what)| {
                let mut l = LineBuf::new();
                l.styled(&format!("{k:<KEY_W$}"), p.fg).reset().push(' ').styled(what, p.dim);
                l.into_line()
            }));
            v.push(Line::default());
            v
        })
        .collect();
    let body_h = term_h.saturating_sub(3).max(1); // title row, footer, slack
    // As many columns as fit side by side, `GAP` apart.
    let cols = ((term_w.saturating_sub(2) + GAP) / (widest + GAP)).max(1);
    let col_w = widest + GAP;
    // Fill columns top to bottom, a section kept whole where it fits.
    let mut columns: Vec<Vec<Line<'static>>> = vec![Vec::new()];
    for sec in sections {
        let last = columns.last_mut().expect("one column");
        if !last.is_empty() && last.len() + sec.len() > body_h && columns.len() < cols {
            columns.push(Vec::new());
        }
        columns.last_mut().expect("one column").extend(sec);
    }
    // Say so when the window cuts the list short, rather than hiding keys.
    let cut = columns.iter().any(|c| c.iter().skip(body_h).any(|l| l.cols() > 0));
    let more = if cut { "  ·  widen the window for the rest" } else { "" };
    let mut title = LineBuf::new();
    title.push_str("  ").styled("K E Y S", p.accent.patch(p.bold)).reset().push_str("   ").styled(&format!("? or Esc closes{more}"), p.dim);
    let mut out = vec![tui::truncate(title.into_line(), term_w)];
    for row in 0..body_h {
        let mut line = LineBuf::new();
        line.push_str("  ");
        for (c, col) in columns.iter().enumerate() {
            let cell = col.get(row).cloned().unwrap_or_default();
            let w = cell.cols();
            line.append(cell);
            if c + 1 < columns.len() {
                line.reset().spaces(col_w.saturating_sub(w));
            }
        }
        out.push(tui::truncate(line.into_line(), term_w));
    }
    while out.last().is_some_and(|l| tui::plain(l).trim().is_empty()) {
        out.pop();
    }
    (out, cut)
}

/// Fit the EQ editor into `avail` rows of `term_w` columns: every line cut to
/// the width, the body cut to leave room for the footer (the key bar, which
/// `hints::bar` already fitted to the width).
fn fit_eq_screen(body: Vec<Line<'static>>, footer: Line<'static>, term_w: usize, avail: usize) -> Vec<Line<'static>> {
    if avail == 0 {
        return Vec::new();
    }
    let mut out: Vec<Line<'static>> = body
        .into_iter()
        .take(avail - 1)
        .map(|l| tui::truncate(l, term_w))
        .collect();
    out.push(tui::truncate(footer, term_w));
    out
}
/// How long a first Esc in the player waits for the second that quits.
const ESC_QUIT_WINDOW: Duration = Duration::from_secs(2);

/// Open the key list, or close it back to the view it was opened from.
/// Opening it answers a pending "remove …? [y/n]" prompt with no — left
/// armed, a `y` long after the prompt has gone would remove a whole artist.
fn toggle_help(ui: &mut UiState) {
    if ui.view_mode == ViewMode::Help {
        ui.view_mode = ui.help_return;
    } else {
        if let Some((label, _)) = ui.tree_pending_remove.take() {
            ui.set_status(format!("cancelled removing {label}"));
        }
        ui.help_return = ui.view_mode;
        ui.view_mode = ViewMode::Help;
    }
    // No forced repaint: a dialog, so `Player::repaint_screen` repaints when
    // an image is involved (`overlay_change_repaints`), like the others.
}

/// Whether an Esc at `now` is the confirming second press.
fn esc_confirms_quit(armed_until: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    armed_until.is_some_and(|t| now < t)
}

/// How long a bare Esc waits for the rest of a sequence. The loop wakes on
/// the first byte of input now (it used to sleep through a tick and find
/// both halves queued), so a terminal writing ESC apart from the byte after
/// it split a sequence into a lone Esc ("Quit?", a panel closed) and a key.
/// Too short for a person to type a second key in.
const ESC_SEQUENCE_WAIT: Duration = Duration::from_millis(10);

/// Whether the event right behind a bare Esc means the Esc was the first
/// byte of an escape sequence (macOS Cmd+Arrow arrives as ESC + another key
/// press), rather than a real Esc tap.
fn esc_starts_sequence(next: &Event) -> bool {
    matches!(next, Event::Key(k) if k.kind == KeyEventKind::Press)
}

/// A bare Esc with `next` already waiting: whether the two make an escape
/// sequence (both dropped). Otherwise the Esc is real, and `next` is kept
/// for after it (a resize recorded; a click, a wheel notch, a paste handled
/// next — they were thrown away).
fn esc_then(ui: &mut UiState, next: Event) -> bool {
    if esc_starts_sequence(&next) {
        return true;
    }
    match next {
        Event::Resize(_, _) => ui.terminal_resized = true,
        Event::Key(_) => {} // a release: nothing to do
        other => ui.deferred_event = Some(other),
    }
    false
}

/// Whether Keet holds input the terminal's poll cannot see: a clicked
/// hint's key (`ui.pressed`) or the event kept after an Esc
/// (`ui.deferred_event`). The frame loop does not wait then.
pub fn input_waiting(ui: &UiState) -> bool {
    !ui.pressed.is_empty() || ui.deferred_event.is_some()
}

pub fn poll_input(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) -> bool {
    serve_producer(state, ui, playlist);
    // Drain all pending events for responsive input
    loop {
        // A key a click pressed (a key hint) first, as if just typed; then
        // the terminal's events.
        let (ev, clicked) = if let Some(k) = ui.pressed.pop_front() {
            (Event::Key(k), true)
        } else if let Some(e) = ui.deferred_event.take() {
            (e, false)
        } else if event::poll(Duration::ZERO).unwrap_or(false) {
            match event::read() { Ok(e) => (e, false), Err(_) => continue }
        } else {
            break;
        };
        // A mouse moving over the window changes nothing on screen.
        if let Event::Mouse(m) = &ev {
            if matches!(m.kind, event::MouseEventKind::Moved) {
                continue;
            }
            ui.dirty = true;
            mouse(state, ui, playlist, *m);
            continue;
        }
        // A key release changes nothing (Windows reports one for every key:
        // each keystroke drew two frames).
        if !matches!(&ev, Event::Key(k) if k.kind != KeyEventKind::Press) {
            ui.dirty = true;
        }

        if let Event::Resize(_, _) = ev {
            ui.terminal_resized = true;
            continue;
        }
        if let Event::Paste(text) = &ev {
            // Closes "Quit?" as any other key does: under it, a drop opened
            // the `o` dialog, Esc then cancelled that (losing the path) and
            // the next Esc quit.
            ui.esc_quit_until = None;
            paste(ui, playlist, text);
            continue;
        }

        let k = match ev {
            Event::Key(k) => k,
            _ => continue,
        };
        if k.kind != KeyEventKind::Press {
            continue;
        }

        // macOS terminals translate Cmd+Arrow (and similar shortcuts) into an ESC
        // byte followed by another char — crossterm hands us two separate events.
        // If a bare Esc is immediately followed by another pending event, treat
        // the pair as an unrecognized escape sequence and drop both. Human typing
        // rarely produces 0ms gaps, so a zero-duration poll returning true here
        // is a reliable signal.
        //
        // Only another key PRESS makes it a sequence. Windows (crossterm on
        // ConPTY) reports a Release for every key, so a quick Esc tap arrives
        // as Press+Release inside one 50 ms tick — treating that Release as
        // "the second half" threw most Esc taps away (leaving views, cancelling
        // search, quitting). A waiting resize is recorded, not swallowed.
        if k.code == KeyCode::Esc
            && !clicked
            && k.modifiers.is_empty()
            && event::poll(ESC_SEQUENCE_WAIT).unwrap_or(false)
        {
            if let Ok(next) = event::read() {
                if esc_then(ui, next) {
                    continue;
                }
            }
        }

            // The "Quit?" dialog: a second Esc quits (below), any other
            // key closes it and does what it does.
            if k.code != KeyCode::Esc {
                ui.esc_quit_until = None;
            }

            // A clicked key hint is an action, never typing: a click on the
            // player's `q` or `␣` typed it into the search. The field's own
            // keys (Enter, Esc) still go to it; any other clicked key closes
            // the field and acts.
            if field_has_keys(ui) && clicked && !matches!(k.code, KeyCode::Enter | KeyCode::Esc) {
                cancel_field(ui);
            }
            // On to the next event (only a quit returns): returning after
            // each typed key cost a loop pass and a frame per character — a
            // path dropped on Windows arrives as keys — and held up what was
            // queued behind it.
            if field_has_keys(ui) {
                if handle_text_input(state, ui, playlist, k) {
                    return true;
                }
                continue;
            }
            // Commands only (a field above takes keys as they came).
            let k = shift_to_capital(k);

            // A staged bulk remove is awaiting confirmation: `y` removes, any
            // other key cancels — but Ctrl+C still quits. Answered before any
            // view's keys: handled inside the tree's, Tab flipped to the list
            // with the prompt still up, and a `y` much later removed a whole
            // artist.
            if let Some((label, paths)) = ui.tree_pending_remove.take() {
                if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                    state.quit();
                    return true;
                }
                if matches!(k, KeyEvent { code: KeyCode::Char('y'), .. }) {
                    let indices = indices_of(playlist, &paths);
                    tree_remove_indices(state, ui, playlist, &indices);
                } else {
                    ui.set_status(format!("cancelled removing {label}"));
                }
                return false;
            }

            // Lyrics view keys (when in Normal input mode), never with Ctrl or
            // Alt: Ctrl+D pages the playlist, and pressed here by habit it
            // shifted the lyrics half a second and saved that for the track.
            if ui.view_mode == ViewMode::Lyrics && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                match k {
                    // Screen rows (wrapped lines take several), applied by
                    // the next frame (`UiState::lyrics_step`).
                    KeyEvent { code: KeyCode::Char('w'), .. } => {
                        ui.scroll_lyrics(-1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('s'), .. } => {
                        ui.scroll_lyrics(1);
                        continue;
                    }
                    // Back to the song after scrolling away: focusing the
                    // panel keeps it where it was left, so this is the way.
                    KeyEvent { code: KeyCode::Enter, .. } => {
                        ui.lyrics_auto_scroll = true;
                        ui.lyrics_step = 0;
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('d'), .. } => {
                        set_lyrics_offset(ui, playlist, ui.lyrics_offset + 0.5);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('a'), .. } => {
                        set_lyrics_offset(ui, playlist, ui.lyrics_offset - 0.5);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('0'), .. } => {
                        set_lyrics_offset(ui, playlist, 0.0);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Esc, .. } |
                    KeyEvent { code: KeyCode::Char('y'), .. } => {
                        ui.view_mode = ViewMode::Player;
                        continue;
                    }
                    _ => {} // Fall through to global keys
                }
            }

            // `?` opens the key list from any view, and closes it; Esc closes
            // it too. Everything else still works underneath (space pauses).
            if matches!(k, KeyEvent { code: KeyCode::Char('?'), .. }) {
                toggle_help(ui);
                continue;
            }
            if ui.view_mode == ViewMode::Help && matches!(k, KeyEvent { code: KeyCode::Esc, .. }) {
                toggle_help(ui);
                continue;
            }

            // EQ editor keys: arrows select/adjust bands; t / , . / < > edit the
            // selected band's filter type, Q and frequency; brackets cycle presets.
            if ui.view_mode == ViewMode::Eq {
                match k {
                    KeyEvent { code: KeyCode::Left, .. } => {
                        ui.eq_band = ui.eq_band.saturating_sub(1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Right, .. } => {
                        if ui.eq_band + 1 < crate::eq::EQ_BANDS {
                            ui.eq_band += 1;
                        }
                        continue;
                    }
                    KeyEvent { code: KeyCode::Up, modifiers, .. } => {
                        // Plain: 0.5 dB. Shift: 0.1 dB fine step, enough to
                        // dial in AutoEq-style fractional gains exactly.
                        let step = if modifiers.contains(KeyModifiers::SHIFT) { 0.1 } else { 0.5 };
                        state.nudge_eq_gain(ui.eq_band, step);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Down, modifiers, .. } => {
                        let step = if modifiers.contains(KeyModifiers::SHIFT) { 0.1 } else { 0.5 };
                        state.nudge_eq_gain(ui.eq_band, -step);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('['), .. } => {
                        state.step_eq_preset(-1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char(']'), .. } => {
                        state.step_eq_preset(1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('t'), .. } => {
                        state.cycle_eq_type(ui.eq_band, 1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('T'), .. } => {
                        state.cycle_eq_type(ui.eq_band, -1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char(','), .. } => {
                        state.nudge_eq_q(ui.eq_band, -1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('.'), .. } => {
                        state.nudge_eq_q(ui.eq_band, 1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('<'), .. } => {
                        state.nudge_eq_freq(ui.eq_band, -1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('>'), .. } => {
                        state.nudge_eq_freq(ui.eq_band, 1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('a'), .. } => {
                        // Set the preamp the headroom row suggests.
                        let rate = match state.output_rate.load(Ordering::Relaxed) {
                            0 => 48_000.0,
                            r => r as f32,
                        };
                        let h = crate::eq::headroom(&state.eq_bands_array(), state.eq_preamp_db(), rate);
                        if let Some(db) = h.suggested_preamp {
                            state.set_eq_preamp_edited(db);
                        }
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('0'), .. } => {
                        // Restore the selected band's graphic default
                        // (peak, ISO centre, 0 dB, Q 1.41).
                        state.reset_eq_band(ui.eq_band);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Esc, .. } => {
                        ui.view_mode = ui.eq_return;
                        continue;
                    }
                    _ => {} // Fall through to global keys (E/L close, q, space, …)
                }
            }

            // Playlist view keys (when in Normal input mode)
            if ui.view_mode == ViewMode::Playlist {
                // Tab flips the library between the flat list and the artist→album tree.
                if matches!(k, KeyEvent { code: KeyCode::Tab, .. }) {
                    set_pane(ui, !ui.library_tree_mode);
                    if ui.library_tree_mode {
                        ui.tree_dirty = true;
                    }
                    return false;
                }
                if ui.library_tree_mode {
                    match k {
                        KeyEvent { code: KeyCode::Up, .. } => { tree_move(ui, -1); continue; }
                        KeyEvent { code: KeyCode::Down, .. } => { tree_move(ui, 1); continue; }
                        KeyEvent { code: KeyCode::Left, .. } => { tree_collapse_under_cursor(ui); continue; }
                        KeyEvent { code: KeyCode::Right, .. } => { tree_expand_under_cursor(ui); continue; }
                        KeyEvent { code: KeyCode::Enter, .. } => {
                            if let Some(idx) = tree_cursor_play_index(ui) {
                                state.jump_to(idx);
                            }
                            return false;
                        }
                        KeyEvent { code: KeyCode::Char('/'), .. } => {
                            // Seed with the current filter so `/` edits, not resets it.
                            ui.input_mode = InputMode::Search(TextInput::with(&ui.tree_filter));
                            return false;
                        }
                        KeyEvent { code: KeyCode::PageUp, .. } => { tree_page(ui, -1); continue; }
                        KeyEvent { code: KeyCode::PageDown, .. } => { tree_page(ui, 1); continue; }
                        KeyEvent { code: KeyCode::Char('u'), modifiers, .. }
                            if modifiers.contains(KeyModifiers::CONTROL) => { tree_page(ui, -1); continue; }
                        KeyEvent { code: KeyCode::Char('d'), modifiers, .. }
                            if modifiers.contains(KeyModifiers::CONTROL) => { tree_page(ui, 1); continue; }
                        KeyEvent { code: KeyCode::Char('d'), .. }
                        | KeyEvent { code: KeyCode::Delete, .. } => {
                            tree_remove_under_cursor(state, ui, playlist);
                            return false;
                        }
                        KeyEvent { code: KeyCode::Home, .. } => { ui.tree_cursor = 0; continue; }
                        KeyEvent { code: KeyCode::End, .. } => {
                            ui.tree_cursor = tree_visible_len(ui).saturating_sub(1);
                            continue;
                        }
                        KeyEvent { code: KeyCode::Char('g'), .. } => { ui.tree_cursor = 0; continue; }
                        KeyEvent { code: KeyCode::Char('G'), .. } => {
                            ui.tree_cursor = tree_visible_len(ui).saturating_sub(1);
                            continue;
                        }
                        KeyEvent { code: KeyCode::Esc, .. } => {
                            // Esc clears an active filter first, then exits the library.
                            if ui.tree_filter.is_empty() {
                                ui.view_mode = ViewMode::Player;
                            } else {
                                ui.tree_filter.clear();
                                refresh_tree_rows(ui);
                                ui.tree_cursor = 0;
                                ui.tree_scroll = 0;
                            }
                            return false;
                        }
                        _ => {} // fall through to global keys (L, Y, space, q, v, b, …)
                    }
                } else {
                match k {
                    KeyEvent { code: KeyCode::Up, .. } => {
                        playlist_cursor_up(ui);
                        continue; // Drain remaining events for smooth scrolling
                    }
                    KeyEvent { code: KeyCode::Down, .. } => {
                        playlist_cursor_down(ui, playlist);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Home, .. } => {
                        playlist_cursor_home(ui);
                        continue;
                    }
                    KeyEvent { code: KeyCode::End, .. } => {
                        playlist_cursor_end(ui, playlist);
                        continue;
                    }
                    KeyEvent { code: KeyCode::PageUp, .. } => {
                        playlist_cursor_page_up(ui);
                        continue;
                    }
                    KeyEvent { code: KeyCode::PageDown, .. } => {
                        playlist_cursor_page_down(ui, playlist);
                        continue;
                    }
                    // Vim-style fallbacks for Mac keyboards that lack Home/End/PgUp/PgDn.
                    KeyEvent { code: KeyCode::Char('g'), .. } => {
                        playlist_cursor_home(ui);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('G'), .. } => {
                        playlist_cursor_end(ui, playlist);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('u'), modifiers, .. } if modifiers.contains(KeyModifiers::CONTROL) => {
                        playlist_cursor_page_up(ui);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('d'), modifiers, .. } if modifiers.contains(KeyModifiers::CONTROL) => {
                        playlist_cursor_page_down(ui, playlist);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('S'), .. } => {
                        sort_playlist_by_tags(state, ui, playlist);
                        return false;
                    }
                    KeyEvent { code: KeyCode::Enter, .. } => {
                        let Some(target) = shown_track(ui, ui.cursor) else { return false };
                        // Never a jump past the end: the playlist loop would
                        // see current >= len, and with repeat off Keet exits.
                        if target < playlist.len() {
                            state.jump_to(target);
                        }
                        return false;
                    }
                    KeyEvent { code: KeyCode::Char('/'), .. } => {
                        ui.input_mode = InputMode::Search(TextInput::new());
                        return false;
                    }
                    KeyEvent { code: KeyCode::Char('a'), .. } => {
                        enqueue_track(state, ui, playlist);
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
            }

            // Global keys (work in all view modes)
            match k {
                KeyEvent { code: KeyCode::Char(' '), .. } => state.toggle_pause(),
                KeyEvent { code: KeyCode::Up, .. } => state.next(),
                KeyEvent { code: KeyCode::Down, .. } => state.prev(),
                KeyEvent { code: KeyCode::Right, .. } => state.seek(10),
                KeyEvent { code: KeyCode::Left, .. } => state.seek(-10),
                KeyEvent { code: KeyCode::Char('v'), .. } => {
                    // Hi-Fi renders a fixed VU panel and no switchable viz, so
                    // cycling here changed nothing on screen, only the hidden
                    // setting of the other themes. (The panel is fed whatever
                    // the setting: `state.shown_viz`.)
                    if state.theme_kind() == crate::theme::ThemeKind::HiFi {
                        ui.set_status("Hi-Fi uses its VU panel — press T for another theme".to_string());
                        continue;
                    }
                    state.cycle_viz_mode();
                    // Re-anchor the UI at the top of the screen (resize-repaint
                    // path): a taller viz then grows into the reclaimed rows
                    // instead of scrolling the whole screen up.
                    ui.terminal_resized = true;
                }
                KeyEvent { code: KeyCode::Char('+'), .. } |
                KeyEvent { code: KeyCode::Char('='), .. } => state.volume_up(),
                KeyEvent { code: KeyCode::Char('-'), .. } => state.volume_down(),
                KeyEvent { code: KeyCode::Char('e'), .. } => {
                    // E opens (and closes) the EQ+FX editor, over the view
                    // it was opened from. From the key list it acts on the
                    // view under the list, and the list closes: recording
                    // Help as the EQ's way back (and the EQ as the list's)
                    // made Esc bounce between the two forever.
                    let under = under_help(ui);
                    ui.view_mode = match under {
                        ViewMode::Eq => ui.eq_return,
                        view => {
                            ui.eq_return = view;
                            ViewMode::Eq
                        }
                    };
                }
                KeyEvent { code: KeyCode::Char('x'), .. } => state.cycle_effects(),
                KeyEvent { code: KeyCode::Char('f'), .. } => state.toggle_pre_fader(),
                KeyEvent { code: KeyCode::Char('b'), .. } => {
                    state.toggle_viz_style();
                    // Style can change the viz height (VU: 4 vs 3 lines) — same
                    // re-anchor as 'v' so growth never scrolls the screen.
                    ui.terminal_resized = true;
                }
                KeyEvent { code: KeyCode::Char('l'), .. } => match ui.view_mode {
                    ViewMode::Playlist => ui.view_mode = ViewMode::Player,
                    _ => open_view(ui, ViewMode::Playlist),
                },
                KeyEvent { code: KeyCode::Char('y'), .. } => match ui.view_mode {
                    ViewMode::Lyrics => ui.view_mode = ViewMode::Player,
                    _ => open_view(ui, ViewMode::Lyrics),
                },
                KeyEvent { code: KeyCode::Char('s'), .. } => {
                    // The field is the playlist's: it opens there, where it
                    // shows and has the keys (from the player it was opened
                    // out of sight, and the name's letters ran player keys).
                    // A search left open there closes properly first.
                    cancel_field(ui);
                    open_view(ui, ViewMode::Playlist);
                    ui.input_mode = InputMode::SavePlaylist(TextInput::new());
                }
                KeyEvent { code: KeyCode::Char('R'), .. } => {
                    toggle_repeat(ui, state);
                }
                KeyEvent { code: KeyCode::Char('r'), .. } => {
                    rescan(ui, playlist);
                }
                KeyEvent { code: KeyCode::Char('z'), .. } => {
                    toggle_shuffle(state, ui, playlist);
                }
                KeyEvent { code: KeyCode::Char('o'), .. } => {
                    // A search left open in the playlist closes properly (its
                    // filter with it): replaced as is, its matches stayed
                    // the list with no field to show it.
                    cancel_field(ui);
                    set_input_mode(ui, InputMode::OpenPath(TextInput::new()));
                }
                KeyEvent { code: KeyCode::Char('p'), .. } => {
                    if has_native_picker() {
                        match pick_folder_native() {
                            Some(p) => switch_source_paths(state, ui, playlist, p),
                            None => ui.set_status("Cancelled".to_string()),
                        }
                    } else {
                        ui.set_status("Native picker unavailable; press o to type a path".to_string());
                    }
                }
                KeyEvent { code: KeyCode::Char('q'), .. } => { state.quit(); return true; }
                // Shift+K / Shift+W: capitals however reported (`shift_to_capital`).
                KeyEvent { code: KeyCode::Char('k'), .. } => toggle_dock(state, ui),
                KeyEvent { code: KeyCode::Char('K'), .. } => close_docked(ui),
                KeyEvent { code: KeyCode::Char('W'), .. } => ui.save_defaults = true,
                KeyEvent { code: KeyCode::Char('m'), .. } => {
                    let on = !state.mouse.load(Ordering::Relaxed);
                    state.mouse.store(on, Ordering::Relaxed);
                    tui::set_mouse(on);
                    ui.set_status(if on { "mouse on (Shift+drag selects text)" } else { "mouse off" }.to_string());
                }
                // Esc closes every other view, so in the player a stray one
                // (meant for a view already closed) quit without warning: it
                // now asks once, and a second Esc within ESC_QUIT_WINDOW quits.
                KeyEvent { code: KeyCode::Esc, .. } => {
                    let now = std::time::Instant::now();
                    if esc_confirms_quit(ui.esc_quit_until, now) {
                        state.quit();
                        return true;
                    }
                    // The "Quit?" dialog shows while this is armed.
                    ui.esc_quit_until = Some(now + ESC_QUIT_WINDOW);
                }
                KeyEvent { code: KeyCode::Char('c'), modifiers: KeyModifiers::CONTROL, .. } => {
                    state.quit(); return true;
                }
                KeyEvent { code: KeyCode::Char('c'), .. } => state.cycle_crossfeed(),
                KeyEvent { code: KeyCode::Char('i'), .. } => {
                    // Minimal shows cpu/mem permanently in SIGNAL, so there is
                    // nothing to toggle — say so instead of silently flipping a
                    // flag that changes nothing on screen.
                    if state.theme_kind() == crate::theme::ThemeKind::Minimal {
                        ui.set_status("cpu/mem are always shown in this theme".to_string());
                    } else {
                        state.toggle_stats();
                    }
                }
                KeyEvent { code: KeyCode::Char('['), .. } => state.balance_left(),
                KeyEvent { code: KeyCode::Char(']'), .. } => state.balance_right(),
                KeyEvent { code: KeyCode::Char('L'), .. } => {
                    // HiFi's VU panel has no extra detail: toggling flipped
                    // the hidden setting of the other themes' visualization.
                    if state.theme_kind() == crate::theme::ThemeKind::HiFi {
                        ui.set_status("Hi-Fi's VU panel has no extra detail — press T for another theme".to_string());
                        continue;
                    }
                    let on = state.toggle_viz_extras();
                    let what = match state.shown_viz() {
                        VizMode::VuMeter => "level history",
                        VizMode::SpectrumHorizontal | VizMode::SpectrumVertical | VizMode::Polar => "band legend",
                        VizMode::Cymatics => "frequency readout",
                        _ => "detail (not used by this viz)",
                    };
                    ui.set_status(format!("{what}: {}", if on { "on" } else { "off" }));
                    // Costs or frees a row, so the frame changes height.
                    ui.terminal_resized = true;
                    continue;
                }
                KeyEvent { code: KeyCode::Char('F'), .. } => {
                    let on = state.toggle_viz_fullscreen();
                    // Full window is the visualization alone: docked panels
                    // are not drawn in it, and the one with the keys floated
                    // over it. The keys go back to the player; the dock
                    // stays for the way out.
                    if on && matches!(ui.view_mode, ViewMode::Playlist | ViewMode::Lyrics) {
                        ui.view_mode = ViewMode::Player;
                    }
                    ui.set_status(if on { "full window: on".into() } else { String::from("full window: off") });
                    // The frame changes height by many rows; re-anchor at row 1
                    // instead of letting it grow downward and scroll the screen.
                    ui.terminal_resized = true;
                    continue;
                }
                KeyEvent { code: KeyCode::Char('t'), .. } => {
                    let kind = state.cycle_theme();
                    ui.set_status(format!("theme: {}", kind.name()));
                    // Theme switch changes paint top-to-bottom; force a full redraw so
                    // residual lines from the previous theme don't bleed through.
                    ui.terminal_resized = true;
                    // Themes reserve different cover slots (Minimal's is
                    // smaller), and half-block/Sixel bake the size in at decode
                    // time — so the image has to be re-decoded, not just
                    // re-placed. Flagged here; main respawns the worker.
                    ui.cover_resize_pending = true;
                    ui.cover_dirty_frame = true;
                }
                _ => {}
            }
    }
    false
}

/// Change the playing track's lyrics offset and remember it for that track.
/// Only for synced lyrics (the footer offers the keys only then): an offset
/// set on plain lyrics was saved, and shifted synced lyrics found later.
fn set_lyrics_offset(ui: &mut UiState, playlist: &[PathBuf], secs: f64) {
    if !ui.lyrics().is_some_and(|l| l.is_synced()) {
        return;
    }
    ui.lyrics_offset = secs;
    if let Some(path) = playlist.get(ui.current) {
        ui.lyrics_offsets.set(path, secs);
    }
}

/// `/` search while the tree view is showing: keystrokes drive `ui.tree_filter`
/// live. Enter keeps the filter and returns to navigating the results; Esc
/// clears it. Up/Down/PageUp/PageDown move the cursor through the filtered rows.
fn tree_search_input(ui: &mut UiState, key: KeyEvent) -> bool {
    let InputMode::Search(field) = &mut ui.input_mode else { return false };
    match field.handle(key) {
        Edit::Cancel => cancel_field(ui),
        // Keep the filter, navigate the results.
        Edit::Submit => ui.input_mode = InputMode::Normal,
        Edit::Changed => rebuild_tree_filter(ui),
        Edit::Moved => {}
        Edit::Ignored => match key.code {
            KeyCode::Up => tree_move(ui, -1),
            KeyCode::Down => tree_move(ui, 1),
            KeyCode::PageUp => tree_page(ui, -1),
            KeyCode::PageDown => tree_page(ui, 1),
            _ => {}
        },
    }
    false
}

fn handle_text_input(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>, key: KeyEvent) -> bool {
    // Ctrl+C quits from any text prompt, same as everywhere else; the field
    // never takes it as typing.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.quit();
        return true;
    }
    // The tree view filters itself; its `/` search has its own handling.
    if ui.library_tree_mode && matches!(ui.input_mode, InputMode::Search(_)) {
        return tree_search_input(ui, key);
    }
    let edit = match &mut ui.input_mode {
        InputMode::Search(t) | InputMode::SavePlaylist(t) | InputMode::OpenPath(t) => t.handle(key),
        InputMode::Normal => return false,
    };
    let text = ui.input_mode.field().map(|t| t.text().to_string()).unwrap_or_default();
    match (&ui.input_mode, edit) {
        (InputMode::Search(_), Edit::Cancel) => cancel_field(ui),
        (InputMode::Search(_), Edit::Submit) => {
            // A non-empty query with zero hits leaves filtered_indices
            // empty — falling through to ui.cursor here would jump to
            // an unrelated track. Just close the search instead.
            let no_matches = !text.is_empty() && ui.filtered_indices.is_empty();
            if !no_matches {
                let target = if ui.filtered_indices.is_empty() {
                    ui.cursor
                } else {
                    ui.filtered_indices.get(ui.cursor).copied().unwrap_or(0)
                };
                state.jump_to(target);
            }
            ui.input_mode = InputMode::Normal;
            ui.filtered_indices.clear();
            ui.cursor = 0;
            ui.scroll_offset = 0;
        }
        (InputMode::Search(_), Edit::Changed) => rebuild_filter(ui, playlist),
        (InputMode::Search(_), Edit::Ignored) => match key.code {
            KeyCode::Up => playlist_cursor_up(ui),
            KeyCode::Down => playlist_cursor_down(ui, playlist),
            KeyCode::PageUp => playlist_cursor_page_up(ui),
            KeyCode::PageDown => playlist_cursor_page_down(ui, playlist),
            _ => {}
        },
        (InputMode::SavePlaylist(_), Edit::Cancel) => cancel_field(ui),
        (InputMode::SavePlaylist(_), Edit::Submit) => {
            ui.input_mode = InputMode::Normal;
            if !text.is_empty() {
                match crate::playlist::save_m3u(playlist, &text) {
                    Ok(path) => {
                        let fname = path.file_name().unwrap_or_default().to_string_lossy();
                        ui.set_status(format!("Saved {} tracks to {}", playlist.len(), fname));
                    }
                    Err(e) => ui.set_status(format!("Save failed: {}", e)),
                }
            }
        }
        (InputMode::OpenPath(_), Edit::Cancel) => set_input_mode(ui, InputMode::Normal),
        (InputMode::OpenPath(_), Edit::Submit) => match crate::welcome::parse_typed_path(&text) {
            Some(p) if p.exists() => {
                set_input_mode(ui, InputMode::Normal);
                switch_source_paths(state, ui, playlist, p);
            }
            // Stay open on a typo: the path is there to fix.
            Some(p) => ui.open_error = Some(format!("Not found: {}", p.display())),
            None => set_input_mode(ui, InputMode::Normal),
        },
        (InputMode::OpenPath(_), Edit::Changed) => ui.open_error = None,
        _ => {}
    }
    false
}

/// Esc in the playlist panel's field: a search drops its filter (the list's
/// or the tree's), "save as" just closes.
///
/// The cursor stays on what it was on: a key hint clicked with a search open
/// closes the search first, then acts — `d remove` on a docked list's match
/// removed the playlist's first track (the cursor reset to 0), and in the
/// tree staged its first artist. The list's cursor goes to its track's place
/// in the whole list; the tree's row is unfolded to and kept.
fn cancel_field(ui: &mut UiState) {
    if matches!(ui.input_mode, InputMode::Search(_)) {
        if ui.library_tree_mode {
            let row = tree_row_at_cursor(ui);
            ui.tree_filter.clear();
            if let Some(row) = row {
                use crate::library::VisibleRow;
                // Unfold down to it: with the filter gone the fold decides.
                match row {
                    VisibleRow::Track { artist, album, .. } => {
                        crate::library::expand(&ui.library_tree, &mut ui.tree_fold, VisibleRow::Artist { artist });
                        crate::library::expand(&ui.library_tree, &mut ui.tree_fold, VisibleRow::Album { artist, album });
                    }
                    VisibleRow::Album { artist, .. } => {
                        crate::library::expand(&ui.library_tree, &mut ui.tree_fold, VisibleRow::Artist { artist });
                    }
                    VisibleRow::Artist { .. } => {}
                }
            }
            refresh_tree_rows(ui);
            ui.tree_cursor = row.and_then(|r| ui.tree_rows.iter().position(|&x| x == r)).unwrap_or(0);
        } else {
            if list_filtered(ui) {
                ui.cursor = ui.filtered_indices.get(ui.cursor).copied().unwrap_or(0);
            }
            ui.filtered_indices.clear();
        }
    }
    if matches!(ui.input_mode, InputMode::Search(_) | InputMode::SavePlaylist(_)) {
        ui.input_mode = InputMode::Normal;
    }
}

/// Give the keys to the list (`tree` false) or the tree pane. A search open
/// in the other pane closes first, its filter with it: it is that pane's,
/// and handed over, the query filtered the new pane while the old one stayed
/// filtered with no field to show it (Enter and Delete acted on hidden rows).
fn set_pane(ui: &mut UiState, tree: bool) {
    if ui.library_tree_mode != tree {
        cancel_field(ui);
        ui.library_tree_mode = tree;
    }
}

/// The letters Keet binds as capitals (Shift+K, W, L, F, R, S, G, T).
const SHIFT_BOUND: &[char] = &['k', 'w', 'l', 'f', 'r', 's', 'g', 't'];

/// A command letter with Shift as its capital: terminals report Shift+L as
/// `L` or as `l` with Shift (matched form by form, Shift+L, Shift+F and the
/// EQ's Shift+T were missed). Only for the letters bound as capitals
/// (`SHIFT_BOUND`): another Shift+letter is the lowercase key it always
/// was (Shift+E opens the EQ — most Unix terminals send it as `E` with
/// Shift, which matched nothing). Only with Shift held: with Caps Lock a
/// capital comes without it and is left alone. And never what is typed into
/// a field: with Caps Lock on, Shift+a arrives as `a` with Shift and means `a`.
fn shift_to_capital(mut k: KeyEvent) -> KeyEvent {
    if let KeyCode::Char(c) = k.code {
        if k.modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_alphabetic() {
            let lower = c.to_ascii_lowercase();
            k.code = KeyCode::Char(if SHIFT_BOUND.contains(&lower) { lower.to_ascii_uppercase() } else { lower });
        }
    }
    k
}

/// Whether the open field has the keys: the `o` dialog always, the
/// playlist's search and save-as while the playlist has them. Left open when
/// the keys move (a click on the lyrics or the player), one keeps its query
/// and place for the way back (closing it then lost the search). Typing,
/// a clicked hint and a paste all go by this.
fn field_has_keys(ui: &UiState) -> bool {
    match ui.input_mode {
        InputMode::OpenPath(_) => true,
        InputMode::Search(_) | InputMode::SavePlaylist(_) => ui.view_mode == ViewMode::Playlist,
        InputMode::Normal => false,
    }
}

/// Change the input mode; opening or closing the `o` dialog forgets why its
/// last path failed.
fn set_input_mode(ui: &mut UiState, mode: InputMode) {
    if mode.is_overlay() != ui.input_mode.is_overlay() {
        ui.open_error = None;
    }
    ui.input_mode = mode;
}

/// How soon a second click on the same row makes a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Rows the wheel moves a list or the lyrics per notch.
const WHEEL_ROWS: usize = 3;

/// A mouse event: a click or drag on a progress bar seeks; a click on a
/// list row moves the cursor there and a double click plays it (in the tree,
/// a double click on an album or artist folds or unfolds it); the wheel
/// scrolls the list or the lyrics; with a dialog open, a click outside it
/// closes it as Esc would.
fn mouse(state: &PlayerState, ui: &mut UiState, playlist: &[PathBuf], m: event::MouseEvent) {
    use event::{MouseButton, MouseEventKind as K};
    // The too-small screen shows nothing to click (it keeps the last real
    // frame's layout for `open_view`, and a click read that as "in the
    // player, outside the docked panel" and took its keys).
    if ui.too_small {
        return;
    }
    let at = |r: &Rect| m.column >= r.x && m.column < r.right() && m.row >= r.y && m.row < r.bottom();
    // With a dialog open only the TOP one reacts (the last drawn, its hits
    // registered after it): a click outside it closes it, and nothing under
    // it reacts — not a key hint of the view (a visible `q` would quit while
    // "Quit?" asks), not a row of the playlist the EQ editor floats over.
    let first = if overlay_open(ui) {
        match ui.hits.iter().rposition(|(_, h)| *h == Hit::Dialog) {
            Some(top) if at(&ui.hits[top].0) => top,
            _ => {
                if matches!(m.kind, K::Down(MouseButton::Left)) {
                    close_dialog(ui);
                }
                return;
            }
        }
    } else {
        0
    };
    let hit = ui.hits[first..].iter().rev().find(|(r, _)| at(r)).copied();
    // The docked panel under the pointer, if any: only from the top dialog
    // on (a click in a floating panel over a docked one is the floating
    // one's). Looked up once, for the key hints, the click and the wheel.
    let panel_under = ui.hits[first..].iter().rev().find_map(|(r, h)| match h {
        Hit::Panel(v) if at(r) => Some(*v),
        _ => None,
    });
    // A click in a docked panel gives it the keys; one elsewhere in the
    // player takes them back. Only what is on top counts: a click in a
    // floating panel over a docked one is the floating one's. Key hints
    // too: one in a docked panel's footer acts on that panel (its "close"
    // would otherwise close nothing, its "scroll" move the player), and one
    // of the player's own key bar on the player (`↑↓ track` moved the
    // docked list's cursor).
    if matches!(m.kind, K::Down(MouseButton::Left)) {
        match panel_under {
            // Docked: it takes the keys, as it was left.
            Some(v) => open_view(ui, v),
            // (A floating panel is a dialog: handled above.)
            // In the player itself (no dialog over it: a click inside one
            // is the dialog's, and took the keys from the panel).
            None if !overlay_open(ui) && ui.docked_shown.contains(&ui.view_mode) => ui.view_mode = ViewMode::Player,
            None => {}
        }
    }
    if let (K::Down(MouseButton::Left), Some((_, Hit::Key(code, mods)))) = (m.kind, hit) {
        ui.pressed.push_back(KeyEvent::new(code, mods));
        return;
    }
    match m.kind {
        K::Down(MouseButton::Left) | K::Drag(MouseButton::Left) => {
            // The same row of the same target: with both panes showing, a
            // list row and a tree row share a screen row.
            let target = hit.map(|(r, _)| r);
            let spot = match hit {
                Some((r, Hit::List { first })) => crate::state::ClickSpot::ListItem(first + (m.row - r.y) as usize),
                Some((r, Hit::Tree { first })) => crate::state::ClickSpot::TreeItem(first + (m.row - r.y) as usize),
                _ => crate::state::ClickSpot::Row(m.row),
            };
            let double = matches!(m.kind, K::Down(_))
                && ui.last_click.is_some_and(|(t, on, was)| was == spot && on == target && t.elapsed() < DOUBLE_CLICK);
            if matches!(m.kind, K::Down(_)) {
                // A double click is used up: a third starts again.
                ui.last_click = if double { None } else { Some((std::time::Instant::now(), target, spot)) };
            }
            match hit {
                Some((r, Hit::Seek)) => {
                    let total = state.total_secs();
                    if total > 0.0 {
                        let frac = (m.column - r.x) as f64 / r.width.saturating_sub(1).max(1) as f64;
                        state.seek_to_offset(frac.clamp(0.0, 1.0) * total - state.time_secs());
                    }
                }
                Some((_, Hit::Tab(view))) if matches!(m.kind, K::Down(_)) => open_view(ui, view),
                Some((r, Hit::List { first })) if matches!(m.kind, K::Down(_)) => {
                    // A click in a pane gives it the keys.
                    set_pane(ui, false);
                    let pos = first + (m.row - r.y) as usize;
                    let len = shown_len(ui, playlist);
                    if pos < len {
                        ui.cursor = pos;
                        ui.list_clicked = Some(pos);
                        if double {
                            let target = shown_track(ui, pos).unwrap_or(pos);
                            state.jump_to(target);
                        }
                    }
                }
                Some((r, Hit::Tree { first })) if matches!(m.kind, K::Down(_)) => {
                    set_pane(ui, true);
                    let pos = first + (m.row - r.y) as usize;
                    if pos < tree_visible_len(ui) {
                        ui.tree_cursor = pos;
                        ui.tree_clicked = Some(pos);
                        if double {
                            match tree_row_at_cursor(ui) {
                                Some(crate::library::VisibleRow::Track { .. }) => {
                                    if let Some(idx) = tree_cursor_play_index(ui) {
                                        state.jump_to(idx);
                                    }
                                }
                                Some(_) => {
                                    // Unfold; already unfolded, fold.
                                    let before = ui.tree_rows.len();
                                    tree_expand_under_cursor(ui);
                                    if ui.tree_rows.len() == before {
                                        tree_collapse_under_cursor(ui);
                                    }
                                }
                                None => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        K::ScrollUp | K::ScrollDown => {
            let down = matches!(m.kind, K::ScrollDown);
            // What is under the pointer scrolls, not what has the keys: a
            // pane's rows, else the panel (docked, or the floating one).
            #[derive(PartialEq)]
            enum Wheel { List, Tree, Lyrics }
            let target = match hit {
                Some((_, Hit::List { .. })) => Some(Wheel::List),
                Some((_, Hit::Tree { .. })) => Some(Wheel::Tree),
                _ => {
                    let docked = panel_under;
                    let floating = matches!(ui.view_mode, ViewMode::Playlist | ViewMode::Lyrics)
                        && ui.hits[first..].iter().any(|(r, h)| *h == Hit::Dialog && at(r));
                    match docked.or(floating.then_some(ui.view_mode)) {
                        Some(ViewMode::Lyrics) => Some(Wheel::Lyrics),
                        Some(ViewMode::Playlist) if ui.library_tree_mode => Some(Wheel::Tree),
                        Some(ViewMode::Playlist) => Some(Wheel::List),
                        _ => None,
                    }
                }
            };
            match target {
                Some(Wheel::Tree) => {
                    tree_move(ui, if down { WHEEL_ROWS as isize } else { -(WHEEL_ROWS as isize) })
                }
                Some(Wheel::List) => {
                    for _ in 0..WHEEL_ROWS {
                        if down { playlist_cursor_down(ui, playlist) } else { playlist_cursor_up(ui) }
                    }
                }
                Some(Wheel::Lyrics) => ui.scroll_lyrics(if down { WHEEL_ROWS as isize } else { -(WHEEL_ROWS as isize) }),
                _ => {}
            }
        }
        _ => {}
    }
}

/// `k`: dock the panel that has the keys into the player's layout, or
/// float it again. HiFi docks below its key bar only with the rows for it.
fn toggle_dock(state: &PlayerState, ui: &mut UiState) {
    let v = ui.view_mode;
    if !matches!(v, ViewMode::Playlist | ViewMode::Lyrics) {
        ui.set_status("k docks the playlist (l) or the lyrics (y) while it is open".to_string());
        return;
    }
    let name = if v == ViewMode::Playlist { "playlist" } else { "lyrics" };
    if let Some(i) = ui.docked.iter().position(|&d| d == v) {
        ui.docked.remove(i);
        ui.set_status(format!("{name} floating"));
    } else {
        let (w, h) = tui::window();
        if !dock_room(state, v, Rect::new(0, 0, w, h)) {
            ui.set_status("no room to dock here: make the window bigger".to_string());
            return;
        }
        ui.docked.push(v);
        ui.set_status(format!("{name} docked (k floats it again)"));
    }
    // The layout changes: images move.
    ui.terminal_resized = true;
}

/// `Shift+K`: close the docked panel that has the keys — out of the dock and
/// off the screen in one press (its key only hands the keys back, so a double
/// tap on `l` or `y` never closes it). Its key opens it floating again.
fn close_docked(ui: &mut UiState) {
    let v = ui.view_mode;
    let Some(i) = ui.docked.iter().position(|&d| d == v) else {
        ui.set_status("Shift+K closes a docked playlist or lyrics while it has the keys".to_string());
        return;
    };
    ui.docked.remove(i);
    ui.view_mode = ViewMode::Player;
    let name = if v == ViewMode::Playlist { "playlist" } else { "lyrics" };
    ui.set_status(format!("{name} closed"));
    // The layout changes: images move.
    ui.terminal_resized = true;
}

/// The first keys of a panel's footer, each with what a click on it does
/// when that is not its key. Docked without the keys: its own key focuses
/// it — a click on that hint is the panel itself (`Hit::Panel`), since
/// pressing the key after the click focused it would hand them straight
/// back; with them, Esc hands them back; either way `k` floats it and
/// `Shift+K` closes it. Floating: `k` docks it, its own key closes it.
pub(crate) fn panel_footer_keys(ui: &UiState, view: ViewMode, upper: bool) -> Vec<(&'static str, String, Option<Hit>)> {
    let label = |s: &str| if upper { s.to_uppercase() } else { s.to_string() };
    let own = panel_key(view);
    if ui.docked_shown.contains(&view) {
        let mut keys = Vec::new();
        if ui.focus != view {
            keys.push((own, label("focus"), Some(Hit::Panel(view))));
        }
        keys.push(("k", label("float"), None));
        keys.push(("⇧k", label("close"), None));
        if ui.focus == view {
            // What Esc does: the tree's first clears its filter.
            let tree_filter = view == ViewMode::Playlist && ui.library_tree_mode && !ui.tree_filter.is_empty();
            keys.push(("Esc", label(if tree_filter { "clear filter" } else { "player" }), None));
        }
        keys
    } else {
        // Floating. Docked all the same (no room for it this frame): `k`
        // takes it out of the dock, and must not promise to dock it.
        let k = if ui.docked.contains(&view) { "undock" } else { "dock" };
        vec![("k", label(k), None), (own, label("close"), None)]
    }
}

/// The key that opens (and focuses) a panel.
fn panel_key(view: ViewMode) -> &'static str {
    if view == ViewMode::Playlist { "l" } else { "y" }
}

/// Give `view` the keys — by its key, a tab, a click in a docked panel. A
/// panel that was not on screen OPENS set up: the list with the cursor on
/// the playing track, the lyrics following the song. One that was — docked,
/// or floating in the last frame (`k` then `y` before a frame: docked since,
/// but it was up) — only takes the keys and stays exactly as it was left (a
/// click to scroll it lost the verse the user had wheeled to, and moved the
/// list under the pointer). A docked panel hidden for want of room was not
/// on screen: it opens floating, set up. And the last frame counts only for
/// a panel still up — docked, or the view under any dialog: one closed since
/// (a double tap, `Shift+K`, keys on the too-small screen, which keeps the
/// last real frame's record) opens set up. A docked panel is never closed by
/// its key (it hands the keys back), so `y y` on one before a frame is
/// focus, unfocus, focus: it keeps its state, as `k y y`. One floating for
/// want of room is off the screen once its keys go back: if a frame shows
/// that, the second `y` sets it up like any panel that was not on screen.
fn open_view(ui: &mut UiState, view: ViewMode) {
    let up = ui.docked.contains(&view) || base_view(ui) == view;
    let shown = up && (ui.docked_shown.contains(&view) || ui.floating_shown == Some(view));
    let opening = ui.view_mode != view && !shown;
    match view {
        ViewMode::Playlist if opening => {
            // On the playing track. With a search's filter showing, the
            // cursor is a position among the matches: the track's there, or
            // the first (its playlist index pointed at another match, or past
            // them, and Enter played that).
            ui.cursor = if list_filtered(ui) {
                ui.filtered_indices.iter().position(|&i| i == ui.current).unwrap_or(0)
            } else {
                ui.current
            };
        }
        ViewMode::Lyrics if opening => ui.reset_lyrics_view(),
        _ => {}
    }
    ui.view_mode = view;
}

/// The view the key list floats over (the EQ editor, perhaps).
fn under_help(ui: &UiState) -> ViewMode {
    if ui.view_mode == ViewMode::Help { ui.help_return } else { ui.view_mode }
}

/// The view the key list and the EQ editor float over (the key list can be
/// over the EQ editor).
fn base_view(ui: &UiState) -> ViewMode {
    let v = under_help(ui);
    if v == ViewMode::Eq { ui.eq_return } else { v }
}

/// Close whatever dialog is open, as Esc would.
fn close_dialog(ui: &mut UiState) {
    if ui.input_mode.is_overlay() {
        set_input_mode(ui, InputMode::Normal);
    } else if ui.view_mode == ViewMode::Help {
        toggle_help(ui);
    } else if ui.view_mode == ViewMode::Eq {
        ui.view_mode = ui.eq_return;
    } else if let Some((label, _)) = ui.tree_pending_remove.take() {
        ui.set_status(format!("cancelled removing {label}"));
    } else if quit_armed(ui) {
        ui.esc_quit_until = None;
    } else if matches!(ui.view_mode, ViewMode::Playlist | ViewMode::Lyrics) {
        // Its field goes with it: left open, every key went on typing into
        // a search nobody could see.
        cancel_field(ui);
        ui.view_mode = ViewMode::Player;
    } else {
        ui.esc_quit_until = None;
    }
}

/// Pasted text (or a folder dropped on the terminal, which most terminals
/// paste): into the open field whole, or, with none open, into a new
/// "open" dialog — dropping a folder on Keet is a way to open it.
fn paste(ui: &mut UiState, playlist: &[PathBuf], text: &str) {
    // Into the field with the keys; with none (a search left open while
    // another panel has them), the `o` dialog, as with no field at all.
    if !field_has_keys(ui) {
        cancel_field(ui);
    }
    match &mut ui.input_mode {
        // A search or a name: a paste's trailing space means nothing (a
        // double-click copy's: "beatles " matched nothing), nor a leading
        // one into an empty field; one continuing the text is meant ("mix"
        // + " 2"). A path keeps them all: a paste can continue one.
        InputMode::Search(t) => {
            t.insert(&field_paste(t, text));
            if ui.library_tree_mode {
                rebuild_tree_filter(ui);
            } else {
                rebuild_filter(ui, playlist);
            }
        }
        InputMode::SavePlaylist(t) => t.insert(&field_paste(t, text)),
        InputMode::OpenPath(t) => {
            t.insert(text);
            // A new path: the old "Not found" goes, as when typing.
            ui.open_error = None;
        }
        // A fresh path: the spaces round it mean nothing either.
        InputMode::Normal => set_input_mode(ui, InputMode::OpenPath(TextInput::with(crate::text_input::clean(text).trim()))),
    }
}

/// A paste into a search or a name: cleaned, its trailing spaces dropped,
/// and its leading ones too when the field is empty (see `paste`).
fn field_paste(field: &TextInput, text: &str) -> String {
    let c = crate::text_input::clean(text);
    let c = c.trim_end();
    if field.is_empty() { c.trim_start().to_string() } else { c.to_string() }
}

/// The flat list's cursor track, by path, in `list` (the playlist the
/// search's hits index): noted before an edit, for `refilter_to` after it.
pub(crate) fn cursor_track(ui: &UiState, list: &[PathBuf]) -> Option<PathBuf> {
    shown_track(ui, ui.cursor).and_then(|i| list.get(i)).cloned()
}

/// After a playlist edit: a live search re-runs its query (its hits were
/// indices into the old list) and its cursor goes back to `keep`, its track
/// before the edit (re-run alone, it went to the first hit, and Enter
/// played that). Without a search, nothing.
/// With nothing typed the list is the whole playlist, so the cursor is
/// found there (looked for among the hits, of which there are none, it
/// stayed on row 0, and Enter played track 1). The scroll is kept: the draw
/// brings the cursor into view (`rebuild_filter` zeroes it for a new query).
pub(crate) fn refilter_to(ui: &mut UiState, playlist: &[PathBuf], keep: Option<PathBuf>) {
    if !list_search_open(ui) {
        return;
    }
    let (scroll, was) = (ui.scroll_offset, ui.cursor);
    rebuild_filter(ui, playlist);
    ui.scroll_offset = scroll;
    let pos = keep.and_then(|k| {
        if list_filtered(ui) {
            ui.filtered_indices.iter().position(|&i| playlist[i] == k)
        } else {
            playlist.iter().position(|p| *p == k)
        }
    });
    // Its track gone (a rescan found the file deleted): the row that took
    // its place, not the top of the list (`rebuild_filter` put it there).
    ui.cursor = pos.unwrap_or_else(|| was.min(shown_len(ui, playlist).saturating_sub(1)));
}

/// Whether the flat list's search is open (the tree's `/` filters the tree,
/// not the list: an edit under it treats the list as unfiltered).
fn list_search_open(ui: &UiState) -> bool {
    matches!(ui.input_mode, InputMode::Search(_)) && !ui.library_tree_mode
}

fn rebuild_filter(ui: &mut UiState, playlist: &[PathBuf]) {
    let query = match &ui.input_mode {
        InputMode::Search(q) => q.text().to_lowercase(),
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

/// Whether the flat list shows a filter: a search with a query typed (its
/// matches, NONE when nothing matches — an empty filter is not "no filter"
/// there: the wheel walked a hidden cursor through the whole playlist behind
/// "(no matches)", and Enter played wherever it got), or one left after
/// Enter.
pub(crate) fn list_filtered(ui: &UiState) -> bool {
    (ui.input_mode.searching() && !ui.library_tree_mode) || !ui.filtered_indices.is_empty()
}

/// Rows in the flat list as shown.
pub(crate) fn shown_len(ui: &UiState, playlist: &[PathBuf]) -> usize {
    if list_filtered(ui) { ui.filtered_indices.len() } else { playlist.len() }
}

/// The playlist index of the flat list's row `pos`, as shown.
pub(crate) fn shown_track(ui: &UiState, pos: usize) -> Option<usize> {
    if list_filtered(ui) { ui.filtered_indices.get(pos).copied() } else { Some(pos) }
}

fn playlist_cursor_down(ui: &mut UiState, playlist: &[PathBuf]) {
    let max = shown_len(ui, playlist).saturating_sub(1);
    if ui.cursor < max {
        ui.cursor += 1;
    }
}

fn playlist_cursor_home(ui: &mut UiState) {
    ui.cursor = 0;
    ui.scroll_offset = 0;
}

fn playlist_cursor_end(ui: &mut UiState, playlist: &[PathBuf]) {
    let max = shown_len(ui, playlist).saturating_sub(1);
    ui.cursor = max;
}

fn playlist_cursor_page_up(ui: &mut UiState) {
    // One-line overlap so the old top line becomes the new bottom — easier to track.
    let page = ui.last_visible_rows.saturating_sub(1).max(1);
    ui.cursor = ui.cursor.saturating_sub(page);
    if ui.cursor < ui.scroll_offset {
        ui.scroll_offset = ui.cursor;
    }
}

fn playlist_cursor_page_down(ui: &mut UiState, playlist: &[PathBuf]) {
    let max = shown_len(ui, playlist).saturating_sub(1);
    let page = ui.last_visible_rows.saturating_sub(1).max(1);
    ui.cursor = (ui.cursor + page).min(max);
}

/// Reorder `current` to follow `saved` (the pre-shuffle snapshot): tracks still
/// present keep their saved order, tracks added since (rescan etc.) append at
/// the end in their current relative order.
fn restore_order(saved: &[PathBuf], current: &[PathBuf]) -> Vec<PathBuf> {
    use std::collections::HashSet;
    use std::path::Path;
    let current_set: HashSet<&Path> = current.iter().map(|p| p.as_path()).collect();
    let saved_set: HashSet<&Path> = saved.iter().map(|p| p.as_path()).collect();
    let mut out: Vec<PathBuf> = saved.iter()
        .filter(|p| current_set.contains(p.as_path()))
        .cloned()
        .collect();
    out.extend(current.iter().filter(|p| !saved_set.contains(p.as_path())).cloned());
    out
}

fn remove_track(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    // Resolve cursor to actual playlist index
    let Some(track_idx) = shown_track(ui, ui.cursor) else { return };
    if track_idx >= playlist.len() { return; }
    let removed_name = ui.metadata_cache.display_name(track_idx, &playlist[track_idx]);
    if remove_indices(state, ui, playlist, &[track_idx]) {
        ui.set_status(format!("Removed: {}", removed_name));
    }
}

/// Remove playlist entries — the one place that does it, for the flat list's
/// `D` and the library tree alike.
///
/// Drops the entries (descending, so earlier indices don't shift), records
/// them in `removed_paths` so a rescan or the repeat cycle won't bring them
/// back, keeps the playing index on the playing track, and when the playing
/// track itself goes, skips it and remembers where its successor now sits
/// (`removed_current_next`, possibly `len` = past the end). The queue count
/// loses any queued entries removed. Refuses to empty the playlist. Returns
/// whether anything was removed.
pub(crate) fn remove_indices(
    state: &PlayerState,
    ui: &mut UiState,
    playlist: &mut Vec<PathBuf>,
    indices: &[usize],
) -> bool {
    let mut idx: Vec<usize> = indices.iter().copied().filter(|&i| i < playlist.len()).collect();
    idx.sort_unstable();
    idx.dedup();
    if idx.is_empty() {
        return false;
    }
    let keep = cursor_track(ui, playlist);
    if idx.len() >= playlist.len() {
        ui.set_status("Can't remove every track".to_string());
        return false;
    }

    // Shift the cache through the scan-safe path below: mutating it
    // positionally while the background scan runs lets in-flight workers —
    // which write by the index of the snapshot they were spawned with — land
    // tags in the wrong slots.
    let old_playlist = playlist.clone();
    for &i in idx.iter().rev() {
        let key = std::fs::canonicalize(&playlist[i]).unwrap_or_else(|_| playlist[i].clone());
        ui.removed_paths.insert(key);
        playlist.remove(i);
    }

    let removed_before_current = idx.iter().filter(|&&i| i < ui.current).count();
    // The unfiltered list's cursor moves up past the rows removed above it:
    // left at its index, it pointed at another song (an artist removed from
    // the tree above it), and Enter or `d` acted on that. Its own track gone,
    // it lands on the one after, as before. (A live search re-runs below.)
    if !list_search_open(ui) {
        ui.cursor -= idx.iter().filter(|&&i| i < ui.cursor).count();
    }
    let queue = (ui.current + 1)..=(ui.current + ui.enqueue_count);
    let queued_removed = idx.iter().filter(|i| queue.contains(i)).count();
    ui.enqueue_count -= queued_removed;
    if idx.binary_search(&ui.current).is_ok() {
        // The playing track is gone: its first surviving successor now sits
        // where the removed tracks before it end. That may be past the end —
        // the transition handler then ends the list (or starts the repeat
        // cycle) instead of replaying the track before it. ui.current itself
        // stays a valid index for the display meanwhile.
        let next = ui.current - removed_before_current;
        ui.removed_current_next = Some(next);
        // The first queued track (if any) now sits at `next` and is the one
        // about to play: it leaves the queue now. The jump that starts it
        // goes from `next` to `next`, which the queue rule reads as a
        // restart and would keep it counted.
        ui.enqueue_count = ui.enqueue_count.saturating_sub(1);
        ui.current = next.min(playlist.len() - 1);
        state.next(); // skip the removed track now
    } else {
        ui.current -= removed_before_current;
    }

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(ui.current, Ordering::Relaxed);
    reindex_and_restart_scan(ui, playlist, &old_playlist);

    // A live search re-runs, its cursor kept on its track; otherwise the
    // cursor is just kept in the list.
    refilter_to(ui, playlist, keep);
    let max_cursor = shown_len(ui, playlist).saturating_sub(1);
    ui.cursor = ui.cursor.min(max_cursor);
    true
}

/// The repeat-all wrap, for tracks queued while its rebuild ran: with the
/// list over, "after the playing track" was the end of the list, so that is
/// where they went (in order), and the new cycle starting at 0 played them
/// last. They move to the front: the first starts the cycle, the rest stay
/// queued behind it. Returns the new cycle's queue count.
pub(crate) fn queued_to_front(list: &mut [PathBuf], queued: usize) -> usize {
    let queued = queued.min(list.len());
    list.rotate_right(queued);
    queued.saturating_sub(1)
}

/// Where `path` sits in the list, looked for at `hint` first (its index when
/// it was named) and then nearest to it — a path normally appears once, and
/// the nearest copy is the best guess when it does not.
fn locate(playlist: &[PathBuf], path: &std::path::Path, hint: usize) -> Option<usize> {
    if playlist.get(hint).is_some_and(|p| p == path) {
        return Some(hint);
    }
    playlist
        .iter()
        .enumerate()
        .filter(|(_, p)| p.as_path() == path)
        .min_by_key(|(i, _)| i.abs_diff(hint))
        .map(|(i, _)| i)
}

/// Main's answer to the producer's "what comes after `after`?", from the list
/// as it is now. With no edit since `after` was named, the one after its
/// index (by path, a file listed twice would send it back to the first
/// copy). After an edit: after the playing track, the next one (the queue
/// sits right after it), or — when it was removed — its successor; after a
/// track further ahead (several short tracks in the ring), the one after
/// that. None where the list ends; the repeat-all wrap and the end of
/// playback are main's.
///
/// Files already found unplayable are passed over: they would only fail
/// again, with the same "Skip:" message. And when nothing has produced audio
/// yet and an unplayable `after` that an edit moved has nothing after it (an
/// auto-sort moved a broken first file to the end, before the producer
/// asked), the answer is the first playable track: "past the end" ended the
/// session unheard.
pub(crate) fn next_pick(state: &PlayerState, ui: &UiState, playlist: &[PathBuf], after: &Pick) -> Option<Pick> {
    let len = playlist.len();
    let skip = |i: usize| state.is_unplayable(&playlist[i]);
    let first_playable_from = |from: usize| (from..len).find(|&i| !skip(i));
    let after_index = if after.gen == ui.playlist_gen && playlist.get(after.index).is_some_and(|p| *p == after.path) {
        Some(after.index)
    } else if playlist.get(ui.current).is_some_and(|p| *p == after.path) {
        Some(ui.current)
    } else {
        None
    };
    let next = match after_index {
        Some(i) => i + 1,
        None => match ui.removed_current_next.filter(|_| !playlist.contains(&after.path)) {
            Some(n) => n,
            None => match locate(playlist, &after.path, after.index) {
                Some(i) => i + 1,
                None => ui.current + 1, // a track ahead was removed: carry on from the playing one
            },
        },
    };
    // Only when the list was edited since `after` was named: a broken last
    // track with no edit is genuinely the end (with repeat off, starting
    // again from the top would be wrong).
    let next = first_playable_from(next).or_else(|| {
        let unheard = !state.produced_audio.load(Ordering::Relaxed)
            && state.is_unplayable(&after.path)
            && after.gen != ui.playlist_gen;
        if unheard { first_playable_from(0) } else { None }
    })?;
    Some(Pick { index: next, path: playlist[next].clone(), gen: ui.playlist_gen })
}

/// Answer the producer's question, if it has one, and publish the playlist's
/// edit generation for it. Called from `poll_input`, which every main-thread
/// wait calls, so the producer never waits longer than a UI frame.
pub(crate) fn serve_producer(state: &PlayerState, ui: &UiState, playlist: &[PathBuf]) {
    state.playlist_gen.store(ui.playlist_gen, Ordering::Release);
    if let Some(question) = state.next_question() {
        state.answer_next(&question, next_pick(state, ui, playlist, &question));
    }
}

/// The producer found nothing after the last track and has finished. It asks
/// once more after an edit while that track plays out, but an edit in the
/// very last moment can still have added a track: the same question, asked
/// here. None = the end of the list (the repeat cycle or the end of playback).
pub(crate) fn next_after_end(state: &PlayerState, ui: &UiState, playlist: &[PathBuf], reported: Option<&Pick>) -> Option<usize> {
    if reported.is_some_and(|p| p.gen == ui.playlist_gen) {
        return None; // no edit since the producer's last answer
    }
    let path = playlist.get(ui.current)?;
    let playing = Pick { index: ui.current, path: path.clone(), gen: ui.playlist_gen };
    next_pick(state, ui, playlist, &playing).map(|p| p.index)
}

/// What the player does with the track the producer reports (a transition,
/// or the target of a rate switch).
#[derive(Debug, PartialEq)]
pub(crate) enum Move {
    /// Continue: it is the right track; this is its index now.
    Advance(usize),
    /// It is not (the list was edited after main named it): go here instead,
    /// draining what the producer queued. `len` = past the end.
    Jump(usize),
}

/// The one check on a producer's report. A pick made at the current edit
/// generation is right by construction. One made before an edit is right
/// only if it is still the track that follows now (`track_after_edit`); a
/// track queued, removed or sorted past in the meantime makes it a jump.
///
/// `after_unplayable`: the track shown as playing was never heard (it would
/// not open, or gave no audio), so "what follows it now" means nothing — an
/// auto-sort can move it anywhere, to the end of the list included, which
/// ended playback with a broken first file. The report is taken as named,
/// wherever its track sits now.
pub(crate) fn resolve_report(state: &PlayerState, ui: &UiState, playlist: &[PathBuf], pick: &Pick, after_unplayable: bool) -> Move {
    if pick.gen == ui.playlist_gen && playlist.get(pick.index).is_some_and(|p| *p == pick.path) {
        return Move::Advance(pick.index);
    }
    if after_unplayable {
        if let Some(i) = locate(playlist, &pick.path, pick.index) {
            return Move::Advance(i);
        }
    }
    // Never a file already found unplayable: jumping to one cut the playing
    // track's end for a file that only fails again.
    let mut expected = track_after_edit(ui.current, ui.removed_current_next, playlist.len(), ui.repeat_mode);
    while expected < playlist.len() && state.is_unplayable(&playlist[expected]) {
        expected += 1;
    }
    if playlist.get(expected).is_some_and(|p| *p == pick.path) {
        Move::Advance(expected)
    } else {
        Move::Jump(expected)
    }
}

/// The track that follows the playing one in the list as it is now, by the
/// repeat mode, as a natural track change would. `removed_next` is where the
/// removed playing track's successor sits. `len` means past the end: the
/// playlist loop turns it into the repeat-all cycle or the end of playback,
/// exactly as when the last track finishes.
pub(crate) fn track_after_edit(
    current: usize,
    removed_next: Option<usize>,
    len: usize,
    repeat: RepeatMode,
) -> usize {
    match (removed_next, repeat) {
        (Some(next), _) => next.min(len),
        (None, RepeatMode::One) => current,
        (None, _) => (current + 1).min(len),
    }
}

/// Cancel the in-flight metadata scan, remap the cache to the reordered playlist,
/// then spawn a fresh scan. Reordering the playlist (sort/shuffle/rescan/source-switch)
/// must go through here: the scan workers write metadata by the index of the playlist
/// snapshot they were spawned with, so reindexing without first joining them lets
/// in-flight writes land in the wrong (remapped) cache slots.
pub(crate) fn reindex_and_restart_scan(
    ui: &mut UiState,
    playlist: &[PathBuf],
    old_playlist: &[PathBuf],
) {
    // Every playlist edit passes here: the producer's picks are checked
    // against this (see `resolve_report`). And the list on screen changes.
    ui.playlist_gen += 1;
    ui.dirty = true;
    ui.metadata_cache.cancel.store(true, Ordering::Relaxed);
    if let Some(h) = ui.scan_handle.take() {
        h.join().ok();
    }
    ui.metadata_cache.reindex(playlist, old_playlist);
    ui.metadata_cache.cancel.store(false, Ordering::Relaxed);
    ui.scan_handle = Some(crate::metadata::spawn_metadata_scan(
        playlist.to_vec(),
        std::sync::Arc::clone(&ui.metadata_cache),
    ));
    ui.tree_dirty = true; // playlist reordered/rescanned — the tree needs rebuilding
}

/// True when the sources should be auto-sorted into artist→album order:
/// folders, every one of them. Files listed by hand are an order the user
/// chose, like an M3U playlist's, and keep it; so does a mix of the two.
fn source_is_sortable(paths: &[PathBuf]) -> bool {
    sortable_with(paths, |p| p.is_dir())
}

fn sortable_with(paths: &[PathBuf], is_dir: impl Fn(&std::path::Path) -> bool) -> bool {
    !paths.is_empty() && paths.iter().all(|p| is_dir(p))
}

/// One-shot auto-sort gate: the sort should run only when it's armed, the
/// background metadata scan has finished (so tags are actually loaded), and
/// we're not shuffling (shuffle order is intentional).
fn auto_sort_should_run(pending: bool, scan_finished: bool, shuffle: bool) -> bool {
    pending && scan_finished && !shuffle
}

/// Fire the one-shot artist→album auto-sort once the background metadata scan
/// has loaded tags. Called every UI frame: a no-op until armed and the scan is
/// finished. The flag is spent the moment the scan completes — even if we're
/// shuffling and skip the sort — so it never fires twice or retroactively after
/// a later shuffle toggle.
pub fn poll_auto_sort(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    if !ui.auto_sort_pending {
        return;
    }
    let scan_finished = ui.scan_handle.as_ref().is_some_and(|h| h.is_finished());
    if !scan_finished {
        return; // tags not loaded yet — keep waiting
    }
    let do_sort = auto_sort_should_run(ui.auto_sort_pending, scan_finished, ui.shuffle);
    ui.auto_sort_pending = false;
    if do_sort {
        sort_playlist_by_tags(state, ui, playlist);
    }
}

/// Arm the one-shot auto-sort after a folder-sourced playlist is (re)built
/// (startup / rescan / source-switch). No-op for curated M3U sources or while
/// shuffling; `poll_auto_sort` then fires it once the scan finishes.
pub fn arm_auto_sort(ui: &mut UiState) {
    ui.auto_sort_pending = source_is_sortable(&ui.source_paths) && !ui.shuffle;
}

// ===== Library tree view (artist → album → track browser) =====

/// Rebuild the artist→album tree from the current playlist + metadata cache.
/// Fold state (kept by name on `ui.tree_fold`) survives; the cursor is clamped.
pub fn rebuild_library_tree(ui: &mut UiState, playlist: &[PathBuf]) {
    let tags: Vec<crate::library::TrackTags> = playlist
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (artist, album) = ui.metadata_cache.artist_album(i);
            let title = ui.metadata_cache.title(i).unwrap_or_else(|| {
                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            });
            crate::library::TrackTags {
                artist,
                album,
                disc: ui.metadata_cache.disc_number(i),
                track: ui.metadata_cache.track_number(i),
                title,
            }
        })
        .collect();
    ui.library_tree = crate::library::build(&tags);
    refresh_tree_rows(ui);
    let n = ui.tree_rows.len();
    if ui.tree_cursor >= n {
        ui.tree_cursor = n.saturating_sub(1);
    }
    ui.tree_dirty = false;
}

/// Throttle for rebuilding the tree while the metadata scan is still loading
/// tags: rebuild on the first frame, then at most every 500 ms. Rebuilding the
/// whole tree (full tag projection + sort) at 20 fps for a large library
/// burned CPU for the entire scan with no visible benefit.
fn tree_scan_refresh_due(elapsed_since_last: Option<Duration>) -> bool {
    elapsed_since_last.is_none_or(|e| e >= Duration::from_millis(500))
}

/// Called each UI frame: while the tree view is showing, keep it fresh — rebuild
/// when the playlist changed (`tree_dirty`) or, throttled, while the scan is
/// still loading tags (so `Unknown` rows settle into their real artists as tags
/// arrive). One final rebuild fires when the scan completes, so the last tags
/// to load always land.
pub fn poll_library_tree(ui: &mut UiState, playlist: &[PathBuf]) {
    if ui.library_tree.artists.is_empty() && !playlist.is_empty() {
        ui.tree_dirty = true; // never built yet
    }
    // Only while the tree shows: the playlist panel, whose second pane is
    // the tree — floating with the keys, or docked on screen with or without
    // them (gating on the keys left a docked tree pane empty, then stale).
    // (`base_view`: a floating playlist under the key list or the EQ editor
    // is still drawn, and its tree pane stayed empty while tags loaded.) Not
    // by `library_tree_mode`: it outlives the panel, and a closed one rebuilt
    // the tree twice a second through a whole tag scan. Whatever changed
    // meanwhile is still owed: `tree_dirty` stays set until it shows.
    if base_view(ui) != ViewMode::Playlist && !ui.docked_shown.contains(&ViewMode::Playlist) {
        return;
    }
    let scan_running = ui.scan_handle.as_ref().is_some_and(|h| !h.is_finished());
    if ui.tree_scan_was_running && !scan_running {
        ui.tree_dirty = true; // scan just finished — settle the final tags
    }
    ui.tree_scan_was_running = scan_running;
    let refresh_due = scan_running
        && tree_scan_refresh_due(ui.tree_scan_refreshed_at.map(|t| t.elapsed()));
    if ui.tree_dirty || refresh_due {
        rebuild_library_tree(ui, playlist);
        ui.tree_scan_refreshed_at = Some(std::time::Instant::now());
        ui.dirty = true; // on screen: drawn now, not at the idle rate
    }
}

/// Render the tree body for a themed library renderer: adjust scroll to keep the
/// cursor visible (small margin), record the viewport height for paging, and
/// return the palette-coloured lines — the cursor's with its bar when
/// `show_cursor` (the pane has the keys).
pub fn render_tree_body(
    ui: &mut UiState,
    height: usize,
    width: usize,
    p: &crate::theme::Palette,
    show_cursor: bool,
) -> Vec<Line<'static>> {
    let n = ui.tree_rows.len();
    if ui.tree_cursor >= n {
        ui.tree_cursor = n.saturating_sub(1);
    }
    ui.tree_view_height = height;
    let margin = 4.min(height / 2);
    let visible = ui.tree_cursor >= ui.tree_scroll && ui.tree_cursor < ui.tree_scroll + height;
    if ui.tree_clicked.take() == Some(ui.tree_cursor) && visible {
        // Put by a click: the row stays under the pointer.
    } else if ui.tree_cursor < ui.tree_scroll + margin {
        ui.tree_scroll = ui.tree_cursor.saturating_sub(margin);
    } else if ui.tree_cursor + margin + 1 > ui.tree_scroll + height {
        ui.tree_scroll = (ui.tree_cursor + margin + 1).saturating_sub(height);
    }
    let max_scroll = n.saturating_sub(height);
    if ui.tree_scroll > max_scroll {
        ui.tree_scroll = max_scroll;
    }
    crate::library::render_library_tree(
        &ui.library_tree,
        &ui.tree_fold,
        &ui.tree_rows,
        // No bar in the pane without the keys: its rows keep their own
        // colours (redrawn plain, the playing track lost its accent).
        if show_cursor { ui.tree_cursor } else { usize::MAX },
        ui.tree_scroll,
        height,
        width,
        p,
        Some(ui.current),
    )
}

/// Re-materialize `ui.tree_rows`: the filtered projection when a `/` filter is
/// active, else the normal fold-based rows. Must be called after every tree,
/// fold, or filter mutation — navigation and rendering read the cache instead
/// of rebuilding the projection per keypress/frame (which cloned artist/album
/// names for every row, every time, on large libraries).
fn refresh_tree_rows(ui: &mut UiState) {
    ui.tree_rows = if ui.tree_filter.is_empty() {
        crate::library::visible_rows(&ui.library_tree, &ui.tree_fold)
    } else {
        crate::library::visible_rows_filtered(&ui.library_tree, &ui.tree_filter)
    };
}

fn tree_visible_len(ui: &UiState) -> usize {
    ui.tree_rows.len()
}

fn tree_row_at_cursor(ui: &UiState) -> Option<crate::library::VisibleRow> {
    ui.tree_rows.get(ui.tree_cursor).copied()
}

/// Re-read the `/` query into `ui.tree_filter` and clamp the cursor to the new
/// filtered row count. Called on each keystroke while searching in the tree.
fn rebuild_tree_filter(ui: &mut UiState) {
    ui.tree_filter = match &ui.input_mode {
        InputMode::Search(q) => q.text().to_string(),
        _ => String::new(),
    };
    refresh_tree_rows(ui);
    let n = tree_visible_len(ui);
    if ui.tree_cursor >= n {
        ui.tree_cursor = n.saturating_sub(1);
    }
    ui.tree_scroll = 0;
}

fn tree_move(ui: &mut UiState, delta: isize) {
    let n = tree_visible_len(ui) as isize;
    if n == 0 {
        ui.tree_cursor = 0;
        return;
    }
    ui.tree_cursor = (ui.tree_cursor as isize + delta).clamp(0, n - 1) as usize;
}

fn tree_page(ui: &mut UiState, dir: isize) {
    let page = ui.tree_view_height.max(1) as isize;
    tree_move(ui, dir * page);
}

fn tree_expand_under_cursor(ui: &mut UiState) {
    if let Some(row) = tree_row_at_cursor(ui) {
        crate::library::expand(&ui.library_tree, &mut ui.tree_fold, row);
        refresh_tree_rows(ui);
    }
}

fn tree_collapse_under_cursor(ui: &mut UiState) {
    use crate::library::VisibleRow;
    if let Some(row) = tree_row_at_cursor(ui) {
        if let VisibleRow::Track { artist, album, .. } = row {
            // Collapse the parent album and land the cursor on it.
            let album_row = VisibleRow::Album { artist, album };
            crate::library::collapse(&ui.library_tree, &mut ui.tree_fold, album_row);
            refresh_tree_rows(ui);
            if let Some(pos) = ui.tree_rows.iter().position(|r| *r == album_row) {
                ui.tree_cursor = pos;
            }
        } else {
            crate::library::collapse(&ui.library_tree, &mut ui.tree_fold, row);
            refresh_tree_rows(ui);
        }
    }
    let n = tree_visible_len(ui);
    if ui.tree_cursor >= n {
        ui.tree_cursor = n.saturating_sub(1);
    }
}

/// The playlist index `Enter` plays: the first track under the cursor (a track →
/// itself, an album → its first track, an artist → their first track).
fn tree_cursor_play_index(ui: &UiState) -> Option<usize> {
    tree_row_at_cursor(ui).and_then(|row| crate::library::first_track_index(&ui.library_tree, row))
}

/// Remove the tracks under the cursor. A track removes just itself; an album or
/// artist stages a confirmation (`tree_pending_remove`) that `y` completes.
fn tree_remove_under_cursor(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    let Some(row) = tree_row_at_cursor(ui) else { return };
    let indices = crate::library::subtree_track_indices(&ui.library_tree, row);
    if indices.is_empty() {
        return;
    }
    match row {
        crate::library::VisibleRow::Track { .. } => {
            tree_remove_indices(state, ui, playlist, &indices);
        }
        crate::library::VisibleRow::Album { artist, album } => {
            let label = format!("album {}", ui.library_tree.artists[artist].albums[album].name);
            ui.tree_pending_remove = Some((label, paths_at(playlist, &indices)));
        }
        crate::library::VisibleRow::Artist { artist } => {
            let label = format!("artist {}", ui.library_tree.artists[artist].name);
            ui.tree_pending_remove = Some((label, paths_at(playlist, &indices)));
        }
    }
}

/// The paths at `indices` (a staged removal outlives the positions).
fn paths_at(playlist: &[PathBuf], indices: &[usize]) -> Vec<PathBuf> {
    indices.iter().filter_map(|&i| playlist.get(i).cloned()).collect()
}

/// Where `paths` sit in the playlist NOW (any that left it are skipped).
fn indices_of(playlist: &[PathBuf], paths: &[PathBuf]) -> Vec<usize> {
    let wanted: std::collections::HashSet<&PathBuf> = paths.iter().collect();
    (0..playlist.len()).filter(|&i| wanted.contains(&playlist[i])).collect()
}

/// Remove a library-tree selection (an artist, album or track) through the
/// same path as the flat list, then reset the tree's cursor.
fn tree_remove_indices(
    state: &PlayerState,
    ui: &mut UiState,
    playlist: &mut Vec<PathBuf>,
    indices: &[usize],
) {
    if remove_indices(state, ui, playlist, indices) {
        ui.tree_cursor = 0;
        ui.tree_scroll = 0;
        ui.set_status(format!("removed {} track(s)", indices.len()));
    }
}

/// Sort the playlist by tag metadata: artist → album → disc → track → title → filename.
/// Tracks without any tags fall to the bottom (sorted among themselves by filename).
/// Preserves the currently-playing track's logical position.
fn sort_playlist_by_tags(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    if playlist.len() < 2 {
        ui.set_status("Nothing to sort".to_string());
        return;
    }

    let old_playlist = playlist.clone();
    let current_path = playlist.get(ui.current).cloned();
    let keep = cursor_track(ui, playlist);
    ui.enqueue_count = 0; // a sort scatters the queue

    // (bucket, artist, album, disc, track, title, filename). The leading u8
    // partitions tagged-vs-untagged so tracks without tags cluster at the bottom
    // rather than mingling alphabetically.
    type SortKey = (u8, String, String, u32, u32, String, String);
    let mut keyed: Vec<(SortKey, PathBuf)> =
        playlist.iter().enumerate().map(|(i, p)| {
            let (artist, album) = ui.metadata_cache.artist_album(i);
            let title = ui.metadata_cache.title(i);
            let track_no = ui.metadata_cache.track_number(i).unwrap_or(0);
            let disc_no = ui.metadata_cache.disc_number(i).unwrap_or(0);
            let filename = p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            let bucket = if artist.is_some() || album.is_some() || title.is_some() { 0 } else { 1 };
            let key = (
                bucket,
                artist.unwrap_or_default().to_lowercase(),
                album.unwrap_or_default().to_lowercase(),
                disc_no,
                track_no,
                title.unwrap_or_default().to_lowercase(),
                filename,
            );
            (key, p.clone())
        }).collect();

    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    *playlist = keyed.into_iter().map(|(_, p)| p).collect();

    // Re-locate the playing track in the new ordering.
    if let Some(ref cp) = current_path {
        if let Some(idx) = playlist.iter().position(|p| p == cp) {
            ui.current = idx;
        }
    }

    // Sorting invalidates the enqueue queue (positions no longer reflect user intent).
    ui.enqueue_count = 0;

    reindex_and_restart_scan(ui, playlist, &old_playlist);
    state.current_track.store(ui.current, Ordering::Relaxed);
    if list_search_open(ui) {
        // A live search's hits are playlist indices into the OLD order, and
        // its cursor is a position in that hit list — re-run the query, the
        // cursor kept on its track. (The one-shot auto-sort fires in the
        // background, so it can land mid-search.)
        refilter_to(ui, playlist, keep);
    } else {
        ui.cursor = ui.current;
    }
    let was_shuffled = ui.shuffle;
    ui.shuffle = false;
    // The user picked an explicit new order; the pre-shuffle snapshot is stale.
    ui.pre_shuffle_order = None;
    ui.set_status(
        if was_shuffled { "Sorted by tags (shuffle off)" } else { "Sorted by tags" }.to_string()
    );
}

/// Toggle runtime shuffle. When turning ON, snapshots the current order and
/// shuffles the tracks after the current one (so the now-playing song isn't
/// interrupted). When turning OFF, restores the snapshotted order — sorting
/// would destroy an M3U's curated order — falling back to a path sort when no
/// snapshot exists (e.g. the session started with --shuffle).
fn toggle_shuffle(state: &PlayerState, ui: &mut UiState, playlist: &mut [PathBuf]) {
    let old_playlist = playlist.to_vec();
    // The cursor's track and the filtered tracks, by path: the reorder moves
    // them, and the cursor stayed on its ROW (another track) with the search
    // results pointing at whatever now sat at their old positions.
    let cursor_track = shown_track(ui, ui.cursor).and_then(|i| playlist.get(i)).cloned();
    let filtered: std::collections::HashSet<PathBuf> =
        ui.filtered_indices.iter().filter_map(|&i| playlist.get(i).cloned()).collect();
    // The queued tracks are part of the tail being reordered.
    ui.enqueue_count = 0;
    ui.shuffle = !ui.shuffle;
    let current_path = playlist.get(ui.current).cloned();

    if ui.shuffle {
        ui.pre_shuffle_order = Some(old_playlist.clone());
        // Shuffle everything after the currently-playing track
        let start = ui.current + 1;
        if start < playlist.len() {
            let tail = &mut playlist[start..];
            crate::playlist::shuffle_list(tail);
        }
        ui.set_status("Shuffle ON".to_string());
    } else {
        let restored = match ui.pre_shuffle_order.take() {
            Some(saved) => restore_order(&saved, playlist),
            None => {
                let mut sorted = playlist.to_vec();
                sorted.sort();
                sorted
            }
        };
        if restored.len() == playlist.len() {
            playlist.clone_from_slice(&restored);
        } else {
            // Duplicate/uncanonicalizable paths can make restore_order return a
            // different length; clone_from_slice would panic on the mismatch.
            // Fall back to the same path sort used when no snapshot exists.
            playlist.sort();
        }
        if let Some(ref cp) = current_path {
            if let Some(idx) = playlist.iter().position(|p| p == cp) {
                ui.current = idx;
            }
        }
        ui.set_status("Shuffle OFF".to_string());
    }
    // Every header reads the playing track's index from state: it named the
    // track now at the OLD position until the next track change.
    state.current_track.store(ui.current, Ordering::Relaxed);
    if !filtered.is_empty() {
        ui.filtered_indices = (0..playlist.len()).filter(|&i| filtered.contains(&playlist[i])).collect();
    }
    if let Some(track) = cursor_track {
        let pos = if list_filtered(ui) {
            ui.filtered_indices.iter().position(|&i| playlist[i] == track)
        } else {
            playlist.iter().position(|p| *p == track)
        };
        if let Some(pos) = pos {
            ui.cursor = pos;
        }
    }
    // Cached metadata is indexed by position — remap it to match the reordered paths.
    reindex_and_restart_scan(ui, playlist, &old_playlist);
}

/// The playing track becomes `new`, by any route: a natural advance, a jump,
/// a skip, an exclusive-mode rate change. The one place `current`, the queue
/// count and the headers' track index change together, so no route can skip
/// the queue rule.
pub(crate) fn advance_to(ui: &mut UiState, state: &PlayerState, new: usize) {
    // The screen changes (title, cursor, next): a frame now, not at the
    // idle rate.
    ui.dirty = true;
    ui.enqueue_count = queue_after_advance(ui.enqueue_count, ui.current, new);
    ui.current = new;
    // The removed track's successor has now been reached (or left behind).
    ui.removed_current_next = None;
    state.current_track.store(new, Ordering::Relaxed);
}

/// The queue after the playing track moves from `old` to `new`: one fewer
/// when the next track starts (the first queued one, while there is a
/// queue), unchanged when the same track restarts (repeat-one), and gone after
/// any other move, which leaves the queued tracks somewhere else entirely.
/// (Emptied on every change, a track queued later would jump ahead of ones
/// still waiting.)
pub(crate) fn queue_after_advance(count: usize, old: usize, new: usize) -> usize {
    if new == old + 1 {
        count.saturating_sub(1)
    } else if new == old {
        count
    } else {
        0
    }
}

fn toggle_repeat(ui: &mut UiState, state: &PlayerState) {
    ui.repeat_mode = ui.repeat_mode.next();
    state.repeat_mode.store(ui.repeat_mode as u8, Ordering::Relaxed);
    let msg = match ui.repeat_mode {
        crate::state::RepeatMode::Off => "Repeat OFF",
        crate::state::RepeatMode::All => "Repeat ALL",
        crate::state::RepeatMode::One => "Repeat ONE",
    };
    ui.set_status(msg.to_string());
}

/// Only reachable with the flat list's search CLOSED: while a search is open
/// every letter (`a` included) goes into the query, and Enter/Esc clear
/// `filtered_indices`. So `ui.cursor` is a playlist position by the time the
/// move below adjusts it; the filtered lookup just above is the general
/// cursor-to-track rule shared with `remove_track`. (The library tree keeps
/// its own filter and queues through its own path.)
pub(crate) fn enqueue_track(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    let Some(track_idx) = shown_track(ui, ui.cursor) else { return };
    if track_idx >= playlist.len() || track_idx == ui.current { return; }
    let keep = cursor_track(ui, playlist);
    // Already in the queue: queueing it again only grew the count, and the
    // next track queued then landed past a track that was never queued.
    if track_idx > ui.current && track_idx <= ui.current + ui.enqueue_count {
        ui.set_status("Already queued".to_string());
        return;
    }

    // Target position: right after current + any previously enqueued tracks.
    // Clamped to the END (len), not the last index: with the last track
    // playing, `len - 1` put the queued track before it, never to be played.
    let target = (ui.current + 1 + ui.enqueue_count).min(playlist.len());

    let name = ui.metadata_cache.display_name(track_idx, &playlist[track_idx]);
    if track_idx == target {
        // Already where it would go (the track right after the queue): it is
        // queued without moving, and counted.
        ui.enqueue_count += 1;
        ui.set_status(format!("Queued: {name}"));
        return;
    }

    // Move the track in the playlist, then remap the cache through the
    // scan-safe path below (same hazard as remove_track: a positional
    // move_entry races in-flight scan workers writing by stale indices).
    let old_playlist = playlist.clone();
    let path = playlist.remove(track_idx);
    let dst = if track_idx < target { target - 1 } else { target };
    playlist.insert(dst, path);

    // Recalculate ui.current — it may have shifted
    // If we removed before current, current shifted down; if we inserted at/before current, it shifted up
    if track_idx < ui.current && dst >= ui.current {
        ui.current -= 1;
    } else if track_idx > ui.current && dst <= ui.current {
        ui.current += 1;
    }

    // Keep cursor on the same logical track
    if track_idx == ui.cursor {
        ui.cursor = dst;
    } else if track_idx < ui.cursor && dst >= ui.cursor {
        ui.cursor -= 1;
    } else if track_idx > ui.cursor && dst <= ui.cursor {
        ui.cursor += 1;
    }

    ui.enqueue_count += 1;
    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(ui.current, Ordering::Relaxed);
    reindex_and_restart_scan(ui, playlist, &old_playlist);
    // With a search, the moves above were list positions, not the hits':
    // re-run it, the cursor on the queued track.
    refilter_to(ui, playlist, keep);
    ui.set_status(format!("Queued: {}", name));
}

/// Replace the current music source with a new path, rebuild the playlist,
/// reindex the metadata cache, and jump playback to the new first track.
fn switch_source_paths(
    state: &PlayerState,
    ui: &mut UiState,
    playlist: &mut Vec<PathBuf>,
    new_path: PathBuf,
) {
    use std::sync::atomic::Ordering;

    if !new_path.exists() {
        ui.set_status(format!("Path not found: {}", new_path.display()));
        return;
    }

    // Honor the current session's shuffle setting. Repeat is preserved implicitly —
    // main.rs's repeat-cycle loop keeps running regardless of source.
    let new_list = match crate::playlist::build_playlist(&new_path, ui.shuffle) {
        Ok(list) => list,
        Err(e) => {
            ui.set_status(format!("Failed to read source: {}", e));
            return;
        }
    };

    // A search left open belongs to the old source: it goes (with its hits,
    // indices into the old list — kept, the next frame ran past the new
    // list's end). `o` and a paste close it the same way.
    cancel_field(ui);
    let old_playlist = std::mem::replace(playlist, new_list);
    ui.source_paths = vec![new_path.clone()];
    // A rescan still running describes the OLD sources: landing afterwards,
    // its result replaced the new playlist. Dropping the receiver discards it.
    ui.rescan_receiver = None;
    ui.pre_shuffle_order = None; // snapshot belongs to the previous source
    ui.current = 0;
    // The queue was positions in the old playlist. Emptied here: the jump
    // would keep it when the old track was also index 0 (advance_to keeps
    // the queue when the index does not move).
    ui.enqueue_count = 0;
    ui.cursor = 0;
    ui.scroll_offset = 0;
    // The tree's filter too, kept after its search closed (Enter): the new
    // library showed only rows matching the old one's query, with no field
    // saying why. The tree is rebuilt for the new list (`tree_dirty`).
    ui.tree_filter.clear();
    ui.tree_cursor = 0;
    ui.tree_scroll = 0;

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(0, Ordering::Relaxed);
    state.forget_unplayable(true);

    reindex_and_restart_scan(ui, playlist, &old_playlist);
    arm_auto_sort(ui); // new folder source → auto-sort once its tags load

    // Signal the producer to break out of the current track and jump to index 0
    // of the new playlist on its next iteration.
    state.jump_to(0);

    let name = new_path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| new_path.display().to_string());
    ui.set_status(format!("Source: {} ({} tracks)", name, playlist.len()));
}

/// Start a rescan of every source. The directory walk and path lookups run on
/// a worker thread (on a large or network library they take seconds);
/// [`poll_rescan`] applies the result when it lands.
fn rescan(ui: &mut UiState, playlist: &[PathBuf]) {
    if ui.rescan_receiver.is_some() {
        ui.set_status("Rescan already running".to_string());
        return;
    }
    let sources = ui.source_paths.clone();
    let snapshot = playlist.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(crate::playlist::scan_sources(&sources, &snapshot));
    });
    ui.rescan_receiver = Some(rx);
    ui.set_status("Rescanning…".to_string());
}

/// Apply a finished background rescan, if one has landed. Called every frame.
pub fn poll_rescan(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    let Some(rx) = ui.rescan_receiver.as_ref() else { return };
    match rx.try_recv() {
        Ok(result) => {
            ui.rescan_receiver = None;
            finish_rescan(state, ui, playlist, &result);
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
        Err(std::sync::mpsc::TryRecvError::Disconnected) => ui.rescan_receiver = None,
    }
}

fn finish_rescan(
    state: &PlayerState,
    ui: &mut UiState,
    playlist: &mut Vec<PathBuf>,
    result: &crate::playlist::RescanResult,
) {
    use std::sync::atomic::Ordering;

    let old_playlist = playlist.clone();
    let current_track_path = playlist.get(ui.current).cloned();
    let keep = cursor_track(ui, playlist);
    // A rescan is how a repaired file comes back: it is tried again.
    state.forget_unplayable(false);
    let (total_added, total_removed) = crate::playlist::apply_rescan(
        playlist,
        result,
        current_track_path.as_deref(),
        &ui.removed_paths,
    );
    let had_error = result.had_error;

    // Find current track's new index
    if let Some(ref track_path) = current_track_path {
        if let Some(new_idx) = playlist.iter().position(|p| p == track_path) {
            ui.current = new_idx;
        } else {
            ui.current = ui.current.min(playlist.len().saturating_sub(1));
        }
    }
    // A reorder invalidates the queue's position count.
    ui.enqueue_count = 0;

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(ui.current, Ordering::Relaxed);

    reindex_and_restart_scan(ui, playlist, &old_playlist);
    arm_auto_sort(ui); // re-settle newly-added tracks into artist→album order

    // Removed tracks can leave the list cursor past the end; Enter then asked
    // for a track that no longer exists. A live search re-runs its query (its
    // hits are indices into the old list).
    if list_search_open(ui) {
        refilter_to(ui, playlist, keep);
    } else {
        ui.cursor = ui.cursor.min(playlist.len().saturating_sub(1));
        ui.scroll_offset = ui.scroll_offset.min(ui.cursor);
    }

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

/// Opens a native folder-picker dialog on macOS via AppleScript.
#[cfg(target_os = "macos")]
fn pick_folder_native() -> Option<PathBuf> {
    let output = std::process::Command::new("osascript")
        .args([
            "-e",
            "try\nPOSIX path of (choose folder with prompt \"Select a music folder\")\non error\nreturn \"\"\nend try",
        ])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(PathBuf::from(s)) }
}

/// Opens a native folder-picker dialog on Windows via PowerShell's Shell.Application COM object.
#[cfg(target_os = "windows")]
fn pick_folder_native() -> Option<PathBuf> {
    let script = "$s = New-Object -ComObject Shell.Application; \
        $f = $s.BrowseForFolder(0, 'Select a music folder', 0, 0); \
        if ($f) { $f.Self.Path }";
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(PathBuf::from(s)) }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn pick_folder_native() -> Option<PathBuf> { None }

fn has_native_picker() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// The first-launch screen (`welcome.rs`), shown when keet runs with no
/// sources and no saved session. Returns the chosen folder, or None if the
/// user quit.
pub fn run_first_launch_picker(kind: crate::theme::ThemeKind) -> Option<PathBuf> {
    crate::welcome::run(kind, has_native_picker(), pick_folder_native)
}

#[cfg(test)]
mod ui_tests {
    use super::*;

    fn visible_len(l: &Line) -> usize {
        crate::tui::Cols::cols(l)
    }

    #[test]
    fn shuffle_keeps_the_cursor_and_the_search_results_on_their_tracks() {
        let state = PlayerState::new();
        let mut ui = test_ui(4);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3"), p("/d.mp3")];
        ui.shuffle = true;
        ui.pre_shuffle_order = Some(playlist.clone());
        playlist = vec![p("/a.mp3"), p("/d.mp3"), p("/c.mp3"), p("/b.mp3")];
        ui.current = 0;
        ui.filtered_indices = vec![1, 3]; // d and b match the search
        ui.cursor = 0; // on d
        toggle_shuffle(&state, &mut ui, &mut playlist); // off: a b c d
        let shown: Vec<&PathBuf> = ui.filtered_indices.iter().map(|&i| &playlist[i]).collect();
        assert_eq!(shown, [&p("/b.mp3"), &p("/d.mp3")], "the results are still b and d");
        assert_eq!(playlist[ui.filtered_indices[ui.cursor]], p("/d.mp3"), "the cursor is still on d");
    }

    #[test]
    fn help_closes_back_to_where_it_opened_and_cancels_a_pending_removal() {
        let mut ui = test_ui(3);
        ui.view_mode = ViewMode::Lyrics;
        toggle_help(&mut ui);
        assert_eq!(ui.view_mode, ViewMode::Help);
        toggle_help(&mut ui);
        assert_eq!(ui.view_mode, ViewMode::Lyrics, "back to lyrics, not the player");
        ui.view_mode = ViewMode::Playlist;
        ui.tree_pending_remove = Some(("artist X".into(), vec![p("/a.mp3")]));
        toggle_help(&mut ui);
        assert!(ui.tree_pending_remove.is_none(), "a later y must not remove anything");
    }

    /// The producer's question for the track after `after`, through the
    /// real slot, answered as poll_input answers it (`serve_producer`).
    fn ask(state: &PlayerState, ui: &UiState, playlist: &[PathBuf], after: &Pick) -> Option<Pick> {
        state.ask_next(after.clone());
        serve_producer(state, ui, playlist);
        state.take_next_answer().expect("main answers in poll_input")
    }

    /// The playing track, as the producer holds it (named at this generation).
    fn playing(ui: &UiState, playlist: &[PathBuf]) -> Pick {
        Pick { index: ui.current, path: playlist[ui.current].clone(), gen: ui.playlist_gen }
    }

    /// The producer reports `pick` as started: the transition handler's
    /// check, then the move (a jump also ends in advance_to, after the
    /// respawn).
    fn report(state: &PlayerState, ui: &mut UiState, playlist: &[PathBuf], pick: &Pick) -> Move {
        let m = resolve_report(state, ui, playlist, pick, false);
        let (Move::Advance(i) | Move::Jump(i)) = m;
        if i < playlist.len() {
            advance_to(ui, state, i);
        }
        m
    }

    /// A natural track change, the way the player makes it: the producer
    /// asks for the track after the playing one and later reports it started.
    fn play_next(state: &PlayerState, ui: &mut UiState, playlist: &[PathBuf]) {
        let next = ask(state, ui, playlist, &playing(ui, playlist)).expect("a next track");
        assert_eq!(report(state, ui, playlist, &next), Move::Advance(next.index), "natural, no jump");
    }

    fn names(playlist: &[PathBuf]) -> Vec<String> {
        playlist.iter().map(|x| x.file_stem().unwrap().to_string_lossy().into_owned()).collect()
    }

    fn list_of(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| p(&format!("/{n}.mp3"))).collect()
    }

    fn queue_named(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>, name: &str) {
        ui.cursor = playlist.iter().position(|x| x == &p(&format!("/{name}.mp3"))).unwrap();
        enqueue_track(state, ui, playlist);
    }

    #[test]
    fn a_track_queued_during_the_last_track_plays_next() {
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = list_of(&["a", "b", "c"]);
        ui.current = 2;
        // Queued before the producer asks: the answer is the queued track.
        queue_named(&state, &mut ui, &mut playlist, "a");
        assert_eq!(names(&playlist), ["b", "c", "a"]);
        let next = ask(&state, &ui, &playlist, &playing(&ui, &playlist)).expect("the queued track");
        assert_eq!(next.path, p("/a.mp3"));
        assert_eq!(report(&state, &mut ui, &playlist, &next), Move::Advance(2));
        assert_eq!(ui.enqueue_count, 0);
        // Queued after it asked (the list had ended): the generation moved,
        // so the producer asks again once the last track has played out.
        let after = playing(&ui, &playlist);
        assert_eq!(ask(&state, &ui, &playlist, &after), None, "the end of the list");
        queue_named(&state, &mut ui, &mut playlist, "b");
        assert_ne!(state.playlist_gen.load(Ordering::Relaxed), ui.playlist_gen, "not yet published");
        let next = ask(&state, &ui, &playlist, &after).expect("asked again: the queued track");
        assert_eq!(state.playlist_gen.load(Ordering::Relaxed), ui.playlist_gen);
        assert_eq!(next.path, p("/b.mp3"));
    }

    #[test]
    fn removing_the_playing_track_starts_the_first_queued_one_and_dequeues_it() {
        let state = PlayerState::new();
        let mut ui = test_ui(5);
        let mut playlist = list_of(&["a", "b", "c", "d", "e"]);
        ui.current = 0;
        queue_named(&state, &mut ui, &mut playlist, "d");
        queue_named(&state, &mut ui, &mut playlist, "e");
        assert_eq!(names(&playlist), ["a", "d", "e", "b", "c"]);
        let a = playing(&ui, &playlist);
        assert!(remove_indices(&state, &mut ui, &mut playlist, &[0]));
        // The producer skips a and asks for the track after it.
        let next = ask(&state, &ui, &playlist, &a).expect("a successor");
        assert_eq!(next.path, p("/d.mp3"));
        assert_eq!(report(&state, &mut ui, &playlist, &next), Move::Advance(0));
        assert_eq!(ui.enqueue_count, 1, "only e is still queued");
        assert_eq!(ui.removed_current_next, None, "reached, so forgotten");
        queue_named(&state, &mut ui, &mut playlist, "c");
        assert_eq!(names(&playlist), ["d", "e", "c", "b"], "c lands after e, not after a phantom");
    }

    #[test]
    fn after_removing_the_playing_track_its_successor_plays_and_the_next_change_is_natural() {
        // The rate-change case: the successor is what the producer is told
        // (and what the stream is rebuilt for), and the change after it is
        // a plain one — the old stale index played c, then jumped back to b.
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = list_of(&["a", "b", "c"]);
        ui.current = 0;
        let a = playing(&ui, &playlist);
        assert!(remove_indices(&state, &mut ui, &mut playlist, &[0]));
        let next = ask(&state, &ui, &playlist, &a).unwrap();
        assert_eq!(report(&state, &mut ui, &playlist, &next), Move::Advance(0));
        assert_eq!(playlist[ui.current], p("/b.mp3"));
        play_next(&state, &mut ui, &playlist);
        assert_eq!(playlist[ui.current], p("/c.mp3"));
    }

    #[test]
    fn an_edit_after_the_answer_turns_the_report_into_a_jump_only_if_it_must() {
        let state = PlayerState::new();
        let mut ui = test_ui(4);
        let mut playlist = list_of(&["a", "b", "c", "d"]);
        ui.current = 0;
        // b was named; then d is queued: d now follows a, so b is wrong.
        let b = ask(&state, &ui, &playlist, &playing(&ui, &playlist)).unwrap();
        queue_named(&state, &mut ui, &mut playlist, "d");
        assert_eq!(report(&state, &mut ui, &playlist, &b), Move::Jump(1));
        assert_eq!(playlist[ui.current], p("/d.mp3"));
        // An edit that leaves the next track where it was costs nothing.
        let mut ui = test_ui(4);
        let mut playlist = list_of(&["a", "b", "c", "d"]);
        let b = ask(&state, &ui, &playlist, &playing(&ui, &playlist)).unwrap();
        assert!(remove_indices(&state, &mut ui, &mut playlist, &[3]));
        assert_eq!(report(&state, &mut ui, &playlist, &b), Move::Advance(1), "b still follows a");
    }

    #[test]
    fn a_track_further_ahead_is_followed_by_its_own_successor() {
        // Several short tracks in the ring: the producer asks after b while
        // a still plays.
        let state = PlayerState::new();
        let ui = test_ui(3);
        let playlist = list_of(&["a", "b", "c"]);
        let b = Pick { index: 1, path: p("/b.mp3"), gen: ui.playlist_gen };
        assert_eq!(ask(&state, &ui, &playlist, &b).map(|n| n.path), Some(p("/c.mp3")));
        let c = Pick { index: 2, path: p("/c.mp3"), gen: ui.playlist_gen };
        assert_eq!(ask(&state, &ui, &playlist, &c), None);
    }

    #[test]
    fn a_file_listed_twice_is_followed_by_what_follows_that_copy() {
        // a, b, a, c with several short tracks in the ring: the producer asks
        // after the SECOND a while the first still plays. Matched by path
        // against the playing track, the answer was b again — a loop.
        let state = PlayerState::new();
        let ui = test_ui(4);
        let playlist = list_of(&["a", "b", "a", "c"]);
        let second_a = Pick { index: 2, path: p("/a.mp3"), gen: ui.playlist_gen };
        assert_eq!(ask(&state, &ui, &playlist, &second_a).map(|n| n.index), Some(3));
    }

    #[test]
    fn known_unplayable_files_are_passed_over_after_an_edit_too() {
        // b failed before; after an edit the report check must not send the
        // player back to it (a jump that cut the playing track's end).
        let state = PlayerState::new();
        let mut ui = test_ui(4);
        let mut playlist = list_of(&["a", "b", "c", "d"]);
        state.mark_unplayable(&p("/b.mp3"), false);
        let c = ask(&state, &ui, &playlist, &playing(&ui, &playlist)).unwrap();
        assert_eq!(c.path, p("/c.mp3"), "the answer skips b");
        assert!(remove_indices(&state, &mut ui, &mut playlist, &[3]), "an edit: the pick is now stale");
        assert_eq!(report(&state, &mut ui, &playlist, &c), Move::Advance(2), "c still follows (b is skipped)");
    }

    #[test]
    fn an_answer_nobody_waits_for_is_not_given() {
        // The producer unwound (a jump, quit) and cancelled its question:
        // main must not leave an answer for the next producer to read.
        let state = PlayerState::new();
        let ui = test_ui(2);
        let playlist = list_of(&["a", "b"]);
        state.ask_next(playing(&ui, &playlist));
        state.cancel_next_question();
        serve_producer(&state, &ui, &playlist);
        assert_eq!(state.take_next_answer(), None);
    }

    #[test]
    fn tracks_queued_during_the_repeat_all_rebuild_start_the_next_cycle() {
        // The list is over (current = len) while the rebuild runs; queued
        // tracks go to the end, in order. The new cycle starts with them.
        let state = PlayerState::new();
        let mut ui = test_ui(4);
        let mut playlist = list_of(&["a", "b", "c", "d"]);
        ui.current = playlist.len();
        queue_named(&state, &mut ui, &mut playlist, "b");
        queue_named(&state, &mut ui, &mut playlist, "a");
        assert_eq!(names(&playlist), ["c", "d", "b", "a"]);
        let count = queued_to_front(&mut playlist, ui.enqueue_count);
        assert_eq!(names(&playlist), ["b", "a", "c", "d"]);
        assert_eq!(count, 1, "b plays first, a stays queued behind it");
        assert_eq!(queued_to_front(&mut playlist, 0), 0);
        assert_eq!(names(&playlist), ["b", "a", "c", "d"], "nothing queued: untouched");
        // Queueing the track already last: the order does not change (so the
        // rebuild must go by the count), and it still starts the cycle.
        let mut ui = test_ui(3);
        let mut playlist = list_of(&["a", "b", "c"]);
        ui.current = playlist.len();
        queue_named(&state, &mut ui, &mut playlist, "c");
        assert_eq!((names(&playlist), ui.enqueue_count), (vec!["a".to_string(), "b".into(), "c".into()], 1));
        assert_eq!(queued_to_front(&mut playlist, ui.enqueue_count), 0);
        assert_eq!(names(&playlist), ["c", "a", "b"]);
    }

    #[test]
    fn the_queue_keeps_its_order_through_a_real_track_change() {
        // Queue d and e; when d starts, a newly queued f must land after e.
        // The count was zeroed on that change (it goes through the jump
        // path), so f jumped ahead of e and e lost its queued marker.
        let state = PlayerState::new();
        let mut ui = test_ui(6);
        let mut playlist: Vec<PathBuf> = ["a", "b", "c", "d", "e", "f"].iter().map(|n| p(&format!("/{n}.mp3"))).collect();
        ui.current = 0;
        let queue = |ui: &mut UiState, playlist: &mut Vec<PathBuf>, name: &str| {
            ui.cursor = playlist.iter().position(|x| x == &p(&format!("/{name}.mp3"))).unwrap();
            enqueue_track(&state, ui, playlist);
        };
        queue(&mut ui, &mut playlist, "d");
        queue(&mut ui, &mut playlist, "e");
        play_next(&state, &mut ui, &playlist);
        assert_eq!(playlist[ui.current], p("/d.mp3"), "the first queued track plays");
        assert_eq!(ui.enqueue_count, 1, "e is still queued");
        queue(&mut ui, &mut playlist, "f");
        let order: Vec<String> = playlist.iter().map(|x| x.file_stem().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(order, ["a", "d", "e", "f", "b", "c"]);
        play_next(&state, &mut ui, &playlist);
        assert_eq!(playlist[ui.current], p("/e.mp3"));
        assert_eq!(state.current_track.load(Ordering::Relaxed), ui.current);
    }

    #[test]
    fn a_track_queued_during_the_last_one_plays_after_it() {
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3")];
        ui.current = 2; // c, the last
        ui.cursor = 0;
        enqueue_track(&state, &mut ui, &mut playlist);
        assert_eq!(playlist, [p("/b.mp3"), p("/c.mp3"), p("/a.mp3")], "a goes AFTER c");
        assert_eq!(playlist[ui.current], p("/c.mp3"));
    }

    #[test]
    fn queueing_the_track_already_next_counts_it() {
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3")];
        ui.current = 0;
        ui.cursor = 1; // b is next anyway
        enqueue_track(&state, &mut ui, &mut playlist);
        assert_eq!(ui.enqueue_count, 1);
        ui.cursor = 2;
        enqueue_track(&state, &mut ui, &mut playlist);
        assert_eq!(playlist, [p("/a.mp3"), p("/b.mp3"), p("/c.mp3")]);
        assert_eq!(ui.enqueue_count, 2, "c queued behind b");
    }

    #[test]
    fn the_key_list_fits_the_window_and_lists_every_key() {
        let p = crate::theme::palette(crate::theme::ThemeKind::Minimal);
        for (w, h) in [(40, 10), (80, 24), (120, 40), (200, 60)] {
            let lines = help_layout(p, w, h).0;
            assert!(lines.len() < h, "{w}×{h}: {} rows", lines.len());
            assert!(lines.iter().all(|l| visible_len(l) <= w), "{w}×{h}: a line overflows");
        }
        // A 100-column window holds two columns.
        let two = help_layout(p, 100, 30).0;
        assert!(two.iter().any(|l| tui::plain(l).contains("PLAYLIST")), "{two:#?}");
        // Wide and tall enough: every key is there.
        let text = help_layout(p, 200, 60).0.iter().map(|l| tui::plain(l)).collect::<Vec<_>>().join("\n");
        for (_, keys) in crate::cli::KEYS {
            for (_, what) in *keys {
                assert!(text.contains(what), "missing {what:?}");
            }
        }
    }

    #[test]
    fn esc_in_the_player_quits_only_when_pressed_twice() {
        let now = std::time::Instant::now();
        assert!(!esc_confirms_quit(None, now), "a first Esc only asks");
        assert!(esc_confirms_quit(Some(now + ESC_QUIT_WINDOW), now), "the second within the window quits");
        assert!(!esc_confirms_quit(Some(now), now + Duration::from_millis(1)), "too late: it asks again");
    }

    /// Run `keys` through `poll_input` as if pressed (the click queue, which
    /// `poll_input` takes before the terminal's events).
    fn press(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>, keys: &[KeyCode]) {
        for &k in keys {
            ui.pressed.push_back(KeyEvent::new(k, KeyModifiers::NONE));
        }
        poll_input(state, ui, playlist);
    }

    #[test]
    fn a_removal_to_confirm_is_answered_by_the_next_key_whatever_it_is() {
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        let mut playlist = vec![p("/A/a.mp3"), p("/A/b.mp3")];
        ui.view_mode = ViewMode::Playlist;
        ui.library_tree_mode = true;
        ui.tree_pending_remove = Some(("artist A".into(), playlist.clone()));
        // Tab answers no; it does not switch panes under the prompt.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Tab]);
        assert!(ui.tree_pending_remove.is_none(), "the prompt is answered");
        assert!(ui.library_tree_mode, "the key was the answer, not a pane switch");
        // And a later `y` removes nothing.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y')]);
        assert_eq!(playlist.len(), 2);
    }

    #[test]
    fn the_key_list_and_the_eq_editor_never_return_to_each_other() {
        let state = PlayerState::new();
        let mut ui = test_ui(1);
        let mut playlist = vec![p("/a.mp3")];
        // ? then e: the EQ opens over the player, the list closes.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('?'), KeyCode::Char('e')]);
        assert_eq!(ui.view_mode, ViewMode::Eq);
        assert_eq!(ui.eq_return, ViewMode::Player);
        // ? over the EQ, then Esc twice: back to the EQ, then the player.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('?'), KeyCode::Esc]);
        assert_eq!(ui.view_mode, ViewMode::Eq);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Esc]);
        assert_eq!(ui.view_mode, ViewMode::Player);
        // ? over the EQ, then e: e closes the EQ under the list.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('e'), KeyCode::Char('?'), KeyCode::Char('e')]);
        assert_eq!(ui.view_mode, ViewMode::Player);
    }

    #[test]
    fn a_new_source_discards_a_rescan_of_the_old_one() {
        let dir = std::env::temp_dir().join(format!("keet-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.mp3"), b"").unwrap();
        let state = PlayerState::new();
        let mut ui = test_ui(1);
        let mut playlist = vec![p("/old/a.mp3")];
        let (_tx, rx) = std::sync::mpsc::channel();
        ui.rescan_receiver = Some(rx);
        ui.current = 0;
        ui.enqueue_count = 2; // a queue in the OLD list, playing its first track
        switch_source_paths(&state, &mut ui, &mut playlist, dir.clone());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ui.rescan_receiver.is_none(), "the old folders' result would replace the new list");
        assert_eq!(ui.enqueue_count, 0, "the old queue does not carry into the new list");
    }

    #[test]
    fn a_staged_removal_follows_its_tracks_through_a_resort() {
        let before = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3"), p("/d.mp3")];
        let staged = paths_at(&before, &[1, 2]); // b and c
        let after = vec![p("/d.mp3"), p("/c.mp3"), p("/a.mp3"), p("/b.mp3")]; // re-sorted meanwhile
        assert_eq!(indices_of(&after, &staged), [1, 3], "still b and c, not a and d");
        assert_eq!(indices_of(&after[..2], &staged), [1], "a track gone meanwhile is skipped");
    }

    #[test]
    fn the_queue_survives_the_next_track_starting() {
        assert_eq!(queue_after_advance(3, 4, 5), 2, "the first queued track started");
        assert_eq!(queue_after_advance(0, 4, 5), 0);
        assert_eq!(queue_after_advance(2, 4, 4), 2, "repeat-one: same track again");
        assert_eq!(queue_after_advance(2, 4, 9), 0, "a jump leaves the queue behind");
        assert_eq!(queue_after_advance(2, 4, 3), 0, "skip back");
    }

    #[test]
    fn too_small_is_below_either_minimum_and_unknown_is_never_small() {
        assert!(!window_too_small(40, 10));
        assert!(window_too_small(39, 40));
        assert!(window_too_small(120, 9));
        assert!(!window_too_small(0, 0), "a pty reporting 0×0 has an unknown size");
    }

    #[test]
    fn the_too_small_screen_fits_the_window_it_complains_about() {
        let p = crate::theme::palette(crate::theme::ThemeKind::Minimal);
        for (w, h) in [(39, 9), (20, 4), (8, 2), (1, 1), (39, 30)] {
            let lines = too_small_lines(w, h, false, "A very long song title indeed", (161.0, 372.0), p);
            assert!(lines.len() < h.max(2), "{w}×{h}: {} rows", lines.len());
            assert!(lines.iter().all(|l| visible_len(l) <= w), "{w}×{h}: {lines:?}");
        }
        let lines = too_small_lines(39, 9, true, "Undertow", (161.0, 372.0), p);
        let text: Vec<String> = lines.iter().map(|l| tui::plain(l).trim().to_string()).collect();
        assert!(text.contains(&"39×9 · Keet needs 40×10".to_string()), "{text:?}");
        assert!(text.contains(&"⏸ Undertow".to_string()), "{text:?}");
    }

    #[test]
    fn format_time_under_an_hour_keeps_mm_ss() {
        assert_eq!(format_time(59.0), "00:59");
        assert_eq!(format_time(125.4), "02:05");
    }

    #[test]
    fn format_time_above_an_hour_shows_h_mm_ss() {
        assert_eq!(format_time(3600.0), "1:00:00");
        assert_eq!(format_time(3725.0), "1:02:05");
        assert_eq!(format_time(2.0 * 3600.0 + 59.0 * 60.0 + 59.0), "2:59:59");
    }

    #[test]
    fn restore_order_keeps_saved_order_and_appends_new() {
        let p = |s: &str| PathBuf::from(s);
        let saved = vec![p("a"), p("b"), p("c")];
        // b removed, d added, rest shuffled
        let current = vec![p("c"), p("d"), p("a")];
        assert_eq!(restore_order(&saved, &current), vec![p("a"), p("c"), p("d")]);
    }

    #[test]
    fn only_folders_are_auto_sorted() {
        let p = |s: &str| PathBuf::from(s);
        let is_dir = |q: &std::path::Path| q.extension().is_none();
        assert!(sortable_with(&[p("/music/rock")], is_dir));
        assert!(sortable_with(&[p("/music/rock"), p("/music/jazz")], is_dir));
        // Files listed by hand, an M3U, or a mix: the order given stays.
        assert!(!sortable_with(&[p("/music/b.flac"), p("/music/a.flac")], is_dir));
        assert!(!sortable_with(&[p("/music/mix.m3u")], is_dir));
        assert!(!sortable_with(&[p("/music/rock"), p("/music/song.flac")], is_dir));
        assert!(!sortable_with(&[], is_dir));
    }

    #[test]
    fn auto_sort_should_run_only_when_pending_scanned_and_not_shuffling() {
        assert!(auto_sort_should_run(true, true, false)); // armed, scan done, not shuffling
        assert!(!auto_sort_should_run(false, true, false)); // not armed
        assert!(!auto_sort_should_run(true, false, false)); // scan not finished — tags not loaded
        assert!(!auto_sort_should_run(true, true, true)); // shuffling — leave the order alone
    }

    fn test_ui(playlist_len: usize) -> UiState {
        let cache = crate::metadata::MetadataCache::new(playlist_len);
        UiState::new(vec![PathBuf::from("/nonexistent-src")], cache)
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn ctrl_c_quits_from_text_input_instead_of_typing_c() {
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        ui.input_mode = InputMode::Search(TextInput::new());
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3")];

        let quit = handle_text_input(
            &state, &mut ui, &mut playlist,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(quit && state.should_quit(), "Ctrl+C in text input must quit");
        if let InputMode::Search(q) = &ui.input_mode {
            assert!(q.is_empty(), "Ctrl+C must not type into the query: {q:?}");
        }

        // A plain 'c' (no modifier) still types.
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        ui.input_mode = InputMode::Search(TextInput::new());
        handle_text_input(
            &state, &mut ui, &mut playlist,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );
        assert!(matches!(&ui.input_mode, InputMode::Search(q) if q.text() == "c"));
        assert!(!state.should_quit());
    }

    #[test]
    fn tree_scan_refresh_due_first_time_then_every_half_second() {
        use std::time::Duration;
        // Never refreshed → due immediately.
        assert!(tree_scan_refresh_due(None));
        // Refreshed recently → wait (rebuilding a large tree at 20 fps burned
        // CPU for the whole scan duration with no visible benefit).
        assert!(!tree_scan_refresh_due(Some(Duration::from_millis(100))));
        // Half a second on → due again.
        assert!(tree_scan_refresh_due(Some(Duration::from_millis(600))));
    }

    #[test]
    fn tree_rows_cache_refreshes_on_expand_and_filter() {
        let tag = |artist: &str, title: &str| crate::library::TrackTags {
            artist: Some(artist.into()),
            album: Some("Album".into()),
            disc: None,
            track: Some(1),
            title: title.into(),
        };
        let mut ui = test_ui(2);
        ui.library_tree = crate::library::build(&[tag("A", "t1"), tag("B", "t2")]);
        refresh_tree_rows(&mut ui);
        assert_eq!(ui.tree_rows.len(), 2, "two collapsed artist rows");

        // Expanding must refresh the cache — navigation reads it, not a rebuild.
        ui.tree_cursor = 0;
        tree_expand_under_cursor(&mut ui);
        assert_eq!(ui.tree_rows.len(), 3, "expand must refresh the cached rows");

        // Filter change must refresh too.
        ui.input_mode = InputMode::Search(TextInput::with("t2"));
        rebuild_tree_filter(&mut ui);
        assert_eq!(
            ui.tree_rows.len(),
            3, // artist B + album + track t2
            "filter must refresh the cached rows: {:?}",
            ui.tree_rows
        );
    }

    #[test]
    fn shuffle_off_with_mismatched_snapshot_falls_back_instead_of_panicking() {
        // A duplicate path surviving dedup (e.g. canonicalize failed on one of
        // two spellings) makes restore_order return FEWER entries than the live
        // playlist — clone_from_slice would panic on the length mismatch.
        let mut ui = test_ui(3);
        ui.shuffle = true;
        ui.pre_shuffle_order = Some(vec![p("/a.mp3")]);
        let mut playlist = vec![p("/b.mp3"), p("/a.mp3"), p("/a.mp3")];

        toggle_shuffle(&PlayerState::new(), &mut ui, &mut playlist);

        assert!(!ui.shuffle);
        assert_eq!(playlist.len(), 3, "fallback must keep every track");
        let mut sorted = playlist.clone();
        sorted.sort();
        assert_eq!(playlist, sorted, "mismatch falls back to the path sort");
    }

    #[test]
    fn tree_removal_of_the_playing_track_skips_it_like_the_flat_list_does() {
        // tree_remove_indices skipped the flat list's bookkeeping: the removed
        // track kept playing under another track's title and cover.
        let state = PlayerState::new();
        let mut ui = test_ui(4);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3"), p("/d.mp3")];
        ui.current = 1;
        tree_remove_indices(&state, &mut ui, &mut playlist, &[1, 2]);
        assert_eq!(playlist, vec![p("/a.mp3"), p("/d.mp3")]);
        assert_eq!(ui.playlist_gen, 1, "an edit: picks made before it are checked");
        assert!(state.take_skip_next(), "the removed playing track must be skipped");
        assert_eq!(ui.removed_current_next, Some(1), "next is /d.mp3, now at index 1");
        assert_eq!(state.total_tracks.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn removing_every_track_is_refused() {
        // Removing the only artist emptied the playlist, and the next frame
        // indexed playlist[ui.current] — a panic.
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3")];
        tree_remove_indices(&state, &mut ui, &mut playlist, &[0, 1]);
        assert_eq!(playlist.len(), 2);
        assert_eq!(ui.playlist_gen, 0, "nothing changed");
    }

    #[test]
    fn removing_the_last_track_while_it_plays_ends_the_list_instead_of_replaying_the_one_before() {
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3")];
        ui.current = 2;
        ui.cursor = 2;
        remove_track(&state, &mut ui, &mut playlist);
        assert_eq!(ui.removed_current_next, Some(2), "past the end");
        assert!(ui.current < playlist.len(), "the display index stays valid");
        assert_eq!(track_after_edit(ui.current, ui.removed_current_next, playlist.len(), RepeatMode::Off), 2);
    }

    #[test]
    fn the_full_window_line_shows_the_format_then_the_clock_then_the_way_out() {
        // The track info leads with the duration, which the clock right after
        // it repeated.
        let items = fullscreen_items("03:42 • 16bit stereo • 44100Hz", 62.0, 222.0);
        assert_eq!(items, ["16bit stereo • 44100Hz", "01:02/03:42", "{⇧F} exit"]);
        // Info without a duration part is kept whole.
        assert_eq!(fullscreen_items("44100Hz", 0.0, 0.0)[0], "44100Hz");
    }

    #[test]
    fn list_scroll_keeps_a_margin_and_never_overscrolls() {
        // 100 items, 20 rows: moving down past row 15 scrolls one at a time.
        assert_eq!(list_scroll(10, 0, 100, 20), 0);
        assert_eq!(list_scroll(16, 0, 100, 20), 1);
        // Moving up keeps 4 rows above the cursor.
        assert_eq!(list_scroll(30, 30, 100, 20), 26);
        // The end of the list: no empty rows below the last item.
        assert_eq!(list_scroll(99, 90, 100, 20), 80);
        // A list shorter than the window never scrolls.
        assert_eq!(list_scroll(5, 3, 8, 20), 0);
    }

    #[test]
    fn the_track_after_a_playlist_edit_follows_the_repeat_mode() {
        // After any edit the next track was always current + 1: repeat-one
        // moved on, and repeat-all on the last track replayed it. Past the end
        // means `len`, which the playlist loop turns into the repeat-all cycle
        // or the end of playback, exactly as a natural end does.
        use RepeatMode::*;
        assert_eq!(track_after_edit(3, None, 10, Off), 4);
        assert_eq!(track_after_edit(3, None, 10, One), 3);
        assert_eq!(track_after_edit(9, None, 10, All), 10);
        assert_eq!(track_after_edit(9, None, 10, Off), 10);
        // The playing track was removed: its successor plays (repeat-one has
        // nothing left to repeat).
        assert_eq!(track_after_edit(3, Some(3), 9, One), 3);
        assert_eq!(track_after_edit(8, Some(9), 9, All), 9);
    }

    #[test]
    fn queueing_keeps_the_header_on_the_playing_track_and_ignores_requeues() {
        let state = PlayerState::new();
        let mut ui = test_ui(5);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3"), p("/d.mp3"), p("/e.mp3")];
        ui.current = 3; // d plays
        state.current_track.store(3, Ordering::Relaxed);
        ui.cursor = 0;
        enqueue_track(&state, &mut ui, &mut playlist); // a moves behind d
        assert_eq!(playlist[ui.current], p("/d.mp3"));
        assert_eq!(state.current_track.load(Ordering::Relaxed), ui.current, "the header followed d");
        assert_eq!(ui.enqueue_count, 1);
        ui.cursor = ui.current + 1; // a again
        enqueue_track(&state, &mut ui, &mut playlist);
        assert_eq!(ui.enqueue_count, 1, "a re-queue is not a second queued track");
    }

    #[test]
    fn shuffle_off_keeps_the_header_on_the_playing_track() {
        let state = PlayerState::new();
        let mut ui = test_ui(4);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3"), p("/d.mp3")];
        ui.shuffle = true;
        ui.pre_shuffle_order = Some(playlist.clone());
        playlist.swap(0, 2); // c a b d → c plays at 0
        ui.current = 0;
        toggle_shuffle(&state, &mut ui, &mut playlist); // off: back to a b c d
        assert_eq!(playlist[ui.current], p("/c.mp3"));
        assert_eq!(state.current_track.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn the_queue_count_follows_removals_and_reorders() {
        let state = PlayerState::new();
        let mut ui = test_ui(5);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3"), p("/d.mp3"), p("/e.mp3")];
        ui.current = 0;
        ui.enqueue_count = 2; // b and c are queued
        ui.cursor = 1;
        remove_track(&state, &mut ui, &mut playlist); // remove queued b
        assert_eq!(ui.enqueue_count, 1, "one queued track left");
        toggle_shuffle(&state, &mut ui, &mut playlist);
        assert_eq!(ui.enqueue_count, 0, "a shuffle scatters the queue");
    }

    #[test]
    fn remove_track_restarts_scan_and_marks_tree_dirty() {
        // Flat-list remove must go through reindex_and_restart_scan: mutating
        // the cache positionally (remove_at) while the background scan is
        // running lets in-flight workers write tags into the wrong slots.
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3")];
        ui.tree_dirty = false;
        ui.cursor = 1; // not the playing track (current = 0)

        remove_track(&state, &mut ui, &mut playlist);

        assert_eq!(playlist, vec![p("/a.mp3"), p("/c.mp3")]);
        assert!(ui.tree_dirty, "remove must reindex via the scan-safe path");
        assert!(ui.scan_handle.is_some(), "scan must be restarted after reindex");
    }

    #[test]
    fn enqueue_track_restarts_scan_and_marks_tree_dirty() {
        // Same hazard as remove: move_entry during an active scan misplaces
        // in-flight tag writes.
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3"), p("/c.mp3")];
        ui.tree_dirty = false;
        ui.cursor = 2; // enqueue /c.mp3 to play right after current (index 0)

        enqueue_track(&state, &mut ui, &mut playlist);

        assert_eq!(playlist, vec![p("/a.mp3"), p("/c.mp3"), p("/b.mp3")]);
        assert!(ui.tree_dirty, "enqueue must reindex via the scan-safe path");
        assert!(ui.scan_handle.is_some(), "scan must be restarted after reindex");
    }

    #[test]
    fn ctrl_c_quits_from_save_playlist_and_tree_filter_input() {
        // SavePlaylist prompt.
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        ui.input_mode = InputMode::SavePlaylist(TextInput::new());
        let mut playlist = vec![p("/a.mp3"), p("/b.mp3")];
        let quit = handle_text_input(
            &state, &mut ui, &mut playlist,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(quit && state.should_quit());

        // Tree-filter search (`/` in the tree view) routes through tree_search_input.
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        ui.library_tree_mode = true;
        ui.input_mode = InputMode::Search(TextInput::new());
        let quit = handle_text_input(
            &state, &mut ui, &mut playlist,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );
        assert!(quit && state.should_quit());
        assert!(ui.tree_filter.is_empty(), "Ctrl+C must not type into the tree filter");
    }

    #[test]
    fn eq_editor_fits_every_window_it_is_drawn_in() {
        // The editor printed its 99-column footer and every body line raw and
        // budgeted its height ignoring the rows above it: narrower than 99
        // columns it wrapped (frame drifts a row per frame, then scrolls),
        // and in a default-size window it scrolled the screen.
        let st = PlayerState::new();
        let bands = st.eq_bands_array();
        let p = crate::theme::palette(crate::theme::ThemeKind::Classic);
        let readouts = [("FX", "None"), ("XFEED", "Off"), ("BAL", "centred"), ("RG", "off")];
        let footer = "  [←→] band  [↑↓] gain (⇧ fine)  [t] type  [,.] Q  [<>] freq  [[]] preset  [0] reset  [e/Esc] close";
        for term_w in 10usize..130 {
            for avail in 0usize..45 {
                let body = crate::eq_ui::render_eq_screen(
                    &bands, 0, "Flat", &readouts, '█', p, term_w, avail.saturating_sub(1),
                );
                let lines = fit_eq_screen(body, Line::raw(footer), term_w, avail);
                assert!(lines.len() <= avail, "{} rows in a {avail}-row budget at width {term_w}", lines.len());
                for l in &lines {
                    assert!(visible_len(l) <= term_w, "{}-col line at width {term_w}: {l:?}", visible_len(l));
                }
            }
        }
    }

    #[test]
    fn esc_tap_survives_its_own_release_event() {
        use crossterm::event::{KeyEventState, KeyModifiers};
        let key = |code, kind| Event::Key(KeyEvent { code, modifiers: KeyModifiers::NONE, kind, state: KeyEventState::NONE });
        // Windows: Press + Release of the same Esc — a real tap.
        assert!(!esc_starts_sequence(&key(KeyCode::Esc, KeyEventKind::Release)));
        // macOS Cmd+Arrow: ESC then another key press — a sequence.
        assert!(esc_starts_sequence(&key(KeyCode::Char('b'), KeyEventKind::Press)));
        // A resize behind the Esc is not a sequence either.
        assert!(!esc_starts_sequence(&Event::Resize(80, 24)));
    }

    #[test]
    fn a_sort_during_search_refilters_instead_of_leaving_stale_hits() {
        // The one-shot artist->album auto-sort can fire mid-search. It reordered
        // the playlist but kept `filtered_indices` (and set the cursor to a
        // playlist index), so the results showed other tracks and Enter played
        // the wrong one.
        let mut playlist: Vec<PathBuf> =
            ["c-song.flac", "a-song.flac", "b-other.flac"].iter().map(PathBuf::from).collect();
        let cache = crate::metadata::MetadataCache::new(playlist.len());
        let mut ui = UiState::new(Vec::new(), cache);
        let st = PlayerState::new();
        ui.input_mode = InputMode::Search(TextInput::with("song"));
        rebuild_filter(&mut ui, &playlist);
        assert_eq!(ui.filtered_indices, vec![0, 1]);
        sort_playlist_by_tags(&st, &mut ui, &mut playlist);
        let hits: Vec<&str> = ui
            .filtered_indices
            .iter()
            .map(|&i| playlist[i].to_str().unwrap())
            .collect();
        assert_eq!(hits, vec!["a-song.flac", "c-song.flac"], "results must follow the new order");
        assert!(ui.cursor < ui.filtered_indices.len(), "cursor is a list position, not a playlist index");
    }

    #[test]
    fn rescan_that_removes_tracks_keeps_the_cursor_on_the_list() {
        // Cursor at the end of a list, files deleted on disk, `r`: the cursor
        // was left past the end, and Enter then jumped to a missing index —
        // the playlist loop saw current >= len and (repeat off) Keet exited.
        let dir = std::env::temp_dir().join(format!("keet_rescan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5 {
            std::fs::write(dir.join(format!("t{i}.flac")), b"").unwrap();
        }
        let mut playlist = crate::playlist::build_playlist(&dir, false).unwrap();
        assert_eq!(playlist.len(), 5);
        let cache = crate::metadata::MetadataCache::new(playlist.len());
        let mut ui = UiState::new(vec![dir.clone()], cache);
        let st = PlayerState::new();
        ui.current = 0;
        ui.cursor = 4;
        ui.scroll_offset = 3;
        for i in 2..5 {
            std::fs::remove_file(dir.join(format!("t{i}.flac"))).unwrap();
        }
        let result = crate::playlist::scan_sources(&ui.source_paths.clone(), &playlist);
        finish_rescan(&st, &mut ui, &mut playlist, &result);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(playlist.len(), 2);
        assert!(ui.cursor < playlist.len(), "cursor {} on a {}-track list", ui.cursor, playlist.len());
        assert!(ui.scroll_offset <= ui.cursor);
    }

    /// Every theme's every view, every viz mode in and out of full window,
    /// drawn into windows from 1×1 up: a layout that computes a rectangle
    /// past the window's edge panics in ratatui's buffer, so this walks the
    /// sizes a drag passes through.
    #[test]
    fn every_view_draws_in_every_window_size() {
        use crate::theme::ThemeKind;
        let state = PlayerState::new();
        state.total_samples.store(48_000 * 200, Ordering::Relaxed);
        state.sample_rate.store(48_000, Ordering::Relaxed);
        let playlist: Vec<PathBuf> = (0..30).map(|i| PathBuf::from(format!("/m/{i:02} a track.flac"))).collect();
        let cache = crate::metadata::MetadataCache::new(playlist.len());
        let mut ui = UiState::new(playlist.clone(), cache);
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics("[00:01.00]one\n[00:02.00]two\n[00:03.00]three")));
        let eq = crate::eq::builtin_presets().remove(0);
        let mut stats = StatsMonitor::new();
        let analyser = VizAnalyser::new(48_000);
        let sizes = [(1, 1), (20, 5), (39, 9), (40, 10), (60, 16), (80, 24), (119, 30), (200, 60)];
        let modes = (0..11u8).map(VizMode::from_u8).collect::<Vec<_>>();
        let mut draw = |ui: &mut UiState, w: u16, h: u16, what: &str| {
            ui.too_small = window_too_small(w as usize, h as usize);
            let buf = tui::testing::draw(w, h, |f| {
                let mut cx = Ctx {
                    state: &state,
                    ui: &mut *ui,
                    name: "a track",
                    track_info: "03:20 • 24bit stereo • 48000Hz",
                    ext: "flac",
                    eq_preset: &eq,
                    fx_name: "None",
                    cf_name: "Off",
                    stats: &mut stats,
                    playlist: &playlist,
                    analyser: &analyser,
                    repaint: true,
                };
                draw(f, &mut cx);
            });
            assert_eq!(tui::testing::rows(&buf).len(), h as usize, "{what} {w}×{h}");
        };
        for kind in [ThemeKind::Classic, ThemeKind::Minimal, ThemeKind::HiFi] {
            state.set_theme(kind);
            for &(w, h) in &sizes {
                ui.view_mode = ViewMode::Player;
                for &mode in &modes {
                    state.viz_mode.store(mode as u8, Ordering::Relaxed);
                    for full in [false, true] {
                        state.viz_fullscreen.store(full, Ordering::Relaxed);
                        draw(&mut ui, w, h, &format!("{kind:?} {mode:?} full={full}"));
                    }
                }
                state.viz_fullscreen.store(false, Ordering::Relaxed);
                for view in [ViewMode::Playlist, ViewMode::Lyrics, ViewMode::Eq, ViewMode::Help] {
                    ui.view_mode = view;
                    draw(&mut ui, w, h, &format!("{kind:?} {view:?}"));
                }
                ui.view_mode = ViewMode::Player;
                ui.esc_quit_until = Some(std::time::Instant::now() + ESC_QUIT_WINDOW);
                ui.tree_pending_remove = Some(("artist Somebody With A Long Name".into(), vec![PathBuf::from("/m/x")]));
                draw(&mut ui, w, h, &format!("{kind:?} confirmations"));
                ui.esc_quit_until = None;
                ui.tree_pending_remove = None;
                ui.input_mode = InputMode::OpenPath(TextInput::with("/a/path/that/goes/on/and/on/日本語"));
                draw(&mut ui, w, h, &format!("{kind:?} open dialog"));
                ui.view_mode = ViewMode::Playlist;
                ui.input_mode = InputMode::Search(TextInput::with("track"));
                draw(&mut ui, w, h, &format!("{kind:?} search"));
                ui.input_mode = InputMode::Normal;
                ui.docked = vec![ViewMode::Playlist, ViewMode::Lyrics];
                for view in [ViewMode::Player, ViewMode::Playlist, ViewMode::Lyrics] {
                    ui.view_mode = view;
                    draw(&mut ui, w, h, &format!("{kind:?} docked, {view:?} has the keys"));
                }
                ui.docked.clear();
                ui.view_mode = ViewMode::Playlist;
                ui.library_tree_mode = true;
                rebuild_library_tree(&mut ui, &playlist);
                draw(&mut ui, w, h, &format!("{kind:?} tree"));
                ui.library_tree_mode = false;
            }
        }
    }

    /// What a frame says, read back off the screen: the key bar and the
    /// track's title in each theme's player view.
    #[test]
    fn the_player_view_reads_right_in_every_theme() {
        use crate::theme::ThemeKind;
        let state = PlayerState::new();
        let playlist = vec![PathBuf::from("/m/Undertow.flac")];
        let mut ui = UiState::new(playlist.clone(), crate::metadata::MetadataCache::new(1));
        let eq = crate::eq::builtin_presets().remove(0);
        let mut stats = StatsMonitor::new();
        let analyser = VizAnalyser::new(48_000);
        for (kind, expect) in [
            (ThemeKind::Classic, ["Undertow", "pause", "clip ○"]),
            (ThemeKind::Minimal, ["K  E  E  T", "Undertow", "NOW PLAYING"]),
            (ThemeKind::HiFi, ["K E E T", "UNDERTOW", "VU METER"]),
        ] {
            state.set_theme(kind);
            let buf = tui::testing::draw(120, 40, |f| {
                let mut cx = Ctx {
                    state: &state,
                    ui: &mut ui,
                    name: "Undertow",
                    track_info: "",
                    ext: "flac",
                    eq_preset: &eq,
                    fx_name: "None",
                    cf_name: "Off",
                    stats: &mut stats,
                    playlist: &playlist,
                    analyser: &analyser,
                    repaint: true,
                };
                draw(f, &mut cx);
            });
            let text = tui::testing::rows(&buf).join("\n");
            for e in expect {
                assert!(text.contains(e), "{kind:?}: no {e:?} in\n{text}");
            }
        }
    }


    /// Minimal's tabs: each click target sits on its title, and the lit one
    /// is the view with the keys (a floating panel's, not the player's).
    #[test]
    fn minimal_tabs_are_hit_where_drawn_and_light_the_view_with_the_keys() {
        let state = PlayerState::new();
        state.set_theme(crate::theme::ThemeKind::Minimal);
        let playlist = vec![PathBuf::from("/m/a.flac")];
        let mut ui = UiState::new(playlist.clone(), crate::metadata::MetadataCache::new(1));
        ui.view_mode = ViewMode::Lyrics;
        let eq = crate::eq::builtin_presets().remove(0);
        let mut stats = StatsMonitor::new();
        let analyser = VizAnalyser::new(48_000);
        let buf = tui::testing::draw(160, 40, |f| {
            let mut cx = Ctx {
                state: &state,
                ui: &mut ui,
                name: "a",
                track_info: "",
                ext: "flac",
                eq_preset: &eq,
                fx_name: "None",
                cf_name: "Off",
                stats: &mut stats,
                playlist: &playlist,
                analyser: &analyser,
                repaint: true,
            };
            draw(f, &mut cx);
        });
        let tabs: Vec<(Rect, ViewMode)> =
            ui.hits.iter().filter_map(|&(r, h)| if let Hit::Tab(v) = h { Some((r, v)) } else { None }).collect();
        assert_eq!(tabs.len(), 3);
        let accent = crate::theme::palette(crate::theme::ThemeKind::Minimal).accent.fg;
        for (r, view) in tabs {
            let text: String = (r.x..r.right()).map(|x| buf[(x, r.y)].symbol().to_string()).collect();
            let want = match view {
                ViewMode::Player => "player",
                ViewMode::Playlist => "library",
                _ => "lyrics",
            };
            assert_eq!(text, want, "the {view:?} tab's hit is off its title");
            let lit = buf[(r.x, r.y)].fg == accent.unwrap();
            assert_eq!(lit, view == ViewMode::Lyrics, "{view:?} lit: {lit}");
        }
    }

    #[test]
    fn a_paste_fills_the_open_field_or_opens_the_path_dialog() {
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        paste(&mut ui, &[], " /Music/Kind of Blue\n");
        assert!(matches!(&ui.input_mode, InputMode::OpenPath(t) if t.text() == "/Music/Kind of Blue"));
        assert!(overlay_open(&ui), "a dialog is open: the frame loop repaints, images go");
        // The save field, in the playlist (where `s` opens it), has the keys.
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::SavePlaylist(TextInput::with("mix"));
        paste(&mut ui, &[], " 2");
        assert!(matches!(&ui.input_mode, InputMode::SavePlaylist(t) if t.text() == "mix 2"));
    }

    #[test]
    fn the_open_dialog_floats_over_the_player_and_takes_the_cursor() {
        let state = PlayerState::new();
        let playlist = vec![PathBuf::from("/m/a.flac")];
        let mut ui = UiState::new(playlist.clone(), crate::metadata::MetadataCache::new(1));
        ui.input_mode = InputMode::OpenPath(TextInput::with("~/Music"));
        let eq = crate::eq::builtin_presets().remove(0);
        let mut stats = StatsMonitor::new();
        let analyser = VizAnalyser::new(48_000);
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        t.draw(|f| {
            let mut cx = Ctx {
                state: &state, ui: &mut ui, name: "a", track_info: "", ext: "flac", eq_preset: &eq,
                fx_name: "None", cf_name: "Off", stats: &mut stats, playlist: &playlist,
                analyser: &analyser, repaint: true,
            };
            draw(f, &mut cx);
        })
        .unwrap();
        tui::discard_images();
        let text = tui::testing::rows(t.backend().buffer()).join("\n");
        assert!(text.contains("Open") && text.contains("› ~/Music"), "{text}");
        // The cursor sits after the typed path, inside the dialog.
        let pos = t.get_cursor_position().unwrap();
        let row = &tui::testing::rows(t.backend().buffer())[pos.y as usize];
        assert!(row.contains("› ~/Music"), "cursor on row {}: {row}", pos.y);
    }


    #[test]
    fn every_dialog_counts_as_an_overlay() {
        let mut ui = UiState::new(Vec::new(), crate::metadata::MetadataCache::new(0));
        assert!(!overlay_open(&ui));
        ui.view_mode = ViewMode::Eq;
        assert!(overlay_open(&ui), "the EQ editor");
        toggle_help(&mut ui);
        assert!(overlay_open(&ui), "the key list");
        toggle_help(&mut ui);
        assert_eq!(ui.view_mode, ViewMode::Eq, "the key list closes back to the EQ editor");
        ui.view_mode = ViewMode::Playlist;
        assert!(overlay_open(&ui), "the playlist floats");
        ui.view_mode = ViewMode::Lyrics;
        assert!(overlay_open(&ui), "the lyrics float");
        ui.view_mode = ViewMode::Player;
        ui.tree_pending_remove = Some(("album AM".into(), vec![PathBuf::from("/a")]));
        assert!(overlay_open(&ui), "a removal to confirm");
        ui.tree_pending_remove = None;
        ui.esc_quit_until = Some(std::time::Instant::now() + ESC_QUIT_WINDOW);
        assert!(overlay_open(&ui), "Quit?");
        ui.esc_quit_until = Some(std::time::Instant::now() - Duration::from_millis(1));
        assert!(!overlay_open(&ui), "Quit? goes when its window ends");
    }


    /// Draw one frame of `ui` in a `w`×`h` window; the buffer.
    fn frame(state: &PlayerState, ui: &mut UiState, playlist: &[PathBuf], w: u16, h: u16) -> ratatui::buffer::Buffer {
        let eq = crate::eq::builtin_presets().remove(0);
        let mut stats = StatsMonitor::new();
        let analyser = VizAnalyser::new(48_000);
        tui::testing::draw(w, h, |f| {
            let mut cx = Ctx {
                state,
                ui,
                name: "a",
                track_info: "",
                ext: "flac",
                eq_preset: &eq,
                fx_name: "None",
                cf_name: "Off",
                stats: &mut stats,
                playlist,
                analyser: &analyser,
                repaint: true,
            };
            draw(f, &mut cx);
        })
    }

    #[test]
    fn a_narrow_docked_panel_wraps_its_keys_and_its_lyrics() {
        let state = PlayerState::new();
        let playlist = vec![p("/m/a.flac")];
        let long = "this lyric line is far too long for a narrow panel to hold";
        for kind in [crate::theme::ThemeKind::Classic, crate::theme::ThemeKind::Minimal] {
            state.set_theme(kind);
            let mut ui = UiState::new(playlist.clone(), crate::metadata::MetadataCache::new(1));
            ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&format!("{long}\nshort"))));
            ui.docked = vec![ViewMode::Lyrics];
            ui.view_mode = ViewMode::Lyrics;
            let buf = frame(&state, &mut ui, &playlist, 50, 40);
            assert_eq!(ui.docked_shown, vec![ViewMode::Lyrics], "{kind:?}");
            let text = tui::testing::rows(&buf).join("\n");
            // Every key is on screen, over two rows or more.
            for key in ["scroll", "float", "close", "player"] {
                assert!(text.contains(key), "{kind:?}: no {key:?} in\n{text}");
            }
            // Every word of the long line, none cut.
            for word in long.split(' ') {
                assert!(text.contains(word), "{kind:?}: {word:?} cut from\n{text}");
            }
        }
    }

    #[test]
    fn the_viz_counts_as_on_screen_only_where_a_frame_showed_it() {
        let state = PlayerState::new();
        state.viz_mode.store(VizMode::SpectrumVertical as u8, Ordering::Relaxed);
        let playlist = vec![p("/m/a.flac")];
        for kind in [crate::theme::ThemeKind::Classic, crate::theme::ThemeKind::Minimal, crate::theme::ThemeKind::HiFi] {
            state.set_theme(kind);
            let mut ui = UiState::new(playlist.clone(), crate::metadata::MetadataCache::new(1));
            frame(&state, &mut ui, &playlist, 120, 50);
            assert!(ui.viz_on_screen(), "{kind:?}: the player shows it");
            // The EQ editor floats over part of it: in a wide window most of
            // it stays in view and it keeps running.
            ui.eq_return = ViewMode::Player;
            ui.view_mode = ViewMode::Eq;
            frame(&state, &mut ui, &playlist, 220, 60);
            assert!(ui.viz_on_screen(), "{kind:?}: the viz runs under the EQ editor");
            // The floating playlist covers the whole of it.
            ui.view_mode = ViewMode::Playlist;
            frame(&state, &mut ui, &playlist, 120, 50);
            assert!(!ui.viz_on_screen(), "{kind:?}: hidden under the playlist");
            // Docked in a window with no room for both: the panel takes the
            // slot, and the viz is not drawn at all.
            if kind != crate::theme::ThemeKind::HiFi {
                ui.docked = vec![ViewMode::Playlist];
                frame(&state, &mut ui, &playlist, 70, 40);
                assert_eq!(ui.docked_shown, vec![ViewMode::Playlist], "{kind:?}");
                assert!(!ui.viz_on_screen(), "{kind:?}: the docked panel took its slot");
            }
        }
    }

    fn click(kind: event::MouseEventKind, column: u16, row: u16) -> event::MouseEvent {
        event::MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    #[test]
    fn config_docks_each_panel_once_and_names_what_is_not_one() {
        let names: Vec<String> = ["playlist", "Library", "lyrics", "eq"].map(String::from).to_vec();
        let (docked, problems) = docked_from_config(&names);
        assert_eq!(docked, vec![ViewMode::Playlist, ViewMode::Lyrics]);
        assert_eq!(problems, vec!["docked (\"eq\" is not a panel)".to_string()]);
    }

    #[test]
    fn a_docked_playlist_keeps_its_tree_pane_built_without_the_keys() {
        let playlist = vec![p("/m/A/x.flac"), p("/m/B/y.flac")];
        let mut ui = test_ui(2);
        ui.view_mode = ViewMode::Player;
        poll_library_tree(&mut ui, &playlist);
        assert!(ui.library_tree.artists.is_empty(), "nothing shows the tree: not built");
        ui.docked_shown = vec![ViewMode::Playlist];
        poll_library_tree(&mut ui, &playlist);
        assert!(!ui.library_tree.artists.is_empty(), "the docked panel's tree pane shows");
    }

    #[test]
    fn a_closed_playlist_does_not_rebuild_its_tree_even_if_the_tree_had_the_keys() {
        let playlist = vec![p("/m/A/x.flac"), p("/m/B/y.flac")];
        let mut ui = test_ui(2);
        ui.view_mode = ViewMode::Player;
        ui.library_tree_mode = true; // the tree pane had the keys when it closed
        poll_library_tree(&mut ui, &playlist);
        assert!(ui.library_tree.artists.is_empty(), "no tree on screen: not built");
        assert!(ui.tree_dirty, "still owed for when it shows");
        ui.view_mode = ViewMode::Playlist;
        poll_library_tree(&mut ui, &playlist);
        assert!(!ui.library_tree.artists.is_empty(), "opened: built");
    }

    #[test]
    fn a_tree_rebuilt_on_screen_is_drawn_at_once() {
        let playlist = vec![p("/m/A/x.flac"), p("/m/B/y.flac")];
        let mut ui = test_ui(2);
        ui.docked_shown = vec![ViewMode::Playlist];
        ui.dirty = false;
        poll_library_tree(&mut ui, &playlist);
        assert!(ui.dirty, "built: a frame now, not at the idle rate");
        ui.dirty = false;
        poll_library_tree(&mut ui, &playlist);
        assert!(!ui.dirty, "nothing rebuilt: no frame asked for");
    }

    #[test]
    fn closing_the_floating_playlist_by_a_click_closes_its_search_too() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac"), p("/b.flac")];
        let mut ui = test_ui(2);
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::with("beat"));
        ui.filtered_indices = vec![1];
        ui.hits.push((Rect::new(10, 5, 60, 20), Hit::Dialog));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 2, 2));
        assert_eq!(ui.view_mode, ViewMode::Player);
        assert!(matches!(ui.input_mode, InputMode::Normal), "keys would go on typing into it");
        assert!(ui.filtered_indices.is_empty());
    }

    #[test]
    fn a_list_click_then_a_tree_click_on_the_same_row_is_not_a_double_click() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..20).map(|i| p(&format!("/A/{i}.flac"))).collect();
        let mut ui = test_ui(20);
        ui.view_mode = ViewMode::Playlist;
        poll_library_tree(&mut ui, &playlist);
        // Both panes: the list on the left, the tree on the right, same rows.
        ui.hits.push((Rect::new(0, 0, 200, 40), Hit::Dialog));
        ui.hits.push((Rect::new(2, 4, 80, 20), Hit::List { first: 0 }));
        ui.hits.push((Rect::new(90, 4, 60, 20), Hit::Tree { first: 0 }));
        let folded = ui.tree_rows.len();
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 10, 4));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 100, 4));
        assert_eq!(ui.tree_rows.len(), folded, "the artist must not fold or unfold on a single click");
        assert_eq!(state.take_jump(), None);
    }

    #[test]
    fn a_clicked_key_hint_acts_instead_of_typing_into_a_field() {
        let state = PlayerState::new();
        let mut ui = test_ui(2);
        let mut playlist = vec![p("/a.flac"), p("/b.flac")];
        ui.docked = vec![ViewMode::Playlist];
        ui.docked_shown = vec![ViewMode::Playlist];
        // Typing in the docked playlist's search, then the player's ␣ hint
        // clicked: the field closes and the click pauses, as ␣ does.
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::with("be"));
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        poll_input(&state, &mut ui, &mut playlist);
        assert!(matches!(ui.input_mode, InputMode::Normal), "the field closed");
        assert!(state.is_paused(), "the click paused, as ␣ does");
        // A search left open while the player has the keys: the click
        // pauses again, and the search stays for the way back.
        ui.view_mode = ViewMode::Player;
        ui.input_mode = InputMode::Search(TextInput::with("be"));
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        poll_input(&state, &mut ui, &mut playlist);
        assert!(!state.is_paused(), "␣ resumed");
        assert!(matches!(&ui.input_mode, InputMode::Search(t) if t.text() == "be"));
    }

    #[test]
    fn the_wheel_scrolls_what_is_under_the_pointer() {
        use event::MouseEventKind as K;
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..40).map(|i| p(&format!("/A{}/{i}.flac", i % 5))).collect();
        let mut ui = test_ui(40);
        ui.view_mode = ViewMode::Playlist;
        poll_library_tree(&mut ui, &playlist);
        // Untagged: one artist, one album. Unfold both so the tree has rows.
        tree_expand_under_cursor(&mut ui);
        tree_move(&mut ui, 1);
        tree_expand_under_cursor(&mut ui);
        ui.tree_cursor = 0;
        assert!(ui.tree_rows.len() > WHEEL_ROWS, "{} rows", ui.tree_rows.len());
        // Both panes floating, the list has the keys: the wheel over the
        // tree moves the tree.
        ui.hits.push((Rect::new(0, 0, 200, 40), Hit::Dialog));
        ui.hits.push((Rect::new(2, 4, 80, 20), Hit::List { first: 0 }));
        ui.hits.push((Rect::new(90, 4, 60, 20), Hit::Tree { first: 0 }));
        mouse(&state, &mut ui, &playlist, click(K::ScrollDown, 100, 6));
        assert_eq!(ui.cursor, 0, "the list stays");
        assert_eq!(ui.tree_cursor, WHEEL_ROWS, "the tree moved");
        // Docked lyrics while the player has the keys: the wheel over them
        // scrolls them.
        ui.hits.clear();
        ui.view_mode = ViewMode::Player;
        ui.hits.push((Rect::new(100, 20, 60, 15), Hit::Panel(ViewMode::Lyrics)));
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&"line\n".repeat(200))));
        mouse(&state, &mut ui, &playlist, click(K::ScrollDown, 110, 25));
        assert_eq!(ui.lyrics_step, WHEEL_ROWS as isize, "the lyrics took the wheel");
        lyrics_panel(&state, &mut ui, Rect::new(0, 0, 60, 15));
        assert_eq!(ui.lyrics_scroll, WHEEL_ROWS);
    }

    #[test]
    fn shift_k_closes_a_docked_panel_and_its_key_only_hands_the_keys_back() {
        let state = PlayerState::new();
        let mut ui = test_ui(1);
        let mut playlist = vec![p("/a.flac")];
        ui.docked = vec![ViewMode::Lyrics, ViewMode::Playlist];
        ui.docked_shown = ui.docked.clone();
        // y focuses the docked lyrics, l moves straight to the playlist.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y')]);
        assert_eq!(ui.view_mode, ViewMode::Lyrics);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('l')]);
        assert_eq!(ui.view_mode, ViewMode::Playlist);
        // A second l (a double tap) hands the keys back; nothing closes.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('l')]);
        assert_eq!(ui.view_mode, ViewMode::Player);
        assert_eq!(ui.docked.len(), 2);
        // Shift+K with the playlist focused closes it: out of the dock.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('l'), KeyCode::Char('K')]);
        assert_eq!(ui.view_mode, ViewMode::Player);
        assert_eq!(ui.docked, vec![ViewMode::Lyrics]);
        // With the player focused it closes nothing.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('K')]);
        assert_eq!(ui.docked, vec![ViewMode::Lyrics]);
    }

    #[test]
    fn a_hint_in_a_docked_panel_acts_on_that_panel() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.docked = vec![ViewMode::Lyrics];
        ui.docked_shown = vec![ViewMode::Lyrics];
        ui.hits.push((Rect::new(80, 10, 60, 20), Hit::Panel(ViewMode::Lyrics)));
        ui.hits.push((Rect::new(90, 28, 2, 1), Hit::Key(KeyCode::Char('K'), KeyModifiers::SHIFT)));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 90, 28));
        assert_eq!(ui.view_mode, ViewMode::Lyrics, "the panel took the keys first");
        assert_eq!(ui.pressed.pop_front().map(|k| k.code), Some(KeyCode::Char('K')));
    }

    #[test]
    fn docked_and_floating_panels_offer_their_own_way_in_and_out() {
        let mut ui = test_ui(1);
        let keys = |ui: &UiState, v| panel_footer_keys(ui, v, false).into_iter().map(|(k, ..)| k).collect::<Vec<_>>();
        assert_eq!(keys(&ui, ViewMode::Lyrics), ["k", "y"]);
        ui.docked_shown = vec![ViewMode::Lyrics];
        ui.focus = ViewMode::Player;
        assert_eq!(keys(&ui, ViewMode::Lyrics), ["y", "k", "⇧k"], "without the keys: y focuses");
        ui.focus = ViewMode::Lyrics;
        assert_eq!(keys(&ui, ViewMode::Lyrics), ["k", "⇧k", "Esc"]);
        assert_eq!(keys(&ui, ViewMode::Playlist), ["k", "l"]);
    }

    #[test]
    fn clicking_a_docked_panels_focus_hint_focuses_it_and_nothing_else() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.docked = vec![ViewMode::Lyrics];
        ui.docked_shown = vec![ViewMode::Lyrics];
        ui.lyrics_auto_scroll = false;
        ui.lyrics_scroll = 7;
        ui.focus = ViewMode::Player;
        ui.hits.push((Rect::new(80, 10, 60, 20), Hit::Panel(ViewMode::Lyrics)));
        let keys = panel_footer_keys(&ui, ViewMode::Lyrics, false);
        let look = key_look(crate::theme::ThemeKind::Classic, crate::theme::palette(crate::theme::ThemeKind::Classic));
        crate::hints::bar_lines(&keys, &look, 40)[0].register(&mut ui.hits, 82, 28);
        assert!(ui.hits.iter().any(|(_, h)| *h == Hit::Key(KeyCode::Char('k'), KeyModifiers::NONE)), "other hints stay keys");
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 82, 28));
        assert_eq!(ui.view_mode, ViewMode::Lyrics);
        assert!(ui.pressed.is_empty(), "y now would hand the keys straight back");
        assert!(!ui.lyrics_auto_scroll, "focused as it was left: the verse the user scrolled to stays");
        assert_eq!(ui.lyrics_scroll, 7);
    }

    #[test]
    fn focusing_a_docked_panel_keeps_it_as_it_was_and_opening_one_sets_it_up() {
        let state = PlayerState::new();
        let mut ui = test_ui(5);
        let mut playlist: Vec<PathBuf> = (0..5).map(|i| p(&format!("/{i}.flac"))).collect();
        ui.current = 3;
        ui.cursor = 1;
        ui.lyrics_scroll = 9;
        ui.lyrics_auto_scroll = false;
        // Docked: y and l only take the keys.
        ui.docked = vec![ViewMode::Lyrics, ViewMode::Playlist];
        ui.docked_shown = ui.docked.clone();
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y')]);
        assert_eq!((ui.view_mode, ui.lyrics_scroll, ui.lyrics_auto_scroll), (ViewMode::Lyrics, 9, false));
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('l')]);
        assert_eq!((ui.view_mode, ui.cursor), (ViewMode::Playlist, 1));
        // Not on screen: opening sets it up.
        ui.view_mode = ViewMode::Player;
        ui.docked.clear();
        ui.docked_shown.clear();
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y')]);
        assert_eq!((ui.lyrics_scroll, ui.lyrics_auto_scroll), (0, true));
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('l')]);
        assert_eq!(ui.cursor, 3, "the cursor on the playing track");
    }

    #[test]
    fn new_lyrics_open_at_the_top_whatever_was_scrolled_before() {
        let state = PlayerState::new();
        let mut ui = test_ui(1);
        let mut playlist = vec![p("/a.flac")];
        ui.view_mode = ViewMode::Lyrics;
        // Song A's lyrics were long; the next song's are still loading.
        ui.set_lyrics(None);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s'), KeyCode::Char('s')]);
        assert_eq!((ui.lyrics_scroll, ui.lyrics_step), (0, 0), "nothing to scroll while loading");
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics("[00:01.00]one\n[00:02.00]two")));
        assert_eq!((ui.lyrics_scroll, ui.lyrics_auto_scroll), (0, true));
        // A scroll the next frame has not applied yet goes with the lyrics
        // it was meant for.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s')]);
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics("[00:01.00]three\n[00:02.00]four")));
        assert_eq!((ui.lyrics_step, ui.lyrics_auto_scroll), (0, true));
    }

    /// The lyrics panel alone, drawn into `area` of a 100×40 window.
    fn lyrics_panel(state: &PlayerState, ui: &mut UiState, area: Rect) -> Vec<String> {
        let eq = crate::eq::builtin_presets().remove(0);
        let mut stats = StatsMonitor::new();
        let analyser = VizAnalyser::new(48_000);
        let playlist = vec![p("/m/a.flac")];
        let buf = tui::testing::draw(100, 40, |f| {
            let mut cx = Ctx {
                state,
                ui,
                name: "a",
                track_info: "",
                ext: "flac",
                eq_preset: &eq,
                fx_name: "None",
                cf_name: "Off",
                stats: &mut stats,
                playlist: &playlist,
                analyser: &analyser,
                repaint: true,
            };
            crate::lyrics_panel::draw(f, &mut cx, area);
        });
        tui::testing::rows(&buf)
    }

    #[test]
    fn lyrics_rows_line_up_and_following_the_song_centres_the_lines_first_row() {
        let state = PlayerState::new();
        state.output_rate.store(48_000, Ordering::Relaxed);
        state.samples_played.store(48_000 * 10 + 24_000, Ordering::Relaxed); // 10.5 s: line 10
        let mut ui = test_ui(1);
        // LRCLIB's form, a space after the timestamp; every line wraps in two.
        let words = "la ".repeat(30);
        let lrc: String = (0..20).map(|i| format!("[00:{i:02}.00] {i:02} {words}\n")).collect();
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&lrc)));
        let area = Rect::new(0, 0, 80, 21); // one footer row, 20 of lyrics
        let rows = lyrics_panel(&state, &mut ui, area);
        assert_eq!(ui.lyrics_scroll, 20 - 10, "line 10's first row (20) in the middle");
        assert!(rows[10].contains("▸ 10 la"), "{:?}", rows[10]);
        // Every row's text starts in column 10, under the first row's: the
        // timestamp's space is not text.
        for r in &rows[..20] {
            let c: Vec<char> = r.chars().collect();
            assert!(c[9] == ' ' && c[10] != ' ', "{r:?}");
        }
    }

    #[test]
    fn a_rewrap_keeps_the_verse_the_user_scrolled_to() {
        let state = PlayerState::new();
        let mut ui = test_ui(1);
        let words = "la ".repeat(20);
        let text: String = (0..30).map(|i| format!("v{i:02} {words}\n")).collect();
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&text)));
        lyrics_panel(&state, &mut ui, Rect::new(0, 0, 90, 11));
        ui.lyrics_auto_scroll = false;
        ui.lyrics_scroll = 10; // one row a line at this width: v10 on top
        assert!(lyrics_panel(&state, &mut ui, Rect::new(0, 0, 90, 11))[0].contains("v10"));
        // Narrower: every line takes more rows, and v10 is still on top.
        let rows = lyrics_panel(&state, &mut ui, Rect::new(0, 0, 40, 11));
        assert!(rows[0].contains("v10"), "{rows:#?}");
        assert!(ui.lyrics_scroll > 10);
    }

    #[test]
    fn docking_and_focusing_before_a_frame_keeps_the_panel_as_it_was() {
        let state = PlayerState::new();
        let mut ui = test_ui(1);
        let mut playlist = vec![p("/m/a.flac")];
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&"line\n".repeat(100))));
        ui.view_mode = ViewMode::Lyrics; // floating, with the keys
        ui.focus = ViewMode::Lyrics;
        frame(&state, &mut ui, &playlist, 100, 40);
        assert_eq!(ui.floating_shown, Some(ViewMode::Lyrics));
        ui.lyrics_auto_scroll = false;
        ui.lyrics_scroll = 7;
        // k docks it, y hands the keys back, y takes them again: no frame
        // in between, so the last one never showed it docked.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('k'), KeyCode::Char('y'), KeyCode::Char('y')]);
        assert_eq!(ui.view_mode, ViewMode::Lyrics);
        assert_eq!((ui.lyrics_scroll, ui.lyrics_auto_scroll), (7, false));
    }

    #[test]
    fn a_floating_panel_under_the_key_list_or_the_eq_editor_is_on_screen() {
        let state = PlayerState::new();
        let mut ui = test_ui(1);
        let mut playlist = vec![p("/m/a.flac")];
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&"line\n".repeat(100))));
        // Over the floating lyrics, scrolled by hand, the key list (`?`) or
        // the EQ editor (`e`) opens; `y` takes the lyrics back as they were.
        for over in [KeyCode::Char('?'), KeyCode::Char('e')] {
            ui.view_mode = ViewMode::Lyrics;
            ui.focus = ViewMode::Lyrics;
            ui.lyrics_auto_scroll = false;
            ui.lyrics_scroll = 7;
            frame(&state, &mut ui, &playlist, 100, 40);
            press(&state, &mut ui, &mut playlist, &[over]);
            assert!(overlay_open(&ui), "{over:?} is open");
            frame(&state, &mut ui, &playlist, 100, 40);
            assert_eq!(ui.floating_shown, Some(ViewMode::Lyrics), "under {over:?}");
            press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y')]);
            assert_eq!(ui.view_mode, ViewMode::Lyrics, "after {over:?}");
            assert_eq!((ui.lyrics_scroll, ui.lyrics_auto_scroll), (7, false), "after {over:?}");
        }
        // The playlist's cursor stays where the user left it, too.
        let mut playlist: Vec<PathBuf> = (0..5).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(5);
        ui.current = 3;
        for over in [KeyCode::Char('?'), KeyCode::Char('e')] {
            ui.view_mode = ViewMode::Playlist;
            ui.focus = ViewMode::Playlist;
            ui.cursor = 1;
            frame(&state, &mut ui, &playlist, 100, 40);
            press(&state, &mut ui, &mut playlist, &[over]);
            frame(&state, &mut ui, &playlist, 100, 40);
            press(&state, &mut ui, &mut playlist, &[KeyCode::Char('l')]);
            assert_eq!((ui.view_mode, ui.cursor), (ViewMode::Playlist, 1), "after {over:?}");
        }
    }

    #[test]
    fn the_too_small_screen_does_not_make_focusing_a_panel_reset_it() {
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..5).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(5);
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&"line\n".repeat(100))));
        ui.current = 3;
        // Docked lyrics, scrolled by hand; the player has the keys.
        ui.docked = vec![ViewMode::Lyrics];
        frame(&state, &mut ui, &playlist, 120, 50);
        assert_eq!(ui.docked_shown, [ViewMode::Lyrics]);
        ui.lyrics_auto_scroll = false;
        ui.lyrics_scroll = 7;
        // The window shrinks below the minimum, and y focuses them.
        ui.too_small = true;
        frame(&state, &mut ui, &playlist, 30, 8);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y')]);
        assert_eq!((ui.lyrics_scroll, ui.lyrics_auto_scroll), (7, false), "focused as they were");
        // The docked playlist, its cursor moved by hand: the same.
        ui.too_small = false;
        ui.view_mode = ViewMode::Player;
        ui.docked = vec![ViewMode::Playlist];
        ui.cursor = 1;
        frame(&state, &mut ui, &playlist, 120, 50);
        assert_eq!(ui.docked_shown, [ViewMode::Playlist]);
        ui.too_small = true;
        frame(&state, &mut ui, &playlist, 30, 8);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('l')]);
        assert_eq!(ui.cursor, 1, "the cursor stays where it was moved");
    }

    #[test]
    fn scrolling_while_the_lyrics_are_not_drawn_does_not_pile_up() {
        let state = PlayerState::new();
        let mut playlist = vec![p("/m/a.flac")];
        let mut ui = test_ui(1);
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&"line\n".repeat(100))));
        ui.view_mode = ViewMode::Lyrics;
        ui.focus = ViewMode::Lyrics;
        frame(&state, &mut ui, &playlist, 100, 40);
        // s held down while the too-small screen is up: nothing to scroll.
        ui.too_small = true;
        for _ in 0..50 {
            press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s')]);
            frame(&state, &mut ui, &playlist, 30, 8);
        }
        ui.too_small = false;
        frame(&state, &mut ui, &playlist, 100, 40);
        assert_eq!(ui.lyrics_scroll, 0, "no jump when the panel comes back");
        // One press with the panel up still scrolls it.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s')]);
        frame(&state, &mut ui, &playlist, 100, 40);
        assert_eq!(ui.lyrics_scroll, 1);
    }

    #[test]
    fn tricky_lyrics_reach_the_screen_whole_in_every_theme_and_width() {
        use crate::theme::ThemeKind;
        // Emoji sequences, flags, joiners after CJK and between Sinhala
        // letters, halfwidth kana with sound marks, text-style emoji.
        let lines = [
            "日本語の歌詞 \u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F468} \u{1F1F5}\u{1F1F9}\u{1F1E7}\u{1F1F7} love",
            "\u{4E2D}\u{200D}x \u{4E2D}\u{200D}\u{4E2D} \u{1F44D}\u{200D}a \u{0DC1}\u{0DCA}\u{200D}\u{0DBB}\u{0DD3} ok",
            "\u{FF76}\u{FF9E}\u{FF76}\u{FF9E}\u{FF76}\u{FF9E} \u{231A}\u{FE0E} 1\u{FE0F}\u{20E3} e\u{301}e\u{301} end",
            "aaaaaaaaaaaaaaaaaaaa\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}bbbbbbbbbbbbbbbbbbbb",
        ];
        let state = PlayerState::new();
        let mut whole = 0;
        for kind in [ThemeKind::Classic, ThemeKind::Minimal, ThemeKind::HiFi] {
            state.set_theme(kind);
            for w in 10..60 {
                let mut ui = test_ui(1);
                ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&lines.join("\n"))));
                let rows = lyrics_panel(&state, &mut ui, Rect::new(0, 0, w, 39));
                for r in &rows {
                    assert!(tui::width(r) <= w as usize, "{kind:?} at {w}: {r:?} past the panel");
                }
                if rows.iter().any(|r| r.contains('┃')) {
                    continue; // they scroll: not all of them on screen at once
                }
                whole += 1;
                let shown: String = rows.concat().chars().filter(|c| *c != ' ').collect();
                for line in lines {
                    let line = if kind == ThemeKind::HiFi { line.to_uppercase() } else { line.to_string() };
                    let want: String = line.chars().filter(|c| *c != ' ').collect();
                    assert!(shown.contains(&want), "{kind:?} at {w}: {line:?} not whole in {rows:#?}");
                }
                if kind == ThemeKind::Minimal {
                    // Centred: as much room on the left as on the right,
                    // give or take the odd column (text_w = w - 1).
                    // The lyric rows: up to the blank rows above the footer.
                    for r in rows.iter().skip_while(|r| r.trim().is_empty()).take_while(|r| !r.trim().is_empty()) {
                        // Rows come back trimmed, measured as drawn.
                        let left = r.len() - r.trim_start().len();
                        let right = (w as usize - 1).saturating_sub(tui::width(r));
                        assert!(left.abs_diff(right) <= 1, "at {w}, off centre: {r:?}");
                    }
                }
            }
        }
        assert!(whole > 100, "only {whole} of 150 panels checked whole");
    }

    #[test]
    fn a_panel_closed_and_opened_again_before_a_frame_opens_set_up() {
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..5).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(5);
        ui.current = 3;
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&"line\n".repeat(100))));
        // A fast double tap on the floating playlist: closed, then opened.
        ui.view_mode = ViewMode::Playlist;
        ui.cursor = 1;
        frame(&state, &mut ui, &playlist, 120, 50);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('l'), KeyCode::Char('l')]);
        assert_eq!((ui.view_mode, ui.cursor), (ViewMode::Playlist, 3), "set up: on the playing track");
        // The floating lyrics, closed and opened on the too-small screen.
        ui.view_mode = ViewMode::Lyrics;
        frame(&state, &mut ui, &playlist, 120, 50);
        ui.lyrics_auto_scroll = false;
        ui.lyrics_scroll = 7;
        ui.too_small = true;
        frame(&state, &mut ui, &playlist, 30, 8);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y'), KeyCode::Char('y')]);
        assert_eq!((ui.lyrics_scroll, ui.lyrics_auto_scroll), (0, true), "set up: following the song");
        // Docked, closed with Shift+K there, and opened again.
        ui.too_small = false;
        ui.view_mode = ViewMode::Player;
        ui.docked = vec![ViewMode::Lyrics];
        frame(&state, &mut ui, &playlist, 120, 50);
        ui.lyrics_auto_scroll = false;
        ui.lyrics_scroll = 7;
        ui.too_small = true;
        frame(&state, &mut ui, &playlist, 30, 8);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y'), KeyCode::Char('K'), KeyCode::Char('y')]);
        assert!(ui.docked.is_empty());
        assert_eq!((ui.lyrics_scroll, ui.lyrics_auto_scroll), (0, true), "set up after Shift+K");
    }

    #[test]
    fn a_status_message_sits_in_the_panels_border_and_covers_nothing() {
        let state = PlayerState::new();
        state.output_rate.store(48_000, Ordering::Relaxed);
        state.samples_played.store(48_000 * 50 + 24_000, Ordering::Relaxed); // line 50
        let mut playlist: Vec<PathBuf> = (0..100).map(|i| p(&format!("/{i:03}.flac"))).collect();
        let lrc: String = (0..100).map(|i| format!("[{:02}:{:02}.00]line {i:02}\n", i / 60, i % 60)).collect();
        for (kind, docked) in [crate::theme::ThemeKind::Classic, crate::theme::ThemeKind::Minimal, crate::theme::ThemeKind::HiFi]
            .into_iter()
            .flat_map(|k| [(k, false), (k, true)])
        {
            state.set_theme(kind);
            for view in [ViewMode::Lyrics, ViewMode::Playlist] {
                let mut ui = test_ui(100);
                ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&lrc)));
                ui.view_mode = view;
                ui.focus = view;
                if docked {
                    ui.docked = vec![view];
                }
                frame(&state, &mut ui, &playlist, 100, 50);
                // The playlist's cursor down the list, until it scrolls.
                if view == ViewMode::Playlist {
                    for _ in 0..40 {
                        press(&state, &mut ui, &mut playlist, &[KeyCode::Down]);
                        frame(&state, &mut ui, &playlist, 100, 50);
                    }
                }
                let before = frame(&state, &mut ui, &playlist, 100, 50);
                let box_of = |ui: &UiState| {
                    *ui.hits.iter().rev().find_map(|(r, h)| matches!(h, Hit::Dialog | Hit::Panel(_)).then_some(r)).expect("the panel")
                };
                let box_before = box_of(&ui);
                let (scroll, list) = (ui.lyrics_scroll, ui.scroll_offset);
                ui.set_status("saved to the playlist".to_string());
                let after = frame(&state, &mut ui, &playlist, 100, 50);
                let rows = tui::testing::rows(&after);
                // The panel's box (the player's own status line shows it too,
                // outside the box).
                let boxed = box_of(&ui);
                let at = boxed.bottom() - 1;
                let in_border = rows[at as usize].contains("saved to the playlist");
                assert_eq!(ui.docked_shown.contains(&view), docked, "{kind:?} {view:?}: docked as asked at 100×50");
                if docked {
                    // The player's own status line is in view beside a docked
                    // panel: it shows the message, the panel does not repeat it.
                    assert!(!in_border, "{kind:?} {view:?} docked: shown twice: {rows:#?}");
                    let elsewhere = (0..50u16).any(|y| {
                        (y < boxed.top() || y >= boxed.bottom()) && rows[y as usize].contains("saved to the playlist")
                    });
                    assert!(elsewhere, "{kind:?} {view:?} docked: the player's status line lost it: {rows:#?}");
                    continue;
                }
                assert!(
                    in_border && rows[at as usize].contains('─'),
                    "{kind:?} {view:?} floating: not on the bottom border: {rows:#?}"
                );
                // Minimal's own status line replaces its 4-row command tray,
                // which moves the slot a docked panel sits in (the theme's
                // layout, not the panel's): there the box itself moves.
                assert_eq!(boxed, box_before, "{kind:?} {view:?} floating: the box moved");
                // Inside the box, only the border row changed.
                for y in boxed.top()..at {
                    for x in boxed.left()..boxed.right() {
                        assert_eq!(before[(x, y)], after[(x, y)], "{kind:?} {view:?} docked {docked}: ({x}, {y}) changed");
                    }
                }
                let at = at as usize;
                assert_eq!((ui.lyrics_scroll, ui.scroll_offset), (scroll, list), "{kind:?} {view:?} docked {docked}: nothing moved");
                // A click on it reaches no list row.
                assert!(
                    !ui.hits.iter().any(|(r, h)| matches!(h, Hit::List { .. } | Hit::Tree { .. }) && r.y <= at as u16 && (at as u16) < r.bottom()),
                    "{kind:?} {view:?} docked {docked}: a list row under the status"
                );
            }
        }
    }

    #[test]
    fn a_docked_panel_shows_a_status_nothing_else_showed() {
        let mut ui = test_ui(1);
        ui.frame_status = Some("output moved".to_string());
        ui.view_mode = ViewMode::Lyrics;
        ui.docked_shown = vec![ViewMode::Lyrics];
        let p = crate::theme::palette(crate::theme::ThemeKind::Classic);
        let boxed = Rect::new(0, 10, 40, 10);
        ui.hits = vec![(boxed, Hit::Panel(ViewMode::Lyrics))];
        // The theme drew its status line: the panel leaves it there.
        ui.status_drawn_at = Some(Rect::new(0, 28, 20, 1));
        let shown = |ui: &UiState| {
            let buf = tui::testing::draw(40, 30, |f| docked_status(f, ui, p));
            tui::testing::rows(&buf).iter().filter(|r| r.contains("output moved")).count()
        };
        assert_eq!(shown(&ui), 0);
        // It did not (its row off the screen): the docked panel shows it.
        ui.status_drawn_at = None;
        assert_eq!(shown(&ui), 1);
    }

    #[test]
    fn a_box_with_no_inside_puts_no_status_below_it() {
        let mut ui = test_ui(1);
        ui.frame_status = Some("saved".to_string());
        let p = crate::theme::palette(crate::theme::ThemeKind::Classic);
        // A one-row box: its inside is empty and starts below it.
        let buf = tui::testing::draw(40, 10, |f| {
            let (boxed, inner) = dialog_box(f, &mut ui.hits, 30, 1, "t", p);
            assert_eq!(inner.height, 0);
            border_status(f, &ui, boxed, inner, p);
        });
        assert!(!tui::testing::rows(&buf).iter().any(|r| r.contains("saved")));
    }

    #[test]
    fn k_in_hifi_docks_only_what_fits() {
        // The dock area existed but was narrower than the panel: `k` said
        // "docked" for a panel `dock` could never place.
        let state = PlayerState::new();
        state.set_theme(crate::theme::ThemeKind::HiFi);
        let narrow = Rect::new(0, 0, 48, 40);
        assert!(crate::ui_hifi::dock_region(narrow).is_some(), "a dock area, too narrow");
        assert!(!dock_room(&state, ViewMode::Playlist, narrow));
        assert!(dock_room(&state, ViewMode::Lyrics, narrow), "the lyrics need less");
        assert!(dock_room(&state, ViewMode::Playlist, Rect::new(0, 0, 120, 40)));
        // Classic and Minimal dock into their visualization slot: no check here.
        state.set_theme(crate::theme::ThemeKind::Classic);
        assert!(dock_room(&state, ViewMode::Playlist, narrow));
    }

    #[test]
    fn shift_l_in_hifi_changes_nothing_hidden() {
        // HiFi's VU panel has no extra detail: Shift+L flipped the hidden
        // setting of another theme's visualization, and said so wrongly.
        let state = PlayerState::new();
        state.set_theme(crate::theme::ThemeKind::HiFi);
        let mut playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        let extras = state.viz_extras();
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
        poll_input(&state, &mut ui, &mut playlist);
        assert_eq!(state.viz_extras(), extras, "the other themes' setting left alone");
        let status = ui.status_message.as_ref().map(|(m, _)| m.clone());
        assert!(status.as_deref().is_some_and(|m| m.contains("Hi-Fi")), "{status:?}");
    }

    #[test]
    fn a_field_keeps_bidi_marks_in_its_text_but_never_draws_them() {
        // A pasted right-to-left override reversed the rest of the row in a
        // bidi-aware terminal. The path keeps it (a real name may have it);
        // the screen never gets it.
        let field = TextInput::with("\u{202E}abc\u{200F}d");
        let (line, col) = field_line(Line::raw("› "), &field, 40);
        let drawn = tui::plain(&line);
        assert!(!drawn.contains('\u{202E}') && !drawn.contains('\u{200F}'), "{drawn:?}");
        assert!(drawn.contains("abcd"), "{drawn:?}");
        assert_eq!(col, 2 + 4, "the cursor after the drawn text");
        assert_eq!(field.text(), "\u{202E}abc\u{200F}d", "the field's text untouched");
    }

    #[test]
    fn shift_capitals_only_keys_keet_binds_and_never_typing() {
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let shift = KeyModifiers::SHIFT;
        // Typing into a field (Caps Lock on: Shift+a arrives as `a` with
        // Shift and means a lowercase a): typed as it came.
        let mut ui = test_ui(1);
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::new());
        ui.deferred_event = Some(Event::Key(KeyEvent::new(KeyCode::Char('a'), shift)));
        poll_input(&state, &mut ui, &mut playlist);
        assert!(matches!(&ui.input_mode, InputMode::Search(t) if t.text() == "a"), "typed as it came");
        // A letter Keet binds no capital for: still the lowercase key
        // (Shift+E opens the EQ, as before).
        let mut ui = test_ui(1);
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char('e'), shift));
        poll_input(&state, &mut ui, &mut playlist);
        assert_eq!(ui.view_mode, ViewMode::Eq);
        // A bound capital: Shift+L, as before.
        let mut ui = test_ui(1);
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char('l'), shift));
        poll_input(&state, &mut ui, &mut playlist);
        assert_eq!(ui.view_mode, ViewMode::Player, "Shift+L, not the playlist");
    }

    #[test]
    fn every_shift_letter_works_however_the_terminal_reports_it() {
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let shift = KeyModifiers::SHIFT;
        let mut ui = test_ui(1);
        // Shift+L as `l` with Shift: the VU history / legend, not the playlist.
        let extras = state.viz_extras();
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char('l'), shift));
        poll_input(&state, &mut ui, &mut playlist);
        assert_eq!(ui.view_mode, ViewMode::Player, "not the playlist");
        assert_ne!(state.viz_extras(), extras, "Shift+L toggled");
        // Shift+F as `f` with Shift: full window, not pre-fader metering.
        let (full, pre) = (state.viz_fullscreen(), state.is_pre_fader());
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char('f'), shift));
        poll_input(&state, &mut ui, &mut playlist);
        assert_ne!(state.viz_fullscreen(), full, "Shift+F toggled");
        assert_eq!(state.is_pre_fader(), pre, "not pre-fader");
    }

    #[test]
    fn shift_k_and_shift_w_work_however_the_terminal_reports_them() {
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let shift = KeyModifiers::SHIFT;
        for k in [KeyEvent::new(KeyCode::Char('K'), shift), KeyEvent::new(KeyCode::Char('k'), shift), KeyEvent::new(KeyCode::Char('K'), KeyModifiers::NONE)] {
            let mut ui = test_ui(1);
            ui.docked = vec![ViewMode::Lyrics];
            ui.docked_shown = vec![ViewMode::Lyrics];
            ui.view_mode = ViewMode::Lyrics;
            ui.pressed.push_back(k);
            poll_input(&state, &mut ui, &mut playlist);
            // Closed, not floated (which empties the dock too, keeping the keys).
            assert!(ui.docked.is_empty() && ui.view_mode == ViewMode::Player, "{k:?} closes the docked panel");
        }
        for k in [KeyEvent::new(KeyCode::Char('W'), shift), KeyEvent::new(KeyCode::Char('w'), shift)] {
            let mut ui = test_ui(1);
            ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&"line\n".repeat(50))));
            ui.view_mode = ViewMode::Lyrics; // where plain w scrolls
            ui.pressed.push_back(k);
            poll_input(&state, &mut ui, &mut playlist);
            assert!(ui.save_defaults, "{k:?} saves the defaults");
            assert_eq!(ui.lyrics_step, 0, "{k:?} does not scroll");
        }
    }

    #[test]
    fn a_paste_is_cleaned_and_clears_an_old_error() {
        let playlist: Vec<PathBuf> = vec![p("/m/Beatles.flac"), p("/m/Other.flac")];
        let mut ui = test_ui(2);
        // With no field open: the `o` dialog, the path cleaned (the copied
        // line's newline gone, a stray escape's ESC a space).
        paste(&mut ui, &playlist, "/Music/a\nb\x1B[31mc\n");
        assert!(matches!(&ui.input_mode, InputMode::OpenPath(t) if t.text() == "/Music/a b [31mc"));
        // Into an `o` dialog showing "Not found": the error goes with the
        // new path, as typing clears it.
        ui.open_error = Some("Not found: /x".into());
        paste(&mut ui, &playlist, "/Music/b");
        assert_eq!(ui.open_error, None);
        // Into a search: a copied line finds what it names.
        ui.input_mode = InputMode::Search(TextInput::new());
        ui.view_mode = ViewMode::Playlist;
        paste(&mut ui, &playlist, "Beatles\n");
        assert!(matches!(&ui.input_mode, InputMode::Search(t) if t.text() == "Beatles"));
        assert_eq!(ui.filtered_indices, [0]);
        // A double-click copy's trailing space: a search and a name trim it.
        ui.input_mode = InputMode::Search(TextInput::new());
        paste(&mut ui, &playlist, " Beatles ");
        assert!(matches!(&ui.input_mode, InputMode::Search(t) if t.text() == "Beatles"));
        ui.input_mode = InputMode::SavePlaylist(TextInput::new());
        paste(&mut ui, &playlist, "mix ");
        assert!(matches!(&ui.input_mode, InputMode::SavePlaylist(t) if t.text() == "mix"));
        // A path keeps its spaces: a paste can continue one ("~/My" + " Music").
        ui.input_mode = InputMode::OpenPath(TextInput::with("~/My"));
        paste(&mut ui, &playlist, " Music");
        assert!(matches!(&ui.input_mode, InputMode::OpenPath(t) if t.text() == "~/My Music"));
    }

    #[test]
    fn the_event_after_an_esc_is_kept_unless_it_makes_a_sequence() {
        use event::{MouseButton, MouseEventKind as K};
        let mut ui = test_ui(1);
        // A key press right behind it: an escape sequence (Cmd+Arrow).
        let key = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(esc_then(&mut ui, key), "the pair is a sequence");
        assert!(ui.deferred_event.is_none());
        // A click, a paste: kept for after the Esc, not thrown away.
        for next in [Event::Mouse(click(K::Down(MouseButton::Left), 3, 4)), Event::Paste("/Music/x".into())] {
            assert!(!esc_then(&mut ui, next.clone()), "a real Esc");
            assert_eq!(ui.deferred_event.take(), Some(next));
        }
        // And `poll_input` handles a kept one first: the paste opens the
        // `o` dialog with the path.
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        ui.deferred_event = Some(Event::Paste("/Music/x".into()));
        poll_input(&state, &mut ui, &mut playlist);
        assert!(matches!(&ui.input_mode, InputMode::OpenPath(t) if t.text() == "/Music/x"));
    }

    #[test]
    fn a_search_with_no_matches_is_an_empty_list() {
        use event::MouseEventKind as K;
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..30).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(30);
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::with("zzz"));
        rebuild_filter(&mut ui, &playlist);
        assert!(ui.filtered_indices.is_empty());
        // The wheel over "(no matches)": nothing to move through.
        ui.hits = vec![(Rect::new(0, 0, 100, 30), Hit::Dialog), (Rect::new(2, 2, 90, 20), Hit::List { first: 0 })];
        for _ in 0..5 {
            mouse(&state, &mut ui, &playlist, click(K::ScrollDown, 10, 5));
        }
        assert_eq!(ui.cursor, 0, "no hidden cursor walking the whole playlist");
        // Enter and the list's keys act on nothing (the field closed first
        // by Esc would bring the whole list back; here it stays open).
        assert!(shown_track(&ui, ui.cursor).is_none());
        let before = playlist.len();
        enqueue_track(&state, &mut ui, &mut playlist);
        remove_track(&state, &mut ui, &mut playlist);
        assert_eq!(playlist.len(), before, "nothing removed");
        assert_eq!(ui.enqueue_count, 0, "nothing queued");
    }

    #[test]
    fn opening_the_playlist_with_a_filter_puts_the_cursor_among_the_matches() {
        let playlist: Vec<PathBuf> = (0..30).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(30);
        // A search for "3" left open: 03, 13, 23 match.
        ui.input_mode = InputMode::Search(TextInput::with("3"));
        rebuild_filter(&mut ui, &playlist);
        // The playing track is a match: the cursor is on it in the matches.
        ui.current = 13;
        open_view(&mut ui, ViewMode::Playlist);
        assert_eq!(ui.filtered_indices[ui.cursor], 13);
        // It is not: the first match, never a position past them.
        ui.view_mode = ViewMode::Player;
        ui.current = 20;
        open_view(&mut ui, ViewMode::Playlist);
        assert_eq!(ui.cursor, 0);
        // A search with nothing typed filters nothing: the playing track.
        ui.view_mode = ViewMode::Player;
        ui.input_mode = InputMode::Search(TextInput::new());
        rebuild_filter(&mut ui, &playlist);
        open_view(&mut ui, ViewMode::Playlist);
        assert_eq!(ui.cursor, 20, "the whole list shows: on the playing track");
    }

    #[test]
    fn a_double_click_near_the_edge_plays_that_row() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..80).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(80);
        ui.view_mode = ViewMode::Playlist;
        frame(&state, &mut ui, &playlist, 100, 40);
        let list = ui.hits.iter().find_map(|(r, h)| matches!(h, Hit::List { .. }).then_some(*r)).expect("the list");
        // The second-to-last visible row: inside the 4-row margin.
        let (x, y) = (list.x + 3, list.bottom() - 2);
        let row = list.height as usize - 2;
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), x, y));
        assert_eq!(ui.cursor, row);
        // The frame between the two clicks (the first marked it dirty).
        frame(&state, &mut ui, &playlist, 100, 40);
        assert_eq!(ui.scroll_offset, 0, "a click does not scroll the list: the row is under the pointer");
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), x, y));
        assert_eq!(state.take_jump(), Some(row), "the double click played the row clicked");
        // A key moving the cursor scrolls with the margin as ever, even
        // before a frame took the click's note.
        let mut playlist = playlist;
        press(&state, &mut ui, &mut playlist, &[KeyCode::Down]);
        frame(&state, &mut ui, &playlist, 100, 40);
        assert!(ui.scroll_offset > 0, "a key move scrolls as before");
    }

    #[test]
    fn a_docked_list_follows_the_playing_track_without_the_keys() {
        let mut ui = test_ui(10);
        ui.view_mode = ViewMode::Player;
        ui.docked_shown = vec![ViewMode::Playlist];
        ui.cursor = 3;
        ui.current = 4;
        follow_playing(&mut ui);
        assert_eq!(ui.cursor, 4, "the docked list's cursor goes with ▶");
        // Not on screen: left where it was.
        ui.docked_shown.clear();
        ui.current = 5;
        follow_playing(&mut ui);
        assert_eq!(ui.cursor, 4);
    }

    #[test]
    fn a_dialog_repaints_the_screen_only_when_an_image_is_involved() {
        let mut ui = test_ui(1);
        tui::forget_images();
        assert!(!overlay_change_repaints(&ui), "no image anywhere: ratatui's diff is enough");
        // An image on screen may go under the dialog.
        tui::place_image(tui::ImageSlot::Cover, Rect::new(0, 0, 20, 10), Some("img".into()));
        tui::emit_images();
        assert!(overlay_change_repaints(&ui));
        // One held back comes back when it closes.
        tui::forget_images();
        ui.images_withheld = vec![tui::ImageSlot::Viz];
        assert!(overlay_change_repaints(&ui));
    }

    #[test]
    fn full_window_hands_the_keys_back_and_keeps_the_dock() {
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.docked = vec![ViewMode::Lyrics];
        ui.docked_shown = vec![ViewMode::Lyrics];
        ui.view_mode = ViewMode::Lyrics; // the docked lyrics have the keys
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('F')]);
        assert!(state.viz_fullscreen());
        assert_eq!(ui.view_mode, ViewMode::Player, "only the visualization shows: no panel floats over it");
        assert_eq!(ui.docked, vec![ViewMode::Lyrics], "the dock comes back on the way out");
    }

    #[test]
    fn the_offset_keys_leave_plain_lyrics_alone() {
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.view_mode = ViewMode::Lyrics;
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics("just words\nno times")));
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('a'), KeyCode::Char('a'), KeyCode::Char('d')]);
        assert_eq!(ui.lyrics_offset, 0.0, "no offset for lyrics with no times");
        assert_eq!(ui.lyrics_offsets.get(&playlist[0]), 0.0, "and none saved for the track");
        // Synced lyrics take them as ever.
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics("[00:01.00]one\n[00:02.00]two")));
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('d')]);
        assert_eq!(ui.lyrics_offset, 0.5);
        assert_eq!(ui.lyrics_offsets.get(&playlist[0]), 0.5);
    }

    #[test]
    fn a_frame_drawn_twice_keeps_the_rows_clicked_under_the_pointer() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..80).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(80);
        ui.view_mode = ViewMode::Playlist;
        ui.library_tree_mode = true;
        // A tree as tall as the list: one artist per track.
        let tags: Vec<_> = (0..80)
            .map(|i| crate::library::TrackTags {
                artist: Some(format!("Artist {i:02}")),
                album: Some("Album".into()),
                disc: None,
                track: Some(1),
                title: format!("t{i}"),
            })
            .collect();
        ui.library_tree = crate::library::build(&tags);
        refresh_tree_rows(&mut ui);
        frame(&state, &mut ui, &playlist, 160, 40);
        let row_near_the_bottom = |ui: &UiState, tree: bool| {
            let r = ui.hits.iter().find_map(|(r, h)| match h {
                Hit::List { .. } if !tree => Some(*r),
                Hit::Tree { .. } if tree => Some(*r),
                _ => None,
            });
            r.map(|r| (r.x + 3, r.bottom() - 2))
        };
        for tree in [false, true] {
            let Some((x, y)) = row_near_the_bottom(&ui, tree) else { panic!("pane {tree} not drawn") };
            mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), x, y));
            let before = (ui.scroll_offset, ui.tree_scroll);
            // The Sixel retry: a first pass thrown away, then the frame shown.
            let once = DrawOnce::of(&ui);
            frame(&state, &mut ui, &playlist, 160, 40);
            once.restore(&mut ui);
            frame(&state, &mut ui, &playlist, 160, 40);
            assert_eq!((ui.scroll_offset, ui.tree_scroll), before, "tree pane {tree}: the row stays under the pointer");
            frame(&state, &mut ui, &playlist, 160, 40);
        }
    }

    #[test]
    fn a_double_click_plays_the_track_clicked_even_if_the_list_scrolled_between() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..50).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(50);
        ui.view_mode = ViewMode::Playlist;
        let list = |first: usize| vec![(Rect::new(0, 0, 100, 30), Hit::Dialog), (Rect::new(2, 2, 90, 20), Hit::List { first })];
        // First click on screen row 18 (track 18, near the bottom edge).
        ui.hits = list(0);
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 10, 20));
        assert_eq!(ui.cursor, 18);
        // The redraw scrolled the list by 3; the second click, same screen
        // row, is track 21: not a double click on track 18.
        ui.hits = list(3);
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 10, 20));
        assert_eq!(state.take_jump(), None, "track 21 must not play: it was clicked once");
        // Clicking track 21 again is a double click on it.
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 10, 20));
        assert_eq!(state.take_jump(), Some(21));
    }

    #[test]
    fn a_click_on_the_other_pane_closes_the_search_with_its_filter() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..30).map(|i| p(&format!("/{i:02}.flac"))).collect();
        for tree_first in [false, true] {
            let mut ui = test_ui(30);
            ui.view_mode = ViewMode::Playlist;
            ui.library_tree_mode = tree_first;
            ui.input_mode = InputMode::Search(TextInput::with("3"));
            if tree_first {
                ui.tree_filter = "3".to_string();
            } else {
                rebuild_filter(&mut ui, &playlist);
            }
            // Two panes: the list on the left, the tree on the right.
            ui.hits = vec![
                (Rect::new(0, 0, 100, 40), Hit::Dialog),
                (Rect::new(2, 2, 50, 30), Hit::List { first: 0 }),
                (Rect::new(60, 2, 38, 30), Hit::Tree { first: 0 }),
            ];
            let (x, y) = if tree_first { (5, 3) } else { (65, 3) };
            mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), x, y));
            assert_eq!(ui.library_tree_mode, !tree_first, "the click moved the keys");
            assert!(matches!(ui.input_mode, InputMode::Normal), "tree first {tree_first}: the search closed");
            assert!(ui.filtered_indices.is_empty() && ui.tree_filter.is_empty(), "tree first {tree_first}: no filter left behind");
        }
    }

    #[test]
    fn images_under_a_dialog_are_not_rebuilt_every_frame() {
        use tui::ImageSlot::{Cover, Viz};
        let mut ui = test_ui(1);
        ui.cover_block_intact = true;
        ui.spectro_block_intact = true;
        // The viz newly covered: marked to be sent again, once.
        note_withheld(&mut ui, vec![Viz], false);
        assert!(!ui.spectro_block_intact && ui.cover_block_intact, "only the covered one");
        // Redrawn under the dialog, it is intact again; still covered next
        // frame, it is not marked again (it was re-encoded every frame, for
        // nothing: withheld).
        ui.spectro_block_intact = true;
        note_withheld(&mut ui, vec![Viz], false);
        assert!(ui.spectro_block_intact, "not rebuilt again while the dialog is up");
        // Uncovered, then covered again: marked again.
        note_withheld(&mut ui, vec![], false);
        note_withheld(&mut ui, vec![Viz, Cover], false);
        assert!(!ui.spectro_block_intact && !ui.cover_block_intact);
    }

    #[test]
    fn lyrics_that_could_not_be_looked_up_say_so() {
        let state = PlayerState::new();
        let mut ui = test_ui(1);
        ui.lyrics_unavailable = Some("LRCLIB busy, try again later");
        let rows = lyrics_panel(&state, &mut ui, Rect::new(0, 0, 80, 10));
        assert!(rows.iter().any(|r| r.contains("lyrics unavailable: LRCLIB busy")), "{rows:#?}");
        assert!(!rows.iter().any(|r| r.contains("no lyrics for this track")), "not a missing track");
        ui.lyrics_unavailable = None;
        let rows = lyrics_panel(&state, &mut ui, Rect::new(0, 0, 80, 10));
        assert!(rows.iter().any(|r| r.contains("no lyrics for this track")));
        // New lyrics (another track's, embedded: no LRCLIB asked) clear it:
        // every change of lyrics goes through `set_lyrics`.
        ui.lyrics_unavailable = Some("LRCLIB busy, try again later");
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics("words")));
        assert_eq!(ui.lyrics_unavailable, None);
    }

    #[test]
    fn the_key_list_says_when_the_window_cuts_it() {
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.view_mode = ViewMode::Help;
        let rows = tui::testing::rows(&frame(&state, &mut ui, &playlist, 70, 16));
        assert!(rows.iter().any(|r| r.contains("widen the window")), "cut, and says so: {rows:#?}");
        let rows = tui::testing::rows(&frame(&state, &mut ui, &playlist, 240, 80));
        assert!(!rows.iter().any(|r| r.contains("widen the window")), "whole: nothing to say");
    }

    #[test]
    fn a_new_source_closes_a_search_left_open() {
        let state = PlayerState::new();
        let dir = std::env::temp_dir().join(format!("keet-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a.flac", "b.flac"] {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        let mut playlist: Vec<PathBuf> = (0..30).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(30);
        // A search for "3" (03, 13, 23) left open while the player has the keys.
        ui.input_mode = InputMode::Search(TextInput::with("3"));
        rebuild_filter(&mut ui, &playlist);
        assert!(ui.filtered_indices.iter().any(|&i| i >= 2));
        switch_source_paths(&state, &mut ui, &mut playlist, dir.clone());
        assert_eq!(playlist.len(), 2);
        assert!(matches!(ui.input_mode, InputMode::Normal) && ui.filtered_indices.is_empty(), "the old search went");
        // And the docked list draws (it indexed past the new list's end).
        ui.docked = vec![ViewMode::Playlist];
        frame(&state, &mut ui, &playlist, 120, 50);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_playlist_edit_keeps_a_searchs_cursor_on_its_track() {
        let state = PlayerState::new();
        // Hits for "3": 03, 13, 23; the cursor on the third (23).
        let fresh = || {
            let playlist: Vec<PathBuf> = (0..30).map(|i| p(&format!("/{i:02}.flac"))).collect();
            let mut ui = test_ui(30);
            ui.view_mode = ViewMode::Playlist;
            ui.input_mode = InputMode::Search(TextInput::with("3"));
            rebuild_filter(&mut ui, &playlist);
            ui.cursor = 2;
            (ui, playlist)
        };
        let on = |ui: &UiState, playlist: &[PathBuf]| shown_track(ui, ui.cursor).map(|i| playlist[i].clone());
        let p23 = Some(p("/23.flac"));
        // Shuffle, sort, removing another track: still on 23 (all reset
        // it to the first hit, and Enter played that).
        let (mut ui, mut playlist) = fresh();
        toggle_shuffle(&state, &mut ui, &mut playlist);
        assert_eq!(on(&ui, &playlist), p23, "shuffle");
        let (mut ui, mut playlist) = fresh();
        sort_playlist_by_tags(&state, &mut ui, &mut playlist);
        assert_eq!(on(&ui, &playlist), p23, "sort");
        let (mut ui, mut playlist) = fresh();
        remove_indices(&state, &mut ui, &mut playlist, &[5]);
        assert_eq!(on(&ui, &playlist), p23, "removal of another track");
        // Queuing the cursor's own track moves it: the cursor goes with it.
        let (mut ui, mut playlist) = fresh();
        enqueue_track(&state, &mut ui, &mut playlist);
        assert_eq!(on(&ui, &playlist), p23, "queued");
        // The repeat-all rebuild (another order, a file gone): re-run, hits
        // in the new list, the cursor on its track.
        let (mut ui, playlist) = fresh();
        let keep = cursor_track(&ui, &playlist);
        let old = playlist.clone();
        let playlist: Vec<PathBuf> = old.iter().rev().filter(|p| !p.ends_with("13.flac")).cloned().collect();
        reindex_and_restart_scan(&mut ui, &playlist, &old);
        refilter_to(&mut ui, &playlist, keep);
        let hits: Vec<&PathBuf> = ui.filtered_indices.iter().map(|&i| &playlist[i]).collect();
        assert_eq!(hits, [&p("/23.flac"), &p("/03.flac")], "the query's hits in the new list");
        assert_eq!(on(&ui, &playlist), p23, "repeat-all rebuild");
    }

    #[test]
    fn an_edit_under_an_open_empty_search_keeps_the_cursor_and_the_scroll() {
        let mut playlist: Vec<PathBuf> = (0..60).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(60);
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::with("")); // `/`, nothing typed
        ui.cursor = 30;
        ui.scroll_offset = 25;
        let keep = cursor_track(&ui, &playlist);
        let old = playlist.clone();
        playlist.reverse(); // a rescan or a sort landing meanwhile
        reindex_and_restart_scan(&mut ui, &playlist, &old);
        refilter_to(&mut ui, &playlist, keep.clone());
        assert_eq!(cursor_track(&ui, &playlist), keep, "still on its track (it went to row 0: Enter played track 1)");
        assert_eq!(ui.scroll_offset, 25, "the list did not jump to the top");
    }

    #[test]
    fn an_edit_under_the_trees_search_keeps_the_lists_cursor_on_a_real_track() {
        let state = PlayerState::new();
        let fresh = || {
            let playlist: Vec<PathBuf> = (0..20).rev().map(|i| p(&format!("/{i:02}.flac"))).collect();
            let mut ui = test_ui(20);
            ui.view_mode = ViewMode::Playlist;
            ui.library_tree_mode = true;
            ui.input_mode = InputMode::Search(TextInput::with("x")); // the tree's filter
            ui.current = 3;
            ui.cursor = 3;
            (ui, playlist)
        };
        // A sort: the list's cursor follows the playing track, as with no search.
        let (mut ui, mut playlist) = fresh();
        let playing = playlist[3].clone();
        sort_playlist_by_tags(&state, &mut ui, &mut playlist);
        assert_ne!(playlist[3], playing, "the sort moved it");
        assert_eq!(playlist.get(ui.cursor), Some(&playing), "the list's cursor is on the playing track");
        // A rescan dropping files past the cursor: clamped into the list.
        let dir = std::env::temp_dir().join(format!("keet_tree_rescan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..5 {
            std::fs::write(dir.join(format!("t{i}.flac")), b"").unwrap();
        }
        let mut playlist = crate::playlist::build_playlist(&dir, false).unwrap();
        let mut ui = UiState::new(vec![dir.clone()], crate::metadata::MetadataCache::new(playlist.len()));
        ui.library_tree_mode = true;
        ui.input_mode = InputMode::Search(TextInput::with("x"));
        ui.cursor = 4;
        for i in 2..5 {
            std::fs::remove_file(dir.join(format!("t{i}.flac"))).unwrap();
        }
        let result = crate::playlist::scan_sources(&ui.source_paths.clone(), &playlist);
        finish_rescan(&state, &mut ui, &mut playlist, &result);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(ui.cursor < playlist.len(), "cursor {} in a list of {}", ui.cursor, playlist.len());
    }

    #[test]
    fn a_key_for_a_field_does_not_hold_up_the_input_behind_it() {
        // A field's key returned from poll_input: every typed character cost
        // a loop pass and a frame (a path dropped on Windows arrives as
        // keys), and what was queued behind it waited.
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..3).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(3);
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::SavePlaylist(TextInput::with(""));
        let vol = state.volume_gain();
        ui.pressed.push_back(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)); // the field's key
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE)); // then a command
        poll_input(&state, &mut ui, &mut playlist);
        assert!(ui.pressed.is_empty(), "all handled in one pass");
        assert!(matches!(ui.input_mode, InputMode::Normal));
        assert!(state.volume_gain() < vol, "the command behind it ran");
    }

    #[test]
    fn shift_with_a_letter_bound_only_in_lower_case_is_that_key() {
        let shift = KeyModifiers::SHIFT;
        let code = |c, m| shift_to_capital(KeyEvent::new(KeyCode::Char(c), m)).code;
        // Most Unix terminals: Shift+E is `E` with Shift. Nothing is bound
        // to `E`, so it did nothing; it is `e`, the EQ.
        assert_eq!(code('E', shift), KeyCode::Char('e'));
        assert_eq!(code('e', shift), KeyCode::Char('e'));
        // Bound capitals stay capitals, however reported.
        assert_eq!(code('L', shift), KeyCode::Char('L'));
        assert_eq!(code('l', shift), KeyCode::Char('L'));
        // No Shift held (Caps Lock): left as it came.
        assert_eq!(code('E', KeyModifiers::NONE), KeyCode::Char('E'));
        // End to end: Shift+E opens the EQ editor.
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char('E'), shift));
        poll_input(&state, &mut ui, &mut playlist);
        assert_eq!(ui.view_mode, ViewMode::Eq);
    }

    #[test]
    fn the_trees_cursor_row_keeps_its_colours_in_the_pane_without_the_keys() {
        let tag = |artist: &str, title: &str| crate::library::TrackTags {
            artist: Some(artist.into()),
            album: Some("Album".into()),
            disc: None,
            track: Some(1),
            title: title.into(),
        };
        let mut ui = test_ui(2);
        ui.library_tree = crate::library::build(&[tag("A", "t1"), tag("B", "t2")]);
        refresh_tree_rows(&mut ui);
        ui.tree_cursor = 0;
        tree_expand_under_cursor(&mut ui); // A, Album, B
        ui.tree_cursor = 1;
        tree_expand_under_cursor(&mut ui); // A, Album, t1, B
        ui.current = 0; // t1 plays
        ui.tree_cursor = 2; // on t1
        let p = crate::theme::palette(crate::theme::ThemeKind::Classic);
        let rows = render_tree_body(&mut ui, 10, 40, p, false);
        let row = &rows[2];
        assert!(row.spans.iter().any(|s| s.style.fg == p.accent.fg && s.content.contains("t1")), "the playing track keeps its accent: {row:?}");
        assert!(row.spans.iter().all(|s| s.style.bg != p.cursor_hl.bg || p.cursor_hl.bg.is_none()), "no cursor bar: {row:?}");
        // With the keys: the bar.
        let rows = render_tree_body(&mut ui, 10, 40, p, true);
        assert_eq!(rows[2], tui::cursor_row(p, &crate::library::render_library_tree(&ui.library_tree, &ui.tree_fold, &ui.tree_rows, usize::MAX, 0, 10, 40, p, Some(0))[2], 40));
    }

    #[test]
    fn a_dialogs_key_targets_stay_inside_it_in_a_narrow_window() {
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..3).map(|i| p(&format!("/{i}.flac"))).collect();
        for (theme, dialog) in (0..3).flat_map(|t| (0..3).map(move |d| (t, d))) {
            state.set_theme([crate::theme::ThemeKind::Classic, crate::theme::ThemeKind::Minimal, crate::theme::ThemeKind::HiFi][theme]);
            for w in [40u16, 41, 42, 44, 50, 60] {
                let mut ui = test_ui(3);
                match dialog {
                    0 => ui.tree_pending_remove = Some(("Some Artist With A Long Name".into(), vec![p("/0.flac")])),
                    1 => ui.esc_quit_until = Some(std::time::Instant::now() + Duration::from_secs(5)),
                    _ => ui.input_mode = InputMode::OpenPath(TextInput::with("")),
                }
                frame(&state, &mut ui, &playlist, w, 24);
                let top = ui.hits.iter().rev().find(|(_, h)| *h == Hit::Dialog).map(|(r, _)| *r).expect("the dialog");
                for (r, h) in ui.hits.iter().filter(|(_, h)| matches!(h, Hit::Key(..))) {
                    if r.y >= top.y && r.y < top.bottom() {
                        assert!(r.right() < top.right(), "dialog {dialog} at {w} columns: {h:?} at {r:?} past its box {top:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_clicked_hint_closing_a_search_acts_on_the_track_highlighted() {
        let state = PlayerState::new();
        let click = |ui: &mut UiState, c: char| ui.pressed.push_back(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        // The flat list: matches of "1" are 01, 10, 11, …; the cursor on 11.
        let mut playlist: Vec<PathBuf> = (0..20).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(20);
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::with("1"));
        rebuild_filter(&mut ui, &playlist);
        ui.cursor = 2;
        assert_eq!(cursor_track(&ui, &playlist), Some(p("/11.flac")));
        click(&mut ui, 'd'); // `d remove` in the footer
        poll_input(&state, &mut ui, &mut playlist);
        assert!(!playlist.contains(&p("/11.flac")), "the highlighted match went");
        assert!(playlist.contains(&p("/00.flac")), "not the playlist's first track");
        // The tree: filtered to t2, the cursor on its track row.
        let tag = |i: usize| crate::library::TrackTags {
            artist: Some("A".into()),
            album: Some("Album".into()),
            disc: None,
            track: Some(i as u32 + 1),
            title: format!("t{i}"),
        };
        let mut playlist: Vec<PathBuf> = (0..4).map(|i| p(&format!("/t{i}.flac"))).collect();
        let mut ui = test_ui(4);
        ui.view_mode = ViewMode::Playlist;
        ui.library_tree_mode = true;
        ui.library_tree = crate::library::build(&(0..4).map(tag).collect::<Vec<_>>());
        ui.input_mode = InputMode::Search(TextInput::with("t2"));
        rebuild_tree_filter(&mut ui);
        ui.tree_cursor = ui.tree_rows.iter().position(|r| matches!(r, crate::library::VisibleRow::Track { .. })).expect("t2's row");
        click(&mut ui, 'd');
        poll_input(&state, &mut ui, &mut playlist);
        assert!(ui.tree_pending_remove.is_none(), "not the whole artist: {:?}", ui.tree_pending_remove.as_ref().map(|r| &r.0));
        assert!(!playlist.contains(&p("/t2.flac")), "t2 went");
        assert_eq!(playlist.len(), 3);
    }

    #[test]
    fn a_folder_dropped_on_quit_closes_it_and_esc_then_cancels_the_open_dialog() {
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.esc_quit_until = Some(std::time::Instant::now() + Duration::from_secs(2)); // "Quit?" up
        ui.deferred_event = Some(Event::Paste("/Music/Kind of Blue".into()));
        poll_input(&state, &mut ui, &mut playlist);
        assert!(matches!(ui.input_mode, InputMode::OpenPath(_)), "the drop opened the dialog");
        assert!(!quit_armed(&ui), "and closed \"Quit?\", as any other key does");
        // Esc now cancels what is on screen, and nothing quits.
        ui.pressed.push_back(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        poll_input(&state, &mut ui, &mut playlist);
        assert!(matches!(ui.input_mode, InputMode::Normal));
        assert!(!state.should_quit());
    }

    #[test]
    fn a_new_source_drops_a_tree_filter_kept_after_its_search_closed() {
        let state = PlayerState::new();
        let dir = std::env::temp_dir().join(format!("keet_new_source_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.flac"), b"").unwrap();
        let mut playlist = vec![p("/a.flac"), p("/b.flac")];
        let mut ui = test_ui(2);
        ui.library_tree_mode = true;
        ui.tree_filter = "beatles".into(); // `/beatles`, then Enter: the field closed, the filter kept
        ui.tree_cursor = 7;
        switch_source_paths(&state, &mut ui, &mut playlist, dir.clone());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(playlist.len(), 1);
        assert!(ui.tree_filter.is_empty(), "the new library is not filtered by the old one's query");
        assert_eq!(ui.tree_cursor, 0);
    }

    #[test]
    fn ctrl_with_a_lyrics_key_neither_shifts_nor_saves_the_offset() {
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.view_mode = ViewMode::Lyrics;
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics("[00:01.00]one\n[00:02.00]two")));
        // As typed (the terminal's events), each on its own: Ctrl+D pages
        // the playlist, and pressed here by habit it shifted the lyrics.
        let typed = |ui: &mut UiState, playlist: &mut Vec<PathBuf>, c, m| {
            ui.deferred_event = Some(Event::Key(KeyEvent::new(KeyCode::Char(c), m)));
            poll_input(&state, ui, playlist);
        };
        for (c, m) in [('d', KeyModifiers::CONTROL), ('d', KeyModifiers::ALT), ('a', KeyModifiers::CONTROL), ('a', KeyModifiers::ALT)] {
            typed(&mut ui, &mut playlist, c, m);
            assert_eq!(ui.lyrics_offset, 0.0, "{m:?}+{c} moved the offset");
            assert_eq!(ui.lyrics_offsets.get(&playlist[0]), 0.0, "{m:?}+{c} saved one");
        }
        typed(&mut ui, &mut playlist, 'd', KeyModifiers::NONE);
        assert_eq!(ui.lyrics_offset, 0.5, "the plain key still does");
        typed(&mut ui, &mut playlist, '0', KeyModifiers::CONTROL);
        assert_eq!(ui.lyrics_offset, 0.5, "Ctrl+0 does not reset it");
    }

    #[test]
    fn a_new_lyric_line_asks_for_a_frame_at_once() {
        let state = PlayerState::new();
        state.output_rate.store(48_000, Ordering::Relaxed);
        let mut ui = test_ui(1);
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics("[00:01.00]one\n[00:02.00]two")));
        let at = |secs: f64| state.samples_played.store((secs * 48_000.0) as u64, Ordering::Relaxed);
        // The panel not on screen: nothing to draw.
        at(1.5);
        assert!(!lyrics_line_moved(&mut ui, &state));
        // Floating: the first look counts, then only a change of line.
        ui.view_mode = ViewMode::Lyrics;
        assert!(lyrics_line_moved(&mut ui, &state), "the line it shows now");
        at(1.9);
        assert!(!lyrics_line_moved(&mut ui, &state), "same line");
        at(2.1);
        assert!(lyrics_line_moved(&mut ui, &state), "\"two\" starts: drawn now, not at the idle rate");
        // Docked without the keys too.
        ui.view_mode = ViewMode::Player;
        ui.docked_shown = vec![ViewMode::Lyrics];
        at(0.5);
        assert!(lyrics_line_moved(&mut ui, &state));
    }

    #[test]
    fn a_track_gone_under_an_open_search_leaves_the_cursor_near_where_it_was() {
        let mut playlist: Vec<PathBuf> = (0..60).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(60);
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::with(""));
        ui.cursor = 30;
        let keep = cursor_track(&ui, &playlist);
        // A rescan: the cursor's file was deleted on disk.
        let old = playlist.clone();
        playlist.remove(30);
        reindex_and_restart_scan(&mut ui, &playlist, &old);
        refilter_to(&mut ui, &playlist, keep);
        assert_eq!(ui.cursor, 30, "the row that took its place, not the top of the list");
        // With a query, the same: the nearest hit, not the first.
        let mut playlist: Vec<PathBuf> = (0..60).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(60);
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::with("5"));
        rebuild_filter(&mut ui, &playlist);
        ui.cursor = 6; // 05 15 25 35 45 50 [51] 52 …
        let keep = cursor_track(&ui, &playlist);
        let old = playlist.clone();
        playlist.retain(|t| Some(t) != keep.as_ref());
        reindex_and_restart_scan(&mut ui, &playlist, &old);
        refilter_to(&mut ui, &playlist, keep);
        assert_eq!(ui.cursor, 6);
    }

    #[test]
    fn the_key_list_opens_and_closes_without_a_forced_repaint() {
        // It is a dialog: `Player::repaint_screen` repaints on an overlay
        // change when an image is involved (`overlay_change_repaints`), and
        // otherwise ratatui's diff is enough. Forced, every `?` sent a whole
        // screen through ConPTY.
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.terminal_resized = false;
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('?')]);
        assert_eq!(ui.view_mode, ViewMode::Help);
        assert!(overlay_open(&ui), "an overlay: the image rule applies");
        assert!(!ui.terminal_resized);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('?')]);
        assert_eq!(ui.view_mode, ViewMode::Player);
        assert!(!ui.terminal_resized);
    }

    #[test]
    fn removing_tracks_above_the_cursor_keeps_it_on_its_track() {
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..60).map(|i| p(&format!("/{i:02}.flac"))).collect();
        let mut ui = test_ui(60);
        ui.cursor = 40;
        // An artist's ten tracks above it, removed from the tree.
        remove_indices(&state, &mut ui, &mut playlist, &(5..15).collect::<Vec<_>>());
        assert_eq!(playlist.get(ui.cursor), Some(&p("/40.flac")), "the same song, not the one ten below");
        // Its own track removed: the cursor goes to the one after it, as `D` does.
        let at = ui.cursor;
        remove_indices(&state, &mut ui, &mut playlist, &[at]);
        assert_eq!(playlist.get(ui.cursor), Some(&p("/41.flac")));
    }

    #[test]
    fn s_in_the_player_opens_the_playlist_to_save_and_the_name_goes_there() {
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..3).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(3);
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        poll_input(&state, &mut ui, &mut playlist);
        assert_eq!(ui.view_mode, ViewMode::Playlist, "the field is where it shows");
        assert!(matches!(ui.input_mode, InputMode::SavePlaylist(_)));
        // The name's letters go to it (typed, not clicked: a clicked key
        // closes a field), not to the player: `q` would quit.
        assert!(!handle_text_input(&state, &mut ui, &mut playlist, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(matches!(&ui.input_mode, InputMode::SavePlaylist(t) if t.text() == "q"));
        assert!(!state.should_quit());
    }

    #[test]
    fn a_search_left_open_is_closed_properly_by_o_and_s_and_skipped_by_a_paste() {
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..5).map(|i| p(&format!("/{i}.flac"))).collect();
        let left_open = |ui: &mut UiState, playlist: &[PathBuf]| {
            ui.view_mode = ViewMode::Player;
            ui.input_mode = InputMode::Search(TextInput::with("3"));
            rebuild_filter(ui, playlist);
            assert!(!ui.filtered_indices.is_empty());
        };
        // A paste with the player holding the keys: the `o` dialog, not the
        // search out of focus.
        let mut ui = test_ui(5);
        left_open(&mut ui, &playlist);
        paste(&mut ui, &playlist, "/Music/x");
        assert!(matches!(&ui.input_mode, InputMode::OpenPath(t) if t.text() == "/Music/x"), "the o dialog");
        assert!(ui.filtered_indices.is_empty(), "and the search closed properly");
        // `o` and `s`: the search closes properly (its filter too).
        for key in ['o', 's'] {
            let mut ui = test_ui(5);
            left_open(&mut ui, &playlist);
            ui.pressed.push_back(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
            poll_input(&state, &mut ui, &mut playlist);
            assert!(ui.filtered_indices.is_empty(), "{key}: no filter left without its field");
        }
    }

    #[test]
    fn a_docked_search_shows_its_cursor_and_one_left_open_its_way_in() {
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..5).map(|i| p(&format!("/{i}.flac"))).collect();
        let eq = crate::eq::builtin_presets().remove(0);
        let analyser = VizAnalyser::new(48_000);
        let draw = |ui: &mut UiState| {
            let mut stats = StatsMonitor::new();
            let (buf, cursor) = tui::testing::draw_with_cursor(120, 50, |f| {
                let mut cx = Ctx {
                    state: &state, ui, name: "a", track_info: "", ext: "flac", eq_preset: &eq, fx_name: "None",
                    cf_name: "Off", stats: &mut stats, playlist: &playlist, analyser: &analyser, repaint: true,
                };
                draw(f, &mut cx);
            });
            (tui::testing::rows(&buf), cursor)
        };
        let mut ui = test_ui(5);
        ui.docked = vec![ViewMode::Playlist, ViewMode::Lyrics];
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::with("ab"));
        draw(&mut ui);
        // Docked, with the keys: the cursor is in the field, after "ab".
        let (rows, cursor) = draw(&mut ui);
        let field = rows.iter().position(|r| r.contains("/ ab")).expect("the field");
        assert_eq!(cursor.map(|c| c.1 as usize), Some(field), "{rows:#?}");
        // The lyrics take the keys: no cursor, and the playlist's footer
        // offers its way in above the field left open.
        ui.view_mode = ViewMode::Lyrics;
        let (rows, cursor) = draw(&mut ui);
        assert_eq!(cursor, None);
        let field = rows.iter().position(|r| r.contains("/ ab")).expect("the field still shows");
        assert!(rows[..field].iter().any(|r| r.contains("l focus")), "{rows:#?}");
    }

    #[test]
    fn a_search_left_open_keeps_its_place_and_takes_no_other_panels_keys() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..5).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(5);
        ui.docked = vec![ViewMode::Playlist];
        ui.docked_shown = ui.docked.clone();
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::with("3"));
        rebuild_filter(&mut ui, &playlist);
        let (found, cursor) = (ui.filtered_indices.clone(), ui.cursor);
        assert!(!found.is_empty());
        ui.hits = vec![(Rect::new(60, 0, 40, 20), Hit::Panel(ViewMode::Playlist))];
        // A click on the player: the keys go back to it, and keys typed
        // after it act there, not in the search out of focus.
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 5, 5));
        assert_eq!(ui.view_mode, ViewMode::Player);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char(' ')]);
        assert!(state.is_paused(), "space paused the player");
        // The search is as it was left: query, matches, cursor.
        assert!(matches!(&ui.input_mode, InputMode::Search(t) if t.text() == "3"), "the search kept");
        assert_eq!((ui.filtered_indices.clone(), ui.cursor), (found, cursor));
        // Back in the playlist, the field has the keys again: Esc goes to
        // it (and closes it), not to the panel. (The test's keys arrive as
        // clicks, which close a field that has the keys; Esc is the field's.)
        open_view(&mut ui, ViewMode::Playlist);
        ui.pressed.push_back(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        poll_input(&state, &mut ui, &mut playlist);
        assert!(matches!(ui.input_mode, InputMode::Normal), "the field took the Esc");
        assert_eq!(ui.view_mode, ViewMode::Playlist, "and the panel kept the keys");
    }

    #[test]
    fn clicking_follow_in_the_lyrics_does_not_submit_the_playlists_search() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..3).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(3);
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics("[00:01.00]a\n[00:02.00]b")));
        ui.lyrics_auto_scroll = false;
        // Both docked; the playlist has the keys and a search open.
        ui.docked = vec![ViewMode::Playlist, ViewMode::Lyrics];
        ui.docked_shown = ui.docked.clone();
        ui.view_mode = ViewMode::Playlist;
        // A query that matches: submitted, it plays that track.
        // (Typed: the test's `press` queues keys as clicks, which close a
        // field.)
        ui.input_mode = InputMode::Search(TextInput::with("1"));
        rebuild_filter(&mut ui, &playlist);
        assert!(!ui.filtered_indices.is_empty(), "the query matches a track");
        // The lyrics' footer: the panel, and its `↵ follow` hint.
        ui.hits = vec![
            (Rect::new(60, 0, 40, 20), Hit::Panel(ViewMode::Lyrics)),
            (Rect::new(62, 18, 8, 1), Hit::Key(KeyCode::Enter, KeyModifiers::NONE)),
        ];
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 63, 18));
        press(&state, &mut ui, &mut playlist, &[]);
        assert_eq!(ui.view_mode, ViewMode::Lyrics);
        assert!(matches!(&ui.input_mode, InputMode::Search(t) if t.text() == "1"), "the search kept, not submitted");
        assert!(ui.lyrics_auto_scroll, "the Enter went to the lyrics: following again");
        assert_eq!(ui.current, 0, "and played nothing");
    }

    #[test]
    fn undocking_on_the_too_small_screen_takes_it_off_the_record() {
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..3).map(|i| p(&format!("/{i}.flac"))).collect();
        for key in [KeyCode::Char('k'), KeyCode::Char('K')] {
            let mut ui = test_ui(3);
            ui.docked = vec![ViewMode::Playlist];
            ui.view_mode = ViewMode::Playlist;
            frame(&state, &mut ui, &playlist, 120, 50);
            assert_eq!(ui.docked_shown, [ViewMode::Playlist]);
            ui.too_small = true;
            frame(&state, &mut ui, &playlist, 30, 8);
            press(&state, &mut ui, &mut playlist, &[key]);
            frame(&state, &mut ui, &playlist, 30, 8);
            assert!(ui.docked_shown.is_empty(), "{key:?}: no longer docked, not recorded as docked");
        }
    }

    #[test]
    fn a_key_release_marks_no_frame() {
        // Windows reports a release for every key: each keystroke drew two
        // frames.
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.dirty = false;
        let mut release = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        ui.deferred_event = Some(Event::Key(release));
        poll_input(&state, &mut ui, &mut playlist);
        assert!(!ui.dirty);
    }

    #[test]
    fn input_keet_holds_is_waiting_input() {
        let mut ui = test_ui(1);
        assert!(!input_waiting(&ui));
        ui.deferred_event = Some(Event::Paste("/x".into()));
        assert!(input_waiting(&ui), "the event kept after an Esc");
        ui.deferred_event = None;
        ui.pressed.push_back(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(input_waiting(&ui), "a clicked hint's key");
    }

    #[test]
    fn what_changes_the_screen_in_the_background_asks_for_a_frame() {
        // Without input, the frame loop draws at the idle rate (250 ms): a
        // track change, a swapped playlist or a status from a worker showed
        // that late unless it set `ui.dirty`.
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..3).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(3);
        ui.dirty = false;
        advance_to(&mut ui, &state, 1);
        assert!(ui.dirty, "a track change");
        ui.dirty = false;
        let old = playlist.clone();
        reindex_and_restart_scan(&mut ui, &playlist, &old);
        assert!(ui.dirty, "a playlist swap (rescan, sort, repeat-all rebuild)");
        ui.dirty = false;
        ui.set_status("Sorted by tags".into());
        assert!(ui.dirty, "a status");
        ui.dirty = false;
        ui.queue_notice("output moved".into(), std::time::Duration::from_secs(4));
        assert!(ui.dirty, "a notice");
    }

    #[test]
    fn notices_wait_while_the_window_is_too_small() {
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.set_status_for("first".into(), std::time::Duration::from_secs(10));
        ui.queue_notice("must be read".into(), std::time::Duration::from_secs(5));
        ui.status_hold = std::time::Duration::ZERO; // the first is over
        ui.too_small = true;
        frame(&state, &mut ui, &playlist, 30, 8);
        assert_eq!(ui.notice_queue.len(), 1, "not taken (and timed) by a screen that cannot show it");
        ui.too_small = false;
        let rows = tui::testing::rows(&frame(&state, &mut ui, &playlist, 100, 30));
        assert!(rows.iter().any(|r| r.contains("must be read")), "{rows:#?}");
    }

    #[test]
    fn a_status_line_clamped_to_nothing_draws_nothing() {
        let mut ui = test_ui(1);
        ui.frame_status = Some("saved".to_string());
        let p = crate::theme::palette(crate::theme::ThemeKind::Classic);
        // A sub-area's last row asked one past it: `tui::rows` gives a row
        // of no height at its bottom, inside the window.
        let buf = tui::testing::draw(40, 10, |f| {
            let row = tui::rows(Rect::new(0, 0, 40, 5), 5, 1);
            assert_eq!((row.y, row.height), (5, 0));
            draw_status_line(f, &mut ui, row, p);
        });
        assert!(!tui::testing::rows(&buf).iter().any(|r| r.contains("saved")));
        assert_eq!(ui.status_drawn_at, None);
    }

    #[test]
    fn a_status_message_shows_while_a_search_is_typed() {
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..30).map(|i| p(&format!("/{i:03}.flac"))).collect();
        let mut ui = test_ui(30);
        // The floating playlist covers the player's status line; a search is
        // being typed when a notice comes.
        ui.view_mode = ViewMode::Playlist;
        ui.input_mode = InputMode::Search(TextInput::with("abc"));
        ui.set_status("output moved to DAC".to_string());
        let rows = tui::testing::rows(&frame(&state, &mut ui, &playlist, 100, 40));
        let n: usize = rows.iter().map(|r| r.matches("output moved to DAC").count()).sum();
        assert_eq!(n, 1, "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("/ abc")), "the field still shows: {rows:#?}");
    }

    #[test]
    fn a_long_status_is_cut_to_its_row_with_an_ellipsis() {
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        for kind in [crate::theme::ThemeKind::Classic, crate::theme::ThemeKind::Minimal, crate::theme::ThemeKind::HiFi] {
            state.set_theme(kind);
            let mut ui = test_ui(1);
            ui.set_status("output moved to a device with a very long name, exclusive mode off".to_string());
            let rows = tui::testing::rows(&frame(&state, &mut ui, &playlist, 50, 30));
            let row = rows.iter().find(|r| r.contains("output moved")).unwrap_or_else(|| panic!("{kind:?}: {rows:#?}"));
            assert!(row.trim_end().ends_with('…') && tui::width(row) <= 50, "{kind:?}: {row:?}");
        }
    }

    #[test]
    fn a_status_message_shows_once_whatever_the_window_width() {
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..30).map(|i| p(&format!("/{i:03}.flac"))).collect();
        for kind in [crate::theme::ThemeKind::Classic, crate::theme::ThemeKind::Minimal, crate::theme::ThemeKind::HiFi] {
            state.set_theme(kind);
            for view in [ViewMode::Lyrics, ViewMode::Playlist] {
                // Narrow: the floating box covers the player's status line.
                // Wide: the lyrics (90 at most) and the list (160) leave it in
                // view beside them.
                // Short too: HiFi's status row (21st) is then off the screen.
                for (w, h) in [60u16, 100, 140, 200, 220].into_iter().flat_map(|w| [(w, 40u16), (w, 15)]) {
                    let mut ui = test_ui(30);
                    ui.view_mode = view;
                    ui.focus = view;
                    ui.set_status("saved to the playlist".to_string());
                    let rows = tui::testing::rows(&frame(&state, &mut ui, &playlist, w, h));
                    let n: usize = rows.iter().map(|r| r.matches("saved to the playlist").count()).sum();
                    assert_eq!(n, 1, "{kind:?} {view:?} at {w}x{h}: {rows:#?}");
                }
            }
        }
    }

    #[test]
    fn scrolling_lyrics_that_cannot_move_keeps_them_following_the_song() {
        let state = PlayerState::new();
        let mut playlist = vec![p("/m/a.flac")];
        let mut ui = test_ui(1);
        // Ten synced lines in a 20-row panel: nothing to scroll.
        let lrc: String = (0..10).map(|i| format!("[00:{i:02}.00]line {i}\n")).collect();
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&lrc)));
        ui.view_mode = ViewMode::Lyrics;
        ui.focus = ViewMode::Lyrics;
        let area = Rect::new(0, 0, 60, 22);
        lyrics_panel(&state, &mut ui, area);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s'), KeyCode::Char('w')]);
        ui.scroll_lyrics(WHEEL_ROWS as isize);
        lyrics_panel(&state, &mut ui, area);
        assert!(ui.lyrics_auto_scroll, "nothing moved: still following the song");
        // In a panel too short for them, a step that moves stops following.
        let small = Rect::new(0, 0, 60, 6);
        lyrics_panel(&state, &mut ui, small);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s')]);
        lyrics_panel(&state, &mut ui, small);
        assert!(!ui.lyrics_auto_scroll);
    }

    #[test]
    fn enter_brings_scrolled_lyrics_back_to_the_song() {
        let state = PlayerState::new();
        state.output_rate.store(48_000, Ordering::Relaxed);
        state.samples_played.store(48_000 * 50 + 24_000, Ordering::Relaxed); // line 50
        let mut playlist = vec![p("/m/a.flac")];
        let mut ui = test_ui(1);
        let lrc: String = (0..100).map(|i| format!("[{:02}:{:02}.00]line {i:02}\n", i / 60, i % 60)).collect();
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&lrc)));
        // Docked, focused, scrolled away from the song.
        ui.docked = vec![ViewMode::Lyrics];
        ui.view_mode = ViewMode::Lyrics;
        ui.focus = ViewMode::Lyrics;
        let area = Rect::new(0, 0, 60, 21);
        let following = lyrics_panel(&state, &mut ui, area);
        let follow_at = ui.lyrics_scroll;
        assert!(!following.iter().any(|r| r.contains("follow")), "no hint while following");
        for _ in 0..20 {
            press(&state, &mut ui, &mut playlist, &[KeyCode::Char('w')]);
            lyrics_panel(&state, &mut ui, area);
        }
        assert!(!ui.lyrics_auto_scroll);
        let rows = lyrics_panel(&state, &mut ui, area);
        assert!(rows.iter().any(|r| r.contains("↵ follow")), "the way back offered: {rows:#?}");
        // The frame that stops following shows the hint itself (it used to
        // come a frame later, and frames drawn while waiting for a rate
        // change kept the stale footer).
        press(&state, &mut ui, &mut playlist, &[KeyCode::Enter]);
        lyrics_panel(&state, &mut ui, area);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('w')]);
        let rows = lyrics_panel(&state, &mut ui, area);
        assert!(!ui.lyrics_auto_scroll);
        assert!(rows.iter().any(|r| r.contains("↵ follow")), "in the same frame: {rows:#?}");
        // Enter: following again, on the current line.
        press(&state, &mut ui, &mut playlist, &[KeyCode::Enter]);
        lyrics_panel(&state, &mut ui, area);
        assert!(ui.lyrics_auto_scroll);
        assert_eq!(ui.lyrics_scroll, follow_at);
        // Before the first line (none is current yet), following shows the
        // top, not wherever the panel was scrolled.
        state.samples_played.store(0, Ordering::Relaxed);
        let late: String = (0..100).map(|i| format!("[{:02}:{:02}.00]line {i:02}\n", (i + 20) / 60, (i + 20) % 60)).collect();
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&late)));
        for _ in 0..30 {
            press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s')]);
            lyrics_panel(&state, &mut ui, area);
        }
        assert!(ui.lyrics_scroll > 0);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Enter]);
        lyrics_panel(&state, &mut ui, area);
        assert_eq!(ui.lyrics_scroll, 0, "following before the first line: the top");
        // Plain lyrics have no song position to follow: no hint.
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&"line\n".repeat(100))));
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s')]);
        let rows = lyrics_panel(&state, &mut ui, area);
        assert!(!rows.iter().any(|r| r.contains("follow")), "{rows:#?}");
    }

    #[test]
    fn the_last_lyric_row_is_reachable_whatever_the_footer_takes() {
        let state = PlayerState::new();
        state.output_rate.store(48_000, Ordering::Relaxed);
        state.samples_played.store(48_000 * 10, Ordering::Relaxed);
        let mut playlist = vec![p("/m/a.flac")];
        let lrc: String = (0..100).map(|i| format!("[{:02}:{:02}.00]L{i:02}\n", i / 60, i % 60)).collect();
        // Narrow panels, where `↵ follow` can wrap the footer onto another
        // row once following stops.
        for w in 20..60u16 {
            let mut ui = test_ui(1);
            ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&lrc)));
            ui.docked_shown = vec![ViewMode::Lyrics];
            ui.view_mode = ViewMode::Lyrics;
            ui.focus = ViewMode::Lyrics;
            let area = Rect::new(0, 0, w, 16);
            lyrics_panel(&state, &mut ui, area);
            let mut rows = Vec::new();
            for _ in 0..120 {
                press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s')]);
                rows = lyrics_panel(&state, &mut ui, area);
            }
            assert!(rows.iter().any(|r| r.contains("L99")), "at {w}: the last row never shows: {rows:#?}");
        }
    }

    #[test]
    fn s_scrolls_lyrics_to_their_last_row_and_no_further() {
        let state = PlayerState::new();
        let mut ui = test_ui(1);
        let mut playlist = vec![p("/m/a.flac")];
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&"line\n".repeat(100))));
        // y opens the panel and s scrolls, both before a frame: the press
        // counts (a limit from the last frame's layout used to swallow it).
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y'), KeyCode::Char('s')]);
        let area = Rect::new(0, 0, 60, 21); // one footer row, 20 of lyrics
        lyrics_panel(&state, &mut ui, area);
        assert_eq!(ui.lyrics_scroll, 1);
        // Held down, a frame between presses: it stops at the last row.
        for _ in 0..150 {
            press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s')]);
            lyrics_panel(&state, &mut ui, area);
        }
        assert_eq!(ui.lyrics_scroll, 100 - 20);
        // A panel too small to draw leaves it alone.
        lyrics_panel(&state, &mut ui, Rect::new(0, 0, 3, 1));
        assert_eq!(ui.lyrics_scroll, 80);
        // No lyrics, nothing to scroll.
        ui.set_lyrics(None);
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('s')]);
        assert_eq!(ui.lyrics_scroll, 0);
    }

    #[test]
    fn scrolling_synced_lyrics_right_after_opening_steps_from_the_current_line() {
        let state = PlayerState::new();
        state.output_rate.store(48_000, Ordering::Relaxed);
        state.samples_played.store(48_000 * 50 + 24_000, Ordering::Relaxed); // 50.5 s: line 50
        let mut ui = test_ui(1);
        let mut playlist = vec![p("/m/a.flac")];
        let lrc: String = (0..100).map(|i| format!("[{:02}:{:02}.00]line {i:02}\n", i / 60, i % 60)).collect();
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&lrc)));
        let area = Rect::new(0, 0, 60, 21); // one footer row, 20 of lyrics
        // Following the song, line 50 sits in the middle: row 40 on top.
        for (keys, top) in [(&[KeyCode::Char('s')][..], 41), (&[KeyCode::Char('w')][..], 39)] {
            ui.view_mode = ViewMode::Player;
            ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&lrc)));
            // y opens it, then w or s before any frame has drawn it.
            press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y')]);
            press(&state, &mut ui, &mut playlist, keys);
            let rows = lyrics_panel(&state, &mut ui, area);
            assert_eq!(ui.lyrics_scroll, top, "{keys:?}: {rows:#?}");
            assert!(!ui.lyrics_auto_scroll, "{keys:?} stops following the song");
        }
        // The wheel too.
        ui.view_mode = ViewMode::Player;
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&lrc)));
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y')]);
        frame(&state, &mut ui, &playlist, 100, 40);
        let top = ui.lyrics_scroll;
        ui.set_lyrics(Some(crate::lyrics::parse_lyrics(&lrc)));
        ui.view_mode = ViewMode::Player;
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('y')]);
        // The floating panel is centred in the 100×40 window.
        mouse(&state, &mut ui, &playlist, click(event::MouseEventKind::ScrollDown, 50, 20));
        frame(&state, &mut ui, &playlist, 100, 40);
        assert_eq!(ui.lyrics_scroll, top + WHEEL_ROWS);
    }

    #[test]
    fn a_hidden_docked_panel_opens_set_up_and_one_on_screen_does_not() {
        let state = PlayerState::new();
        let mut ui = test_ui(5);
        let mut playlist: Vec<PathBuf> = (0..5).map(|i| p(&format!("/{i}.flac"))).collect();
        ui.current = 3;
        ui.cursor = 0;
        // Docked, but the last frame had no room for it: it opens floating,
        // with the cursor on the playing track.
        ui.docked = vec![ViewMode::Playlist];
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('l')]);
        assert_eq!(ui.cursor, 3);
    }

    #[test]
    fn a_click_in_a_floating_panel_over_a_docked_one_is_the_floating_ones() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..20).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(20);
        ui.docked = vec![ViewMode::Lyrics];
        ui.docked_shown = vec![ViewMode::Lyrics];
        ui.view_mode = ViewMode::Playlist; // floating over the docked lyrics
        ui.hits.push((Rect::new(0, 10, 100, 20), Hit::Panel(ViewMode::Lyrics)));
        ui.hits.push((Rect::new(10, 2, 80, 30), Hit::Dialog));
        ui.hits.push((Rect::new(12, 4, 70, 20), Hit::List { first: 0 }));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 20, 15));
        assert_eq!(ui.view_mode, ViewMode::Playlist, "the playlist keeps the keys");
        assert_eq!(ui.cursor, 11, "and the click moved its cursor");
    }

    #[test]
    fn esc_in_the_footer_says_what_esc_does() {
        // The tree's Esc clears a filter first: "Esc player" was wrong then.
        let mut ui = test_ui(1);
        ui.docked_shown = vec![ViewMode::Playlist];
        ui.focus = ViewMode::Playlist;
        ui.library_tree_mode = true;
        ui.tree_filter = "beat".into();
        let esc = |ui: &UiState| panel_footer_keys(ui, ViewMode::Playlist, false).into_iter().find(|(k, _, _)| *k == "Esc").map(|(_, l, _)| l);
        assert_eq!(esc(&ui).as_deref(), Some("clear filter"));
        ui.tree_filter.clear();
        assert_eq!(esc(&ui).as_deref(), Some("player"));
    }

    #[test]
    fn the_open_dialogs_keys_are_clickable() {
        // Written by hand, its "Enter open · Esc cancel" registered nothing.
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.input_mode = InputMode::OpenPath(TextInput::with("~/Music"));
        let rows = tui::testing::rows(&frame(&state, &mut ui, &playlist, 100, 30));
        for key in [KeyCode::Enter, KeyCode::Esc] {
            assert!(ui.hits.iter().any(|(_, h)| *h == Hit::Key(key, KeyModifiers::NONE)), "{key:?} clickable");
        }
        assert!(rows.iter().any(|r| r.contains("drop a folder here")), "{rows:#?}");
    }

    #[test]
    fn the_key_list_in_a_window_too_narrow_for_it_does_not_panic() {
        // A fast drag can draw a frame narrower than the too-small screen's
        // check saw: the key list's layout came back empty, and `[1..]` of it
        // panicked.
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.view_mode = ViewMode::Help;
        for w in 1..12 {
            frame(&state, &mut ui, &playlist, w, 20);
        }
    }

    #[test]
    fn the_tree_refreshes_under_the_key_list_and_the_eq_editor() {
        let playlist: Vec<PathBuf> = (0..4).map(|i| p(&format!("/A{i}/t.flac"))).collect();
        for (over, back) in [(ViewMode::Eq, "eq"), (ViewMode::Help, "help")] {
            let mut ui = test_ui(4);
            // The floating playlist, the EQ editor (or the key list) over it.
            ui.eq_return = ViewMode::Playlist;
            ui.help_return = ViewMode::Playlist;
            ui.view_mode = over;
            poll_library_tree(&mut ui, &playlist);
            assert!(!ui.tree_rows.is_empty(), "under the {back} dialog, the tree pane is built");
        }
    }

    #[test]
    fn a_player_key_hint_acts_on_the_player_not_the_focused_panel() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let mut playlist: Vec<PathBuf> = (0..5).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(5);
        // The docked playlist has the keys; the player's `↑↓ track` hint.
        ui.docked = vec![ViewMode::Playlist];
        ui.docked_shown = vec![ViewMode::Playlist];
        ui.view_mode = ViewMode::Playlist;
        ui.cursor = 2;
        ui.hits = vec![
            (Rect::new(60, 0, 40, 20), Hit::Panel(ViewMode::Playlist)),
            (Rect::new(2, 28, 1, 1), Hit::Key(KeyCode::Up, KeyModifiers::NONE)),
        ];
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 2, 28));
        assert_eq!(ui.view_mode, ViewMode::Player, "the player takes the keys back");
        press(&state, &mut ui, &mut playlist, &[]);
        assert_eq!(ui.cursor, 2, "the list cursor did not move");
    }

    #[test]
    fn a_click_inside_a_dialog_leaves_a_focused_docked_panel_focused() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        // Docked lyrics with the keys; "Quit?" floats over them.
        ui.docked = vec![ViewMode::Lyrics];
        ui.docked_shown = vec![ViewMode::Lyrics];
        ui.view_mode = ViewMode::Lyrics;
        ui.esc_quit_until = Some(std::time::Instant::now() + ESC_QUIT_WINDOW);
        ui.hits.push((Rect::new(0, 10, 100, 20), Hit::Panel(ViewMode::Lyrics)));
        ui.hits.push((Rect::new(20, 12, 40, 6), Hit::Dialog));
        // A click inside the dialog, on no key: nothing changes hands.
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 30, 14));
        assert_eq!(ui.view_mode, ViewMode::Lyrics, "the lyrics keep the keys");
        assert!(quit_armed(&ui), "and the dialog stays");
        // The same with the dialog's the only hit (nothing of the player's
        // registered before it).
        ui.hits = vec![(Rect::new(20, 12, 40, 6), Hit::Dialog)];
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 30, 14));
        assert_eq!(ui.view_mode, ViewMode::Lyrics, "the dialog's hit first: still the dialog's click");
        ui.hits = vec![(Rect::new(0, 10, 100, 20), Hit::Panel(ViewMode::Lyrics))];
        // On the too-small screen there is nothing to click: the keys stay.
        ui.esc_quit_until = None;
        ui.too_small = true;
        ui.hits.clear();
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 5, 2));
        assert_eq!(ui.view_mode, ViewMode::Lyrics, "a click on the too-small screen");
        ui.too_small = false;
        ui.hits = vec![(Rect::new(0, 10, 100, 20), Hit::Panel(ViewMode::Lyrics))];
        // Without a dialog, a click in the player still takes them back.
        ui.esc_quit_until = None;
        ui.hits.truncate(1);
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 5, 2));
        assert_eq!(ui.view_mode, ViewMode::Player);
    }

    #[test]
    fn the_k_hint_says_what_k_does() {
        let state = PlayerState::new();
        let mut playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        let k_label = |ui: &UiState| {
            panel_footer_keys(ui, ViewMode::Lyrics, false).into_iter().find(|(k, _, _)| *k == "k").map(|(_, l, _)| l)
        };
        // Floating, not docked: k docks it.
        ui.view_mode = ViewMode::Lyrics;
        assert_eq!(k_label(&ui).as_deref(), Some("dock"));
        // Docked, but floating for want of room: k takes it out of the dock.
        ui.docked = vec![ViewMode::Lyrics];
        assert_eq!(k_label(&ui).as_deref(), Some("undock"));
        press(&state, &mut ui, &mut playlist, &[KeyCode::Char('k')]);
        assert!(ui.docked.is_empty(), "and k did that");
        // Docked on screen: k floats it.
        ui.docked = vec![ViewMode::Lyrics];
        ui.docked_shown = vec![ViewMode::Lyrics];
        assert_eq!(k_label(&ui).as_deref(), Some("float"));
    }

    #[test]
    fn only_the_top_dialog_takes_clicks() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..20).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(20);
        // The EQ editor over the floating playlist.
        ui.eq_return = ViewMode::Playlist;
        ui.view_mode = ViewMode::Eq;
        ui.hits.push((Rect::new(0, 0, 100, 40), Hit::Dialog));
        ui.hits.push((Rect::new(2, 2, 90, 30), Hit::List { first: 0 }));
        ui.hits.push((Rect::new(30, 10, 40, 10), Hit::Dialog));
        // On a playlist row outside the EQ box: closes the EQ, moves nothing.
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 5, 5));
        assert_eq!(ui.view_mode, ViewMode::Playlist, "the EQ editor closed");
        assert_eq!(ui.cursor, 0, "the row under it did not take the click");
        // Inside the EQ box over a playlist row: the row does not react.
        ui.view_mode = ViewMode::Eq;
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 35, 15));
        assert_eq!(ui.view_mode, ViewMode::Eq);
        assert_eq!(ui.cursor, 0);
    }

    #[test]
    fn a_key_hint_under_an_open_dialog_closes_the_dialog_instead() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        // "Quit?" is up; the player's `q` hint shows beside it.
        ui.esc_quit_until = Some(std::time::Instant::now() + ESC_QUIT_WINDOW);
        ui.hits.push((Rect::new(2, 30, 1, 1), Hit::Key(KeyCode::Char('q'), KeyModifiers::NONE)));
        ui.hits.push((Rect::new(20, 10, 30, 5), Hit::Dialog));
        ui.hits.push((Rect::new(22, 13, 1, 1), Hit::Key(KeyCode::Char('y'), KeyModifiers::NONE)));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 2, 30));
        assert!(ui.pressed.is_empty(), "the q under the dialog must not quit");
        assert!(!overlay_open(&ui), "the click closed the dialog");
        // A hint inside a dialog still presses its key.
        ui.esc_quit_until = Some(std::time::Instant::now() + ESC_QUIT_WINDOW);
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 22, 13));
        assert_eq!(ui.pressed.pop_front().map(|k| k.code), Some(KeyCode::Char('y')));
    }

    #[test]
    fn a_click_on_the_progress_bar_seeks_there() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        state.sample_rate.store(48_000, Ordering::Relaxed);
        state.total_samples.store(48_000 * 100, Ordering::Relaxed);
        let mut ui = test_ui(1);
        let playlist = vec![p("/a.flac")];
        ui.hits.push((Rect::new(10, 5, 101, 1), Hit::Seek));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 60, 5));
        assert!((state.take_seek() - 50.0).abs() < 0.01, "the middle of the bar is the middle of the track");
        mouse(&state, &mut ui, &playlist, click(K::Drag(MouseButton::Left), 110, 5));
        assert!((state.take_seek() - 100.0).abs() < 0.01, "dragged to the end");
        // Off the bar: nothing.
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 60, 6));
        assert_eq!(state.take_seek(), 0.0);
    }

    #[test]
    fn a_click_selects_a_row_and_a_double_click_plays_it() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..20).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(20);
        ui.view_mode = ViewMode::Playlist;
        // The floating panel's box, and its rows 4.. showing positions from 10.
        ui.hits.push((Rect::new(1, 2, 90, 20), Hit::Dialog));
        ui.hits.push((Rect::new(2, 4, 80, 8), Hit::List { first: 10 }));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 20, 6));
        assert_eq!(ui.cursor, 12);
        assert_eq!(state.take_jump(), None, "one click only selects");
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 30, 6));
        assert_eq!(state.take_jump(), Some(12), "a second click on the row plays it");
        // The wheel moves the cursor.
        mouse(&state, &mut ui, &playlist, click(K::ScrollDown, 20, 6));
        assert_eq!(ui.cursor, 12 + WHEEL_ROWS);
    }

    #[test]
    fn a_click_outside_a_dialog_closes_it_and_one_inside_does_not() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.view_mode = ViewMode::Lyrics;
        toggle_help(&mut ui);
        ui.hits.push((Rect::new(10, 5, 30, 10), Hit::Dialog));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 15, 8));
        assert_eq!(ui.view_mode, ViewMode::Help, "inside: stays");
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 2, 2));
        assert_eq!(ui.view_mode, ViewMode::Lyrics, "outside: closed back to where it opened");
    }


    #[test]
    fn a_click_on_a_tab_opens_its_view() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist: Vec<PathBuf> = (0..5).map(|i| p(&format!("/{i}.flac"))).collect();
        let mut ui = test_ui(5);
        ui.current = 3;
        ui.hits.push((Rect::new(60, 0, 7, 1), Hit::Tab(ViewMode::Playlist)));
        ui.hits.push((Rect::new(72, 0, 6, 1), Hit::Tab(ViewMode::Lyrics)));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 74, 0));
        assert_eq!(ui.view_mode, ViewMode::Lyrics);
        assert!(ui.lyrics_auto_scroll);
        ui.view_mode = ViewMode::Player;
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 62, 0));
        assert_eq!(ui.view_mode, ViewMode::Playlist);
        assert_eq!(ui.cursor, 3, "the list opens on the playing track, as `l` does");
        // The playlist floats: a click outside its box (here, on the tabs
        // under it) closes it.
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 74, 0));
        assert_eq!(ui.view_mode, ViewMode::Player);
    }


    #[test]
    fn a_click_on_a_key_hint_presses_that_key_even_in_a_dialog() {
        use event::{MouseButton, MouseEventKind as K};
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let mut ui = test_ui(1);
        ui.hits.push((Rect::new(4, 30, 1, 1), Hit::Key(KeyCode::Char('l'), KeyModifiers::NONE)));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 4, 30));
        assert_eq!(ui.pressed.pop_front().map(|k| k.code), Some(KeyCode::Char('l')));
        // Inside an open dialog its own hints press too (they come after the
        // dialog's box in `hits`, so they are found first).
        ui.esc_quit_until = Some(std::time::Instant::now() + ESC_QUIT_WINDOW);
        ui.hits.push((Rect::new(10, 10, 30, 5), Hit::Dialog));
        ui.hits.push((Rect::new(12, 12, 3, 1), Hit::Key(KeyCode::Esc, KeyModifiers::NONE)));
        mouse(&state, &mut ui, &playlist, click(K::Down(MouseButton::Left), 13, 12));
        assert_eq!(ui.pressed.pop_front().map(|k| k.code), Some(KeyCode::Esc));
    }

    #[test]
    fn every_theme_registers_its_key_bar_where_it_draws_it() {
        use crate::theme::ThemeKind;
        let state = PlayerState::new();
        let playlist = vec![p("/a.flac")];
        let eq = crate::eq::builtin_presets().remove(0);
        let mut stats = StatsMonitor::new();
        let analyser = VizAnalyser::new(48_000);
        for kind in [ThemeKind::Classic, ThemeKind::Minimal, ThemeKind::HiFi] {
            state.set_theme(kind);
            let mut ui = UiState::new(playlist.clone(), crate::metadata::MetadataCache::new(1));
            let buf = tui::testing::draw(120, 40, |f| {
                let mut cx = Ctx {
                    state: &state, ui: &mut ui, name: "a", track_info: "", ext: "flac", eq_preset: &eq,
                    fx_name: "None", cf_name: "Off", stats: &mut stats, playlist: &playlist,
                    analyser: &analyser, repaint: true,
                };
                draw(f, &mut cx);
            });
            // The Space key's spot shows the ␣ glyph.
            let space = ui
                .hits
                .iter()
                .find(|(_, h)| *h == Hit::Key(KeyCode::Char(' '), KeyModifiers::NONE))
                .map(|(r, _)| *r)
                .unwrap_or_else(|| panic!("{kind:?}: no Space hint"));
            assert_eq!(buf[(space.x, space.y)].symbol(), "␣", "{kind:?} at {space:?}");
        }
    }


    #[test]
    fn the_playlist_floats_with_both_panes_when_there_is_room() {
        use crate::theme::ThemeKind;
        let state = PlayerState::new();
        state.set_theme(ThemeKind::Classic);
        let playlist: Vec<PathBuf> = (0..8).map(|i| p(&format!("/m/{i:02} song.flac"))).collect();
        let eq = crate::eq::builtin_presets().remove(0);
        let analyser = VizAnalyser::new(48_000);
        let screen = |w: u16, tree: bool| {
            let mut ui = UiState::new(playlist.clone(), crate::metadata::MetadataCache::new(playlist.len()));
            ui.view_mode = ViewMode::Playlist;
            ui.library_tree_mode = tree;
            let mut stats = StatsMonitor::new();
            let buf = tui::testing::draw(w, 30, |f| {
                let mut cx = Ctx {
                    state: &state, ui: &mut ui, name: "a", track_info: "", ext: "flac", eq_preset: &eq,
                    fx_name: "None", cf_name: "Off", stats: &mut stats, playlist: &playlist,
                    analyser: &analyser, repaint: true,
                };
                draw(f, &mut cx);
            });
            tui::testing::rows(&buf).join("\n")
        };
        let wide = screen(140, false);
        assert!(wide.contains("Playlist · 8 tracks") && wide.contains("Title") && wide.contains("Artist / album"), "{wide}");
        assert!(wide.contains("Tab tree pane"), "Tab moves between the panes: {wide}");
        let narrow = screen(100, false);
        assert!(narrow.contains("Title") && !narrow.contains("Artist / album"), "{narrow}");
        let narrow_tree = screen(100, true);
        assert!(narrow_tree.contains("Artist / album") && !narrow_tree.contains("Title  "), "{narrow_tree}");
    }


    #[test]
    fn docked_panels_share_the_row_by_priority() {
        let mut ui = test_ui(1);
        ui.docked = vec![ViewMode::Playlist, ViewMode::Lyrics];
        ui.panel_recent = vec![ViewMode::Lyrics, ViewMode::Playlist];
        let row = |w: u16| Rect::new(0, 10, w, 20);
        let kinds = |d: &Dock| {
            let mut v: Vec<&str> = Vec::new();
            if d.viz.is_some() {
                v.push("viz");
            }
            v.extend(d.panels.iter().map(|(p, _)| if *p == ViewMode::Lyrics { "lyrics" } else { "playlist" }));
            v
        };
        // Room for all: the visualization, then the lyrics, then the playlist,
        // filling the row.
        let wide = dock(&ui, true, row(200));
        assert_eq!(kinds(&wide), ["viz", "lyrics", "playlist"]);
        let right = wide.panels.last().unwrap().1.right();
        assert_eq!(right, 200);
        // Less: the panel used last keeps its room, then the visualization.
        assert_eq!(kinds(&dock(&ui, true, row(90))), ["viz", "lyrics"]);
        // A small window: docking replaces the visualization.
        assert_eq!(kinds(&dock(&ui, true, row(60))), ["lyrics"]);
        // The playlist used last instead: it is the one that stays.
        ui.panel_recent = vec![ViewMode::Playlist, ViewMode::Lyrics];
        assert_eq!(kinds(&dock(&ui, true, row(60))), ["playlist"]);
        // Too short for any panel: the visualization alone.
        assert_eq!(kinds(&dock(&ui, true, Rect::new(0, 0, 200, 4))), ["viz"]);
        // Too narrow for any panel (and for the visualization's own minimum):
        // the visualization keeps the slot rather than it going blank.
        assert_eq!(dock(&ui, true, row(35)).viz, Some(row(35)));
        // Nothing docked: the visualization alone, the whole row.
        ui.docked.clear();
        assert_eq!(dock(&ui, true, row(200)).viz, Some(row(200)));
    }

}
