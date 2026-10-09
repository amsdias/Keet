
use std::path::PathBuf;
use std::time::Duration;
use std::sync::atomic::Ordering;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;

use crate::state::{
    PlayerState, VizMode, RepeatMode,
    C_RESET, C_BOLD, C_DIM, C_CYAN, C_RED,
    ViewMode, InputMode, UiState,
};
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

use crate::ansi::{truncate_ansi, truncate_plain, visible_len};

/// Counts the frame lines actually emitted below the first (anchor) row, so
/// the caller's cursor-up math is derived from what was printed instead of
/// predicted by hand — predicted counts drifting from printed reality was a
/// recurring off-by-one source (the next frame's cursor-up then lands
/// mid-frame and the layout smears).
///
/// It also keeps the frame inside the window, for every theme at once: a line
/// wider than the window is cut (a wrapped line takes a row nobody counted,
/// which smears the layout), and rows that would fall below the bottom edge
/// are not emitted (a frame taller than the window scrolls the screen on every
/// redraw). Both used to be each renderer's own job, and several missed it.
pub(crate) struct FrameWriter {
    below_first: usize,
    /// Columns per line; None = unlimited.
    width: Option<usize>,
    /// Rows allowed below the anchor; None = unlimited.
    max_below: Option<usize>,
}

impl FrameWriter {
    /// Unbounded — for tests and anything not drawn into the terminal.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new() -> Self {
        Self { below_first: 0, width: None, max_below: None }
    }

    /// Bounded by the terminal window, with the anchor row at its top.
    pub(crate) fn fitted() -> Self {
        match terminal::size() {
            // A size of zero is "unknown" (some pseudo-terminals report it),
            // not a window nothing fits in.
            Ok((cols, rows)) if cols > 0 && rows > 0 => Self {
                below_first: 0,
                width: Some(cols as usize),
                max_below: Some((rows as usize).saturating_sub(1)),
            },
            _ => Self::new(),
        }
    }

    fn fit<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        match self.width {
            Some(w) if visible_len(s) > w => truncate_ansi(s, w).into(),
            _ => s.into(),
        }
    }

    fn room(&self) -> bool {
        self.max_below.is_none_or(|m| self.below_first < m)
    }

    /// Print the frame's first line in place (carriage return + erase): the
    /// anchor row the next frame's cursor-up returns to. Not counted.
    pub(crate) fn first_line(&mut self, s: &str) {
        crate::term::out!("\r\x1B[K{}", self.fit(s));
    }

    /// The anchor row WITHOUT the leading erase, for a row that starts with an
    /// image's cells (Classic's cover): erasing first would wipe a placed
    /// image on frames that only step past it. Callers end such a row with
    /// their own erase-to-EOL, which clears only what lies after the image.
    pub(crate) fn first_line_raw(&mut self, s: &str) {
        crate::term::out!("\r{}", s);
    }

    /// Advance one line and print with erase-to-EOL.
    pub(crate) fn line(&mut self, s: &str) {
        if self.room() {
            crate::term::out!("\n\r\x1B[K{}", self.fit(s));
            self.below_first += 1;
        }
    }

    /// Advance one line and print WITHOUT erase — sixel blocks erase
    /// themselves, and an EL here would wipe that row's slice of the image.
    /// Not cut to width: an image escape cannot be cut (its renderers size it).
    pub(crate) fn line_raw(&mut self, s: &str) {
        if self.room() {
            crate::term::out!("\n\r{}", s);
            self.below_first += 1;
        }
    }

    /// Lines emitted below the anchor row this frame.
    pub(crate) fn count(&self) -> usize {
        self.below_first
    }
}

/// What the full-window anchor line shows after the title (joined with the
/// same `•` the format uses inside itself, so every dot on the line matches),
/// most important
/// first (a narrow window drops from the end, see `ansi::fit_segments`): the
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
/// theme's list uses it — each used to carry its own copy.
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

/// Top-level renderer dispatcher. Routes by the active theme: each theme owns
/// its view renderers (`ui_classic`, `ui_minimal`, `ui_hifi`); the EQ editor
/// is one shared screen. Every renderer
/// honours the same contract — return the number of lines drawn below the
/// anchor row (line 1) so the caller's cursor-up math stays exact.
#[allow(clippy::too_many_arguments)] // cohesive render context; bundling into a struct adds no clarity
pub fn print_status(state: &PlayerState, ui: &mut UiState, name: &str, track_info: &str, ext: &str, eq_preset: &crate::eq::EqPreset, fx_name: &str, cf_name: &str, stats: &mut StatsMonitor, prev_frame_lines: usize, playlist: &[PathBuf], analyser: &VizAnalyser) -> usize {
    use crate::theme::ThemeKind;
    // Keep the live EQ bands mirroring the selected preset while not editing, so
    // the curve/editor show the active shape and edits start from it.
    if !state.is_eq_custom() {
        state.set_eq_bands(&eq_preset.bands_10());
        state.set_eq_preamp_db(eq_preset.preamp);
    }
    // Leaving the player view has to take the album cover with it.
    //
    // A Kitty image is an OVERLAY bound to an image id, not cell content, so
    // the lyrics/library/EQ screens drawing text over those cells does not
    // remove it — the cover just sits on top of them. (Sixel, iTerm2 and
    // half-block are cell content and get erased naturally.) The views that
    // draw a cover set `cover_block_intact`.
    //
    // Clearing the flag for every protocol is deliberate: it forces one repaint
    // when the player comes back, which Sixel and iTerm2 need because their
    // pixels really were overwritten while away.
    // Classic keeps its cover (in the header) on every view but the EQ editor.
    let cover_view = match state.theme_kind() {
        ThemeKind::Classic => matches!(ui.view_mode, ViewMode::Player | ViewMode::Playlist | ViewMode::Lyrics),
        ThemeKind::Minimal => ui.view_mode == ViewMode::Player,
        ThemeKind::HiFi => false,
    };
    if !cover_view && ui.cover_block_intact {
        if matches!(crate::cover::detect_protocol(), crate::cover::GraphicsProtocol::Kitty) {
            crate::term::out!("{}", crate::cover::kitty_clear_escape());
        }
        ui.cover_block_intact = false;
        ui.cover_dirty_frame = true;
    }

    // The EQ+FX editor is one shared, palette-driven screen across all themes.
    if ui.view_mode == ViewMode::Eq {
        return print_status_eq_view(state, ui, eq_preset, fx_name, cf_name, prev_frame_lines);
    }
    // The key list is one shared, palette-driven screen too.
    if ui.view_mode == ViewMode::Help {
        return print_status_help_view(state, prev_frame_lines);
    }
    if state.theme_kind() == ThemeKind::Minimal {
        match ui.view_mode {
            ViewMode::Player => {
                return crate::ui_minimal::print_status_minimal(state, ui, name, track_info, eq_preset, fx_name, cf_name, stats, prev_frame_lines, analyser);
            }
            ViewMode::Playlist => {
                return crate::ui_minimal::print_status_minimal_library(state, ui, name, prev_frame_lines, playlist);
            }
            ViewMode::Lyrics => {
                return crate::ui_minimal::print_status_minimal_lyrics(state, ui, name, prev_frame_lines);
            }
            ViewMode::Eq | ViewMode::Help => unreachable!("EQ and help views handled above"),
        }
    }
    if state.theme_kind() == ThemeKind::HiFi {
        match ui.view_mode {
            ViewMode::Player => {
                return crate::ui_hifi::print_status_hifi(state, ui, name, eq_preset, fx_name, cf_name, stats, prev_frame_lines);
            }
            ViewMode::Playlist => {
                return crate::ui_hifi::print_status_hifi_library(state, ui, name, prev_frame_lines, playlist);
            }
            ViewMode::Lyrics => {
                return crate::ui_hifi::print_status_hifi_lyrics(state, ui, name, prev_frame_lines);
            }
            ViewMode::Eq | ViewMode::Help => unreachable!("EQ and help views handled above"),
        }
    }
    crate::ui_classic::print_status_classic(state, ui, name, track_info, ext, eq_preset, fx_name, cf_name, stats, prev_frame_lines, playlist, analyser)
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
) -> Vec<String> {
    let rst = p.reset;
    let icon = if paused { "⏸" } else { "▶" };
    let rows: [(String, String); 7] = [
        ("▲ window too small".into(), p.warn.into()),
        (format!("{w}×{h} · Keet needs {}×{}", MIN_WINDOW.0, MIN_WINDOW.1), p.dim.into()),
        (String::new(), String::new()),
        (format!("{icon} {}", truncate_plain(title, w.saturating_sub(4))), p.fg.into()),
        (format!("{} / {}", format_time(time.0), format_time(time.1)), p.accent.into()),
        (String::new(), String::new()),
        ("space pause · ↑↓ track · q quit".into(), p.dim.into()),
    ];
    let top = h.saturating_sub(rows.len()) / 2;
    let mut out = vec![String::new(); top];
    for (text, colour) in rows {
        let text = truncate_plain(&text, w);
        let pad = w.saturating_sub(visible_len(&text)) / 2;
        out.push(format!("{}{colour}{text}{rst}", " ".repeat(pad)));
    }
    out.truncate(h.saturating_sub(1).max(1));
    out
}

/// Paint the too-small screen from the top of the window.
pub fn print_too_small(state: &PlayerState, ui: &UiState, name: &str) {
    let (w, h) = terminal::size().map(|(w, h)| (w as usize, h as usize)).unwrap_or((0, 0));
    let idx = state.current_track.load(Ordering::Relaxed);
    let title = ui.metadata_cache.title(idx).unwrap_or_else(|| name.to_string());
    let p = crate::theme::palette(state.theme_kind());
    let lines = too_small_lines(w, h, state.is_paused(), &title, (state.time_secs(), state.total_secs()), p);
    crate::term::out!("\x1B[H");
    for (i, line) in lines.iter().enumerate() {
        crate::term::out!("{}\r\x1B[K{line}", if i == 0 { "" } else { "\n" });
    }
    crate::term::out!("\x1B[J");
}

/// The EQ+FX editor screen — one shared renderer for all themes (palette-driven).
fn print_status_eq_view(
    state: &PlayerState,
    ui: &mut UiState,
    eq_preset: &crate::eq::EqPreset,
    fx_name: &str,
    cf_name: &str,
    prev_frame_lines: usize,
) -> usize {
    let kind = state.theme_kind();
    let p = crate::theme::palette(kind);
    let knob = match kind {
        crate::theme::ThemeKind::Minimal => '●',
        crate::theme::ThemeKind::HiFi => '◆',
        crate::theme::ThemeKind::Classic => '█',
    };
    let (term_w, term_h) = terminal::size()
        .map(|(w, h)| (w as usize, h as usize))
        .unwrap_or((120, 40));

    if prev_frame_lines != usize::MAX && prev_frame_lines > 0 {
        crate::term::out!("\x1B[{}F", prev_frame_lines);
    }

    let title = if state.is_eq_custom() {
        "Custom".to_string()
    } else {
        eq_preset.name.clone()
    };
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
    let readouts = vec![
        ("FX", fx_name),
        ("XFEED", cf_name),
        ("BAL", bal_str.as_str()),
        ("RG", rg_str),
    ];
    // The screen starts at the anchor row and keeps one row of slack below.
    let avail = term_h.saturating_sub(1);
    let bands = state.eq_bands_array();
    let mut body = crate::eq_ui::render_eq_screen(
        &bands,
        ui.eq_band,
        &title,
        &readouts,
        knob,
        p,
        term_w,
        avail.saturating_sub(2),
    );
    let out_rate = match state.output_rate.load(Ordering::Relaxed) {
        0 => 48_000.0,
        r => r as f32,
    };
    let headroom = crate::eq::headroom(&bands, pre_db, out_rate);
    body.push(crate::eq_ui::headroom_line(&headroom, pre_db, p));
    let footer_full = format!(
        "  {dim}[←→] band  [↑↓] gain (⇧ fine)  [t] type  [,.] Q  [<>] freq  [[]] preset  [0] reset  [e/Esc] close{rst}",
        dim = p.dim, rst = p.reset,
    );
    let footer_short = format!(
        "  {dim}←→ band ↑↓ gain t type ,. Q <> freq e close{rst}",
        dim = p.dim, rst = p.reset,
    );
    let lines = fit_eq_screen(body, &footer_full, &footer_short, term_w, avail);

    // First line is the anchor; the rest sit below it.
    let mut below = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            crate::term::out!("\r\x1B[K{}", line);
        } else {
            crate::term::out!("\n\r\x1B[K{}", line);
            below += 1;
        }
    }

    crate::term::out!("\x1B[J");
    crate::term::flush();
    below
}


/// The `?` screen: every key (`cli::KEYS`), sections flowed into as many
/// columns as the window holds, cut to its height.
pub(crate) fn help_lines(p: &crate::theme::Palette, term_w: usize, term_h: usize) -> Vec<String> {
    const KEY_W: usize = 12;
    const GAP: usize = 2;
    let rst = p.reset;
    let widest = crate::cli::KEYS
        .iter()
        .flat_map(|(_, keys)| keys.iter())
        .map(|(_, what)| KEY_W + 1 + visible_len(what))
        .max()
        .unwrap_or(40);
    // Each section as its lines: title, then one row per key.
    let sections: Vec<Vec<String>> = crate::cli::KEYS
        .iter()
        .map(|(title, keys)| {
            let mut v = vec![format!("{}{}{title}{rst}", p.accent, p.bold)];
            v.extend(keys.iter().map(|(k, what)| format!("{}{k:<KEY_W$}{rst} {}{what}{rst}", p.fg, p.dim)));
            v.push(String::new());
            v
        })
        .collect();
    let body_h = term_h.saturating_sub(3).max(1); // title row, footer, slack
    // As many columns as fit side by side, `GAP` apart.
    let cols = ((term_w.saturating_sub(2) + GAP) / (widest + GAP)).max(1);
    let col_w = widest + GAP;
    // Fill columns top to bottom, a section kept whole where it fits.
    let mut columns: Vec<Vec<String>> = vec![Vec::new()];
    for sec in sections {
        let last = columns.last_mut().expect("one column");
        if !last.is_empty() && last.len() + sec.len() > body_h && columns.len() < cols {
            columns.push(Vec::new());
        }
        columns.last_mut().expect("one column").extend(sec);
    }
    // Say so when the window cuts the list short, rather than hiding keys.
    let cut = columns.iter().any(|c| c.iter().skip(body_h).any(|l| !l.is_empty()));
    let more = if cut { "  ·  widen the window for the rest" } else { "" };
    let title = format!("  {}{}K E Y S{rst}   {}? or Esc closes{more}{rst}", p.accent, p.bold, p.dim);
    let mut out = vec![truncate_ansi(&title, term_w)];
    for row in 0..body_h {
        let mut line = String::from("  ");
        for (c, col) in columns.iter().enumerate() {
            let cell = col.get(row).map(String::as_str).unwrap_or("");
            line.push_str(cell);
            if c + 1 < columns.len() {
                let pad = col_w.saturating_sub(visible_len(cell));
                line.push_str(&" ".repeat(pad));
            }
        }
        out.push(truncate_ansi(line.trim_end(), term_w));
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out
}

fn print_status_help_view(state: &PlayerState, prev_frame_lines: usize) -> usize {
    let p = crate::theme::palette(state.theme_kind());
    let (term_w, term_h) = terminal::size().map(|(w, h)| (w as usize, h as usize)).unwrap_or((120, 40));
    if prev_frame_lines != usize::MAX && prev_frame_lines > 0 {
        crate::term::out!("\x1B[{}F", prev_frame_lines);
    }
    let mut w = FrameWriter::fitted();
    for (i, line) in help_lines(p, term_w, term_h).iter().enumerate() {
        if i == 0 { w.first_line(line) } else { w.line(line) }
    }
    crate::term::out!("\x1B[J");
    crate::term::flush();
    w.count()
}

/// Fit the EQ editor into `avail` rows of `term_w` columns: every line cut to
/// the width (the 99-column footer wrapped on an 80-column terminal, the frame
/// landed a row low every frame and then scrolled the screen at 20 fps), the
/// body cut to leave room for the footer, and the short footer used when the
/// full one would not fit.
fn fit_eq_screen(
    body: Vec<String>,
    footer_full: &str,
    footer_short: &str,
    term_w: usize,
    avail: usize,
) -> Vec<String> {
    if avail == 0 {
        return Vec::new();
    }
    let footer = if visible_len(footer_full) <= term_w { footer_full } else { footer_short };
    let mut out: Vec<String> = body
        .into_iter()
        .take(avail - 1)
        .map(|l| truncate_ansi(&l, term_w))
        .collect();
    out.push(truncate_ansi(footer, term_w));
    out
}
/// How long a first Esc in the player waits for the second that quits.
const ESC_QUIT_WINDOW: Duration = Duration::from_secs(2);

/// Open the key list, or close it back to the view it was opened from (it
/// always went back to the player). Opening it answers a pending "remove …?
/// [y/n]" prompt with no — left armed, a `y` long after the prompt had gone
/// removed a whole artist.
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
    ui.terminal_resized = true;
}

/// Whether an Esc at `now` is the confirming second press.
fn esc_confirms_quit(armed_until: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    armed_until.is_some_and(|t| now < t)
}

/// Whether the event right behind a bare Esc means the Esc was the first
/// byte of an escape sequence (macOS Cmd+Arrow arrives as ESC + another key
/// press), rather than a real Esc tap.
fn esc_starts_sequence(next: &Event) -> bool {
    matches!(next, Event::Key(k) if k.kind == KeyEventKind::Press)
}

pub fn poll_input(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) -> bool {
    // Drain all pending events for responsive input
    while event::poll(Duration::ZERO).unwrap_or(false) {
        let ev = match event::read() { Ok(e) => e, Err(_) => continue };

        if let Event::Resize(_, _) = ev {
            ui.terminal_resized = true;
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
            && k.modifiers.is_empty()
            && event::poll(Duration::ZERO).unwrap_or(false)
        {
            match event::read() {
                Ok(next) if esc_starts_sequence(&next) => continue,
                Ok(Event::Resize(_, _)) => ui.terminal_resized = true,
                _ => {}
            }
        }

            // In text input mode, route to text handler
            match &ui.input_mode {
                InputMode::Search(_) | InputMode::SavePlaylist(_) => {
                    return handle_text_input(state, ui, playlist, k);
                }
                InputMode::Normal => {}
            }

            // Lyrics view keys (when in Normal input mode)
            if ui.view_mode == ViewMode::Lyrics {
                match k {
                    KeyEvent { code: KeyCode::Char('w'), .. } => {
                        ui.lyrics_auto_scroll = false;
                        ui.lyrics_scroll = ui.lyrics_scroll.saturating_sub(1);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('s'), .. } => {
                        ui.lyrics_auto_scroll = false;
                        if let Some(ref lyrics) = ui.lyrics {
                            let max = lyrics.line_count().saturating_sub(1);
                            if ui.lyrics_scroll < max {
                                ui.lyrics_scroll += 1;
                            }
                        }
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
                        ui.view_mode = ViewMode::Player;
                        continue;
                    }
                    _ => {} // Fall through to global keys (E/L close, q, space, …)
                }
            }

            // Playlist view keys (when in Normal input mode)
            if ui.view_mode == ViewMode::Playlist {
                // Tab flips the library between the flat list and the artist→album tree.
                if matches!(k, KeyEvent { code: KeyCode::Tab, .. }) {
                    ui.library_tree_mode = !ui.library_tree_mode;
                    if ui.library_tree_mode {
                        ui.tree_dirty = true;
                    }
                    return false;
                }
                if ui.library_tree_mode {
                    // A staged bulk remove is awaiting confirmation: `y` removes,
                    // any other key cancels.
                    if let Some((label, paths)) = ui.tree_pending_remove.take() {
                        if matches!(k, KeyEvent { code: KeyCode::Char('y'), .. }) {
                            let indices = indices_of(playlist, &paths);
                            tree_remove_indices(state, ui, playlist, &indices);
                        } else {
                            ui.set_status(format!("cancelled removing {label}"));
                        }
                        return false;
                    }
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
                            ui.input_mode = InputMode::Search(ui.tree_filter.clone());
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
                        KeyEvent { code: KeyCode::Char('g'), modifiers, .. }
                            if !modifiers.contains(KeyModifiers::SHIFT) => { ui.tree_cursor = 0; continue; }
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
                    KeyEvent { code: KeyCode::Char('g'), modifiers, .. } if !modifiers.contains(KeyModifiers::SHIFT) => {
                        playlist_cursor_home(ui);
                        continue;
                    }
                    KeyEvent { code: KeyCode::Char('g'), modifiers, .. } if modifiers.contains(KeyModifiers::SHIFT) => {
                        playlist_cursor_end(ui, playlist);
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
                    KeyEvent { code: KeyCode::Char('s'), modifiers, .. } if modifiers.contains(KeyModifiers::SHIFT) => {
                        sort_playlist_by_tags(state, ui, playlist);
                        return false;
                    }
                    KeyEvent { code: KeyCode::Char('S'), .. } => {
                        sort_playlist_by_tags(state, ui, playlist);
                        return false;
                    }
                    KeyEvent { code: KeyCode::Enter, .. } => {
                        let target = if ui.filtered_indices.is_empty() {
                            ui.cursor
                        } else {
                            ui.filtered_indices.get(ui.cursor).copied().unwrap_or(ui.cursor)
                        };
                        // A jump past the end made the playlist loop see
                        // current >= len — with repeat off, Keet exited.
                        if target < playlist.len() {
                            state.jump_to(target);
                        }
                        return false;
                    }
                    KeyEvent { code: KeyCode::Char('/'), .. } => {
                        ui.input_mode = InputMode::Search(String::new());
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
                    // cycling here changed nothing on screen — and landing on
                    // VizMode::None starved the analyser those meters read,
                    // leaving them flat. Ignore the key rather than let it
                    // silently break the panel.
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
                    // E opens (and closes) the EQ+FX editor screen.
                    ui.view_mode = match ui.view_mode {
                        ViewMode::Eq => ViewMode::Player,
                        _ => ViewMode::Eq,
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
                KeyEvent { code: KeyCode::Char('l'), .. } => {
                    ui.view_mode = match ui.view_mode {
                        ViewMode::Player | ViewMode::Lyrics | ViewMode::Eq | ViewMode::Help => {
                            ui.cursor = ui.current;
                            ensure_cursor_visible(ui, playlist);
                            ViewMode::Playlist
                        }
                        ViewMode::Playlist => ViewMode::Player,
                    };
                }
                KeyEvent { code: KeyCode::Char('y'), .. } => {
                    ui.view_mode = match ui.view_mode {
                        ViewMode::Player | ViewMode::Playlist | ViewMode::Eq | ViewMode::Help => {
                            ui.lyrics_scroll = 0;
                            ui.lyrics_auto_scroll = true;
                            ViewMode::Lyrics
                        }
                        ViewMode::Lyrics => ViewMode::Player,
                    };
                }
                KeyEvent { code: KeyCode::Char('s'), .. } => {
                    ui.input_mode = InputMode::SavePlaylist(String::new());
                }
                KeyEvent { code: KeyCode::Char('r'), modifiers, .. } if modifiers.contains(KeyModifiers::SHIFT) => {
                    toggle_repeat(ui, state);
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
                    let picked = prompt_path_line();
                    let _ = terminal::enable_raw_mode();
                    // prompt_path_line prints the prompt/echoed chars inline, which
                    // pushes the UI's cursor-tracking out of sync. Force a full redraw
                    // on the next frame via the same path as a terminal resize.
                    ui.terminal_resized = true;
                    match picked {
                        Some(p) => switch_source_paths(state, ui, playlist, p),
                        None => ui.set_status("Cancelled".to_string()),
                    }
                }
                KeyEvent { code: KeyCode::Char('p'), .. } => {
                    if has_native_picker() {
                        match pick_folder_native() {
                            Some(p) => switch_source_paths(state, ui, playlist, p),
                            None => ui.set_status("Cancelled".to_string()),
                        }
                    } else {
                        ui.set_status("Native picker unavailable; press O to type a path".to_string());
                    }
                }
                KeyEvent { code: KeyCode::Char('q'), .. } => { state.quit(); return true; }
                // Esc closes every other view, so in the player a stray one
                // (meant for a view already closed) quit without warning: it
                // now asks once, and a second Esc within ESC_QUIT_WINDOW quits.
                KeyEvent { code: KeyCode::Esc, .. } => {
                    let now = std::time::Instant::now();
                    if esc_confirms_quit(ui.esc_quit_until, now) {
                        state.quit();
                        return true;
                    }
                    ui.esc_quit_until = Some(now + ESC_QUIT_WINDOW);
                    ui.set_status_for("press Esc again to quit (or q)".to_string(), ESC_QUIT_WINDOW);
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
                    let on = state.toggle_viz_extras();
                    let what = match state.viz_mode() {
                        VizMode::VuMeter => "level history",
                        VizMode::SpectrumHorizontal | VizMode::SpectrumVertical => "band legend",
                        _ => "detail (not used by this viz)",
                    };
                    ui.set_status(format!("{what}: {}", if on { "on" } else { "off" }));
                    // Costs or frees a row, so the frame changes height.
                    ui.terminal_resized = true;
                    continue;
                }
                KeyEvent { code: KeyCode::Char('F'), .. } => {
                    let on = state.toggle_viz_fullscreen();
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

/// `/` search while the tree view is showing: keystrokes drive `ui.tree_filter`
/// live. Enter keeps the filter and returns to navigating the results; Esc
/// clears it. Up/Down/PageUp/PageDown move the cursor through the filtered rows.
/// Change the playing track's lyrics offset and remember it for that track.
fn set_lyrics_offset(ui: &mut UiState, playlist: &[PathBuf], secs: f64) {
    ui.lyrics_offset = secs;
    if let Some(path) = playlist.get(ui.current) {
        ui.lyrics_offsets.set(path, secs);
    }
}

fn tree_search_input(ui: &mut UiState, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => {
            ui.input_mode = InputMode::Normal;
            ui.tree_filter.clear();
            refresh_tree_rows(ui);
            ui.tree_cursor = 0;
            ui.tree_scroll = 0;
        }
        KeyCode::Enter => {
            ui.input_mode = InputMode::Normal; // keep the filter, navigate results
        }
        KeyCode::Backspace => {
            if let InputMode::Search(ref mut q) = ui.input_mode {
                q.pop();
            }
            rebuild_tree_filter(ui);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let InputMode::Search(ref mut q) = ui.input_mode {
                q.push(c);
            }
            rebuild_tree_filter(ui);
        }
        KeyCode::Up => tree_move(ui, -1),
        KeyCode::Down => tree_move(ui, 1),
        KeyCode::PageUp => tree_page(ui, -1),
        KeyCode::PageDown => tree_page(ui, 1),
        _ => {}
    }
    false
}

fn handle_text_input(state: &PlayerState, ui: &mut UiState, _playlist: &mut Vec<PathBuf>, key: KeyEvent) -> bool {
    // Ctrl+C quits from any text prompt, same as everywhere else. Without this
    // the Char(c) arms below would type a literal 'c' into the query — the
    // global Ctrl+C handler is never reached while a prompt is active.
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.quit();
        return true;
    }
    // The tree view filters itself; its `/` search has its own handling.
    if ui.library_tree_mode && matches!(ui.input_mode, InputMode::Search(_)) {
        return tree_search_input(ui, key);
    }
    match &mut ui.input_mode {
        InputMode::Search(ref mut query) => {
            match key.code {
                KeyCode::Esc => {
                    ui.input_mode = InputMode::Normal;
                    ui.filtered_indices.clear();
                    ui.cursor = 0;
                    ui.scroll_offset = 0;
                }
                KeyCode::Enter => {
                    // A non-empty query with zero hits leaves filtered_indices
                    // empty — falling through to ui.cursor here would jump to
                    // an unrelated track. Just close the search instead.
                    let no_matches = !query.is_empty() && ui.filtered_indices.is_empty();
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
                KeyCode::Backspace => {
                    query.pop();
                    rebuild_filter(ui, _playlist);
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(c);
                    rebuild_filter(ui, _playlist);
                }
                KeyCode::Up => {
                    playlist_cursor_up(ui);
                }
                KeyCode::Down => {
                    playlist_cursor_down(ui, _playlist);
                }
                KeyCode::Home => {
                    playlist_cursor_home(ui);
                }
                KeyCode::End => {
                    playlist_cursor_end(ui, _playlist);
                }
                KeyCode::PageUp => {
                    playlist_cursor_page_up(ui);
                }
                KeyCode::PageDown => {
                    playlist_cursor_page_down(ui, _playlist);
                }
                _ => {}
            }
        }
        InputMode::SavePlaylist(ref mut name) => {
            match key.code {
                KeyCode::Esc => {
                    ui.input_mode = InputMode::Normal;
                }
                KeyCode::Enter => {
                    let save_name = name.clone();
                    ui.input_mode = InputMode::Normal;
                    if !save_name.is_empty() {
                        match crate::playlist::save_m3u(_playlist, &save_name) {
                            Ok(path) => {
                                let fname = path.file_name().unwrap_or_default().to_string_lossy();
                                ui.set_status(format!("Saved {} tracks to {}", _playlist.len(), fname));
                            }
                            Err(e) => {
                                ui.set_status(format!("Save failed: {}", e));
                            }
                        }
                    }
                }
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    name.push(c);
                }
                _ => {}
            }
        }
        InputMode::Normal => {}
    }
    false
}

fn rebuild_filter(ui: &mut UiState, playlist: &[PathBuf]) {
    let query = match &ui.input_mode {
        InputMode::Search(q) => q.to_lowercase(),
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

fn playlist_cursor_down(ui: &mut UiState, playlist: &[PathBuf]) {
    let max = if ui.filtered_indices.is_empty() {
        playlist.len().saturating_sub(1)
    } else {
        ui.filtered_indices.len().saturating_sub(1)
    };
    if ui.cursor < max {
        ui.cursor += 1;
    }
}

fn playlist_cursor_home(ui: &mut UiState) {
    ui.cursor = 0;
    ui.scroll_offset = 0;
}

fn playlist_cursor_end(ui: &mut UiState, playlist: &[PathBuf]) {
    let max = if ui.filtered_indices.is_empty() {
        playlist.len().saturating_sub(1)
    } else {
        ui.filtered_indices.len().saturating_sub(1)
    };
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
    let max = if ui.filtered_indices.is_empty() {
        playlist.len().saturating_sub(1)
    } else {
        ui.filtered_indices.len().saturating_sub(1)
    };
    let page = ui.last_visible_rows.saturating_sub(1).max(1);
    ui.cursor = (ui.cursor + page).min(max);
}

fn ensure_cursor_visible(ui: &mut UiState, _playlist: &[PathBuf]) {
    if ui.cursor < ui.scroll_offset {
        ui.scroll_offset = ui.cursor;
    }
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
    let track_idx = if ui.filtered_indices.is_empty() {
        ui.cursor
    } else {
        match ui.filtered_indices.get(ui.cursor) {
            Some(&idx) => idx,
            None => return,
        }
    };
    if track_idx >= playlist.len() { return; }
    let removed_name = ui.metadata_cache.display_name(track_idx, &playlist[track_idx]);
    if remove_indices(state, ui, playlist, &[track_idx]) {
        ui.set_status(format!("Removed: {}", removed_name));
    }
}

/// Remove playlist entries — the one place that does it, for the flat list's
/// `D` and the library tree alike (the tree's own copy skipped all of this:
/// a removed playing track went on playing under another track's title, and
/// removing everything emptied the list and panicked the next frame).
///
/// Drops the entries (descending, so earlier indices don't shift), records
/// them in `removed_paths` so a rescan or the repeat cycle won't bring them
/// back, keeps the playing index on the playing track, and when the playing
/// track itself goes, skips it and remembers where its successor now sits
/// (`removed_current_next`, possibly `len` = past the end). The queue count
/// loses any queued entries removed. Refuses to empty the playlist. Returns
/// whether anything was removed.
fn remove_indices(
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
    ui.playlist_dirty = true;
    reindex_and_restart_scan(ui, playlist, &old_playlist);

    // Rebuild filter if searching, otherwise just adjust cursor
    if !ui.filtered_indices.is_empty() {
        rebuild_filter(ui, playlist);
    }
    let max_cursor = if ui.filtered_indices.is_empty() {
        playlist.len().saturating_sub(1)
    } else {
        ui.filtered_indices.len().saturating_sub(1)
    };
    ui.cursor = ui.cursor.min(max_cursor);
    true
}

/// The track to play after the playlist was edited mid-track — the producer's
/// own idea of "next" came from the old list. Follows the repeat mode like a
/// natural track change (it used to be current + 1 regardless: repeat-one
/// moved on, repeat-all never wrapped). `removed_next` is where the removed
/// playing track's successor sits. `len` means past the end: the playlist
/// loop turns it into the repeat-all cycle or the end of playback, exactly as
/// when the last track finishes.
/// The track that follows the one ending, given the index the producer
/// reports. The producer works from its own copy of the playlist, so after
/// an edit its index means nothing in the real list: the edit is consumed
/// here and `track_after_edit` decides. Every handler that moves on from a
/// producer's report goes through this (the transition, the rate change, the
/// end of the producer's list) — the natural advance used to be the only one
/// that checked, so a track queued during the last track was dropped and a
/// rate change after a removal played the wrong track.
pub(crate) fn next_after_producer(ui: &mut UiState, producer_index: usize, len: usize) -> usize {
    if std::mem::take(&mut ui.playlist_dirty) {
        track_after_edit(ui.current, ui.removed_current_next.take(), len, ui.repeat_mode)
    } else {
        producer_index
    }
}

/// A producer is starting from the list as it is now, so any edit is
/// already in its copy and nothing is left to re-resolve. Left set, the next
/// NATURAL track change took the jump path: a drained ring, ~0.5 s cut off the
/// track's end, no gapless join or crossfade.
pub(crate) fn producer_started(ui: &mut UiState) {
    ui.playlist_dirty = false;
    ui.removed_current_next = None;
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

/// True when the source paths are a folder/file collection we should auto-sort
/// into artist→album order: non-empty and containing no curated `.m3u`/`.m3u8`
/// playlist (an M3U's order is the user's curation and must be preserved).
fn source_is_sortable(paths: &[PathBuf]) -> bool {
    !paths.is_empty()
        && paths.iter().all(|p| {
            let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
            !matches!(ext.as_deref(), Some("m3u") | Some("m3u8"))
        })
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
    if !ui.library_tree_mode {
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
    }
}

/// Render the tree body for a themed library renderer: adjust scroll to keep the
/// cursor visible (small margin), record the viewport height for paging, and
/// return the palette-coloured lines.
pub fn render_tree_body(
    ui: &mut UiState,
    height: usize,
    width: usize,
    p: &crate::theme::Palette,
) -> Vec<String> {
    let n = ui.tree_rows.len();
    if ui.tree_cursor >= n {
        ui.tree_cursor = n.saturating_sub(1);
    }
    ui.tree_view_height = height;
    let margin = 4.min(height / 2);
    if ui.tree_cursor < ui.tree_scroll + margin {
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
        ui.tree_cursor,
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
        InputMode::Search(q) => q.clone(),
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
            ui.set_status(format!("remove {label} — {} tracks?  [y/n]", indices.len()));
            ui.tree_pending_remove = Some((label, paths_at(playlist, &indices)));
        }
        crate::library::VisibleRow::Artist { artist } => {
            let label = format!("artist {}", ui.library_tree.artists[artist].name);
            ui.set_status(format!("remove {label} — {} tracks?  [y/n]", indices.len()));
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
    if matches!(ui.input_mode, InputMode::Search(_)) {
        // A live search's hits are playlist indices into the OLD order, and
        // its cursor is a position in that hit list — re-run the query. (The
        // one-shot auto-sort fires in the background, so it can land mid-search.)
        rebuild_filter(ui, playlist);
    } else {
        ui.cursor = ui.current;
        ensure_cursor_visible(ui, playlist);
    }
    ui.playlist_dirty = true;
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
    let cursor_track = if ui.filtered_indices.is_empty() {
        playlist.get(ui.cursor).cloned()
    } else {
        ui.filtered_indices.get(ui.cursor).and_then(|&i| playlist.get(i)).cloned()
    };
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
        let pos = if ui.filtered_indices.is_empty() {
            playlist.iter().position(|p| *p == track)
        } else {
            ui.filtered_indices.iter().position(|&i| playlist[i] == track)
        };
        if let Some(pos) = pos {
            ui.cursor = pos;
        }
    }
    // Cached metadata is indexed by position — remap it to match the reordered paths.
    reindex_and_restart_scan(ui, playlist, &old_playlist);
    ui.playlist_dirty = true;
}

/// The playing track becomes `new`, by any route: a natural advance, the jump
/// after a playlist edit, a skip, an exclusive-mode rate change. The one
/// place `current`, the queue count and the headers' track index change
/// together — the queue rule used to run on the natural advance only, and a
/// queue edit sends the next track change down the jump path, where the
/// count was zeroed.
pub(crate) fn advance_to(ui: &mut UiState, state: &PlayerState, new: usize) {
    ui.enqueue_count = queue_after_advance(ui.enqueue_count, ui.current, new);
    ui.current = new;
    state.current_track.store(new, Ordering::Relaxed);
}

/// The queue after the playing track moves from `old` to `new`: one fewer
/// when the next track starts (the first queued one, while there is a
/// queue), unchanged when the same track restarts (repeat-one), and gone after
/// any other move, which leaves the queued tracks somewhere else entirely. It
/// used to be emptied on every track change, so tracks queued later jumped
/// ahead of ones still waiting.
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
fn enqueue_track(state: &PlayerState, ui: &mut UiState, playlist: &mut Vec<PathBuf>) {
    let track_idx = if ui.filtered_indices.is_empty() {
        ui.cursor
    } else {
        match ui.filtered_indices.get(ui.cursor) {
            Some(&idx) => idx,
            None => return,
        }
    };
    if track_idx >= playlist.len() || track_idx == ui.current { return; }
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
        // queued without moving. It used to be ignored, and not counted.
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
    ui.playlist_dirty = true;
    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(ui.current, Ordering::Relaxed);
    reindex_and_restart_scan(ui, playlist, &old_playlist);
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

    let old_playlist = std::mem::replace(playlist, new_list);
    ui.source_paths = vec![new_path.clone()];
    // A rescan still running describes the OLD sources: landing afterwards,
    // its result replaced the new playlist. Dropping the receiver discards it.
    ui.rescan_receiver = None;
    ui.pre_shuffle_order = None; // snapshot belongs to the previous source
    ui.current = 0;
    // The queue was positions in the old playlist. Relying on the jump to
    // empty it failed when the old track was also index 0 (advance_to keeps
    // the queue when the index does not move).
    ui.enqueue_count = 0;
    ui.cursor = 0;
    ui.scroll_offset = 0;

    state.total_tracks.store(playlist.len(), Ordering::Relaxed);
    state.current_track.store(0, Ordering::Relaxed);

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
/// a worker thread — on a large or network library they took seconds, and the
/// UI froze for all of it; [`poll_rescan`] applies the result when it lands.
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
    if matches!(ui.input_mode, InputMode::Search(_)) {
        rebuild_filter(ui, playlist);
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

/// Prompt for a path in raw mode so Esc can cancel. Enter submits, Backspace edits,
/// Ctrl-C cancels. On entry/exit this leaves the terminal in cooked mode — the caller
/// is responsible for re-enabling raw mode if it needs it.
fn prompt_path_line() -> Option<PathBuf> {
    let _ = terminal::enable_raw_mode();
    crate::term::out!("\n\r  {}Enter path (Esc to cancel):{} ", C_BOLD, C_RESET);
    crate::term::flush();

    let mut buf = String::new();
    let result = loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Esc => break None,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break None,
                KeyCode::Enter => {
                    let trimmed = buf.trim().to_string();
                    break if trimmed.is_empty() { None } else { Some(PathBuf::from(trimmed)) };
                }
                KeyCode::Backspace
                    if buf.pop().is_some() => {
                        crate::term::out!("\x08 \x08");
                        crate::term::flush();
                    }
                KeyCode::Char(c) => {
                    buf.push(c);
                    crate::term::out!("{}", c);
                    crate::term::flush();
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break None,
        }
    };

    let _ = terminal::disable_raw_mode();
    crate::term::out!("\r\n");
    crate::term::flush();
    result
}

/// Interactive first-launch picker shown when the user runs keet with no args
/// and no saved session. Returns the selected path or None if the user quit.
pub fn run_first_launch_picker() -> Option<PathBuf> {
    let native = has_native_picker();
    loop {
        println!();
        println!("  {}Keet{} — no music source given and no saved session.", C_BOLD, C_RESET);
        println!();
        if native {
            println!("  {}P{}  Pick a folder", C_CYAN, C_RESET);
        }
        println!("  {}T{}  Type a path", C_CYAN, C_RESET);
        println!("  {}Q{}  Quit", C_CYAN, C_RESET);
        println!();
        crate::term::out!("  {}Choose:{} ", C_DIM, C_RESET);
        crate::term::flush();

        if terminal::enable_raw_mode().is_err() {
            return None;
        }
        let key = loop {
            if let Ok(true) = event::poll(Duration::from_millis(500)) {
                if let Ok(Event::Key(k)) = event::read() {
                    if k.kind == KeyEventKind::Release { continue; }
                    break k;
                }
            }
        };
        let _ = terminal::disable_raw_mode();
        println!();

        let chosen = match key.code {
            KeyCode::Char('p') | KeyCode::Char('P') if native => pick_folder_native(),
            KeyCode::Char('t') | KeyCode::Char('T') => prompt_path_line(),
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => return None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return None,
            _ => continue,
        };

        match chosen {
            Some(p) if p.exists() => return Some(p),
            Some(p) => {
                println!("  {}Path not found:{} {}", C_RED, C_RESET, p.display());
            }
            None => {
                println!("  {}Cancelled{}", C_DIM, C_RESET);
            }
        }
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;

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

    /// One track change as the player makes it: the producer reports the
    /// index of the next track in ITS copy of the list (`stale`, which after
    /// an edit is meaningless), the handler resolves it with
    /// `next_after_producer` and moves there with `advance_to`, then a
    /// producer starts from the list as it is. Returns the index moved to.
    fn change_track(state: &PlayerState, ui: &mut UiState, playlist: &[PathBuf], stale: usize) -> usize {
        let target = next_after_producer(ui, stale, playlist.len());
        if target < playlist.len() {
            advance_to(ui, state, target);
            producer_started(ui);
        }
        target
    }

    /// A natural track change: the producer, unaware of edits, reports the
    /// track after the current one.
    fn play_next(state: &PlayerState, ui: &mut UiState, playlist: &[PathBuf]) {
        let stale = ui.current + 1;
        change_track(state, ui, playlist, stale);
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
    fn a_track_queued_during_the_last_track_plays_when_the_producer_runs_out() {
        // The producer's copy ends at c; it reports "past the end" (len of
        // ITS list). The handler must continue at the queued a, not end.
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = list_of(&["a", "b", "c"]);
        ui.current = 2;
        queue_named(&state, &mut ui, &mut playlist, "a");
        assert_eq!(names(&playlist), ["b", "c", "a"]);
        let stale_len = 3;
        let next = change_track(&state, &mut ui, &playlist, stale_len);
        assert_eq!(next, 2, "the queued track, not the end of the list");
        assert_eq!(playlist[ui.current], p("/a.mp3"));
        assert_eq!(ui.enqueue_count, 0);
        // Without an edit, running out is the end.
        let next = change_track(&state, &mut ui, &playlist, playlist.len());
        assert_eq!(next, playlist.len());
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
        assert!(remove_indices(&state, &mut ui, &mut playlist, &[0]));
        // The producer skips a and reports its own next track (index 1 in
        // its copy: d, but stale all the same).
        change_track(&state, &mut ui, &playlist, 1);
        assert_eq!(playlist[ui.current], p("/d.mp3"));
        assert_eq!(ui.enqueue_count, 1, "only e is still queued");
        queue_named(&state, &mut ui, &mut playlist, "c");
        assert_eq!(names(&playlist), ["d", "e", "c", "b"], "c lands after e, not after a phantom");
    }

    #[test]
    fn a_rate_change_after_removing_the_playing_track_plays_its_successor() {
        // Remove a (playing); b needs another rate. The producer, working
        // from [a, b, c], skips a and reports b at ITS index 1 — which is c
        // in the edited list. The handler must start b, and c's end must
        // then be a natural advance, not a jump back to b.
        let state = PlayerState::new();
        let mut ui = test_ui(3);
        let mut playlist = list_of(&["a", "b", "c"]);
        ui.current = 0;
        assert!(remove_indices(&state, &mut ui, &mut playlist, &[0]));
        change_track(&state, &mut ui, &playlist, 1);
        assert_eq!(playlist[ui.current], p("/b.mp3"));
        assert!(!ui.playlist_dirty, "consumed, so the next change is natural");
        play_next(&state, &mut ui, &playlist);
        assert_eq!(playlist[ui.current], p("/c.mp3"));
    }

    #[test]
    fn a_new_producer_clears_the_edit_so_the_next_change_is_natural() {
        // An edit followed by a respawn (a jump, recovery, a rate change):
        // the new producer has the edited list, so its next report is right
        // and must not be re-resolved (that drained the ring and broke
        // gapless at every following track change).
        let mut ui = test_ui(3);
        ui.playlist_dirty = true;
        ui.removed_current_next = Some(1);
        producer_started(&mut ui);
        assert_eq!(next_after_producer(&mut ui, 2, 3), 2);
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
            let lines = help_lines(p, w, h);
            assert!(lines.len() < h, "{w}×{h}: {} rows", lines.len());
            assert!(lines.iter().all(|l| visible_len(l) <= w), "{w}×{h}: a line overflows");
        }
        // A 100-column window holds two columns.
        let two = help_lines(p, 100, 30);
        assert!(two.iter().any(|l| crate::ansi::strip_ansi(l).contains("PLAYLIST")), "{two:#?}");
        // Wide and tall enough: every key is there.
        let text = help_lines(p, 200, 60).iter().map(|l| crate::ansi::strip_ansi(l)).collect::<Vec<_>>().join("\n");
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
        let text: Vec<String> = lines.iter().map(|l| crate::ansi::strip_ansi(l).trim().to_string()).collect();
        assert!(text.contains(&"39×9 · Keet needs 40×10".to_string()), "{text:?}");
        assert!(text.contains(&"⏸ Undertow".to_string()), "{text:?}");
    }

    #[test]
    fn frame_writer_derives_count_from_emission() {
        let mut w = FrameWriter::new();
        w.first_line("anchor");
        assert_eq!(w.count(), 0, "the first line is the anchor row, not counted");
        w.line("progress");
        w.line_raw("sixel transmit");
        w.line("");
        assert_eq!(w.count(), 3, "count must equal the lines actually emitted");
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
    fn source_is_sortable_false_when_any_m3u_present_or_empty() {
        let p = |s: &str| PathBuf::from(s);
        // Folder / file sources are sortable.
        assert!(source_is_sortable(&[p("/music/rock"), p("/music/song.flac")]));
        // Any .m3u / .m3u8 source is a curated order — not sortable (case-insensitive).
        assert!(!source_is_sortable(&[p("/music/mix.m3u")]));
        assert!(!source_is_sortable(&[p("/music/rock"), p("/lists/set.M3U8")]));
        // No sources → nothing to auto-sort.
        assert!(!source_is_sortable(&[]));
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
        ui.input_mode = InputMode::Search(String::new());
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
        ui.input_mode = InputMode::Search(String::new());
        handle_text_input(
            &state, &mut ui, &mut playlist,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
        );
        assert!(matches!(&ui.input_mode, InputMode::Search(q) if q == "c"));
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
        ui.input_mode = InputMode::Search("t2".into());
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
        assert!(ui.playlist_dirty, "the transition handler must re-resolve the next track");
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
        assert!(!ui.playlist_dirty);
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
        ui.input_mode = InputMode::SavePlaylist(String::new());
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
        ui.input_mode = InputMode::Search(String::new());
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
        let full = "  [←→] band  [↑↓] gain (⇧ fine)  [t] type  [,.] Q  [<>] freq  [[]] preset  [0] reset  [e/Esc] close";
        let short = "  ←→ band ↑↓ gain t type ,. Q <> freq e close";
        for term_w in 10usize..130 {
            for avail in 0usize..45 {
                let body = crate::eq_ui::render_eq_screen(
                    &bands, 0, "Flat", &readouts, '█', p, term_w, avail.saturating_sub(1),
                );
                let lines = fit_eq_screen(body, full, short, term_w, avail);
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
        ui.input_mode = InputMode::Search("song".to_string());
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
}
