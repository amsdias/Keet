//! The first-launch screen: what `keet` shows with no music given and no
//! session to resume. An animated field of sound ripples fills the window,
//! the KEET wordmark builds up out of it, and the keys underneath ask for a
//! folder — picked in the system dialog, or typed (or dropped) on a line in
//! the frame.
//!
//! Every frame is a pure function of the time since the screen opened
//! ([`frame`]), so a late frame skips ahead instead of stuttering, and the
//! tests can render any instant. It is shown only when there is nothing to
//! play: a normal launch never waits on it.
//!
//! The design (an animation shown only for an empty state, a pure function
//! of time, a block wordmark the art never paints over) is inspired by the
//! splash screen of nebula (<https://github.com/agentSystemLabs/nebula>).

use crate::text_input::Edit;
use crate::tui::Cols;
use crate::ansi::truncate_plain;
use crate::theme::{Palette, ThemeKind};
use crate::tui::{self, LineBuf};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Time between frames: the player loop's own cadence.
const FRAME: Duration = Duration::from_millis(50);

/// Seconds the scene takes to fade in from black.
const FADE_IN: f32 = 1.2;

/// Glyph ramp for the ripples, faint to bright. Single-column glyphs only.
const RAMP: [char; 8] = ['.', '·', ':', '-', '+', '*', 'o', '@'];

/// Below this the field leaves a cell to the stars.
const FLOOR: f32 = 0.07;

/// Cell aspect: a terminal cell is about twice as tall as it is wide, so a
/// row counts double when measuring distance, or the rings come out oval.
const CELL_ASPECT: f32 = 2.0;

use crate::theme::{base_hue, hsv, Rgb};

/// Seconds for the colours to go once round the hue wheel.
const CYCLE_SECS: f32 = 40.0;

/// Degrees the hue moves from the centre of the ripples to their edge, so
/// the colour drifts outward with the rings.
const RADIAL_HUE: f32 = 40.0;

/// Degrees the wordmark's gradient spans from its first letter to its last.
const MARK_SPAN: f32 = 70.0;

/// The hue at time `t`, `offset` degrees along: the theme's own colour first,
/// then round the wheel and back every [`CYCLE_SECS`].
fn hue_at(kind: ThemeKind, t: f32, offset: f32) -> f32 {
    (base_hue(kind) + t / CYCLE_SECS * 360.0 + offset).rem_euclid(360.0)
}

/// A ripple cell's colour: step `i` of the [`RAMP`] at `hue` — dark and
/// saturated for the faint glyphs, bright and paler toward the crests.
fn field_colour(i: usize, hue: f32) -> Rgb {
    let k = i as f32 / (RAMP.len() - 1) as f32;
    hsv(hue, 0.85 - 0.4 * k * k, 0.32 + 0.68 * k)
}

/// The highlight that sweeps through the wordmark now and then.
const GLINT: Rgb = (255, 255, 255);
/// Star colours: a faint one and a brighter one.
const STAR_DIM: Rgb = (78, 78, 78);
const STAR_LIT: Rgb = (188, 188, 188);


/// KEET in 5-row block letters; each `#` is drawn two cells wide so the
/// letters keep their shape in tall terminal cells.
const LETTERS: [[&str; 5]; 4] = [
    ["#..#", "#.#.", "##..", "#.#.", "#..#"],
    ["####", "#...", "###.", "#...", "####"],
    ["####", "#...", "###.", "#...", "####"],
    ["#####", "..#..", "..#..", "..#..", "..#.."],
];
/// Columns between letters.
const LETTER_GAP: usize = 2;

/// The wordmark's width in columns.
fn wordmark_width() -> usize {
    LETTERS.iter().map(|l| l[0].len() * 2).sum::<usize>() + LETTER_GAP * (LETTERS.len() - 1)
}

/// The window the block letters need; smaller windows get a one-line name.
const BIG_MIN: (usize, usize) = (50, 18);

/// What the bottom of the screen is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The keys: pick, type, quit.
    Keys,
    /// A path being typed (or pasted, or dropped).
    Typing(crate::text_input::TextInput),
}

/// A line under the keys: a problem, or a plain word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub text: String,
    pub error: bool,
}

/// Everything a frame shows besides the time.
pub struct View<'a> {
    pub kind: ThemeKind,
    pub palette: &'a Palette,
    /// A system folder dialog exists (macOS, Windows): `p` is offered.
    pub native: bool,
    pub mode: &'a Mode,
    pub note: Option<&'a Note>,
}

/// How far the scene has faded in `t` seconds after it opened: 0 → 1,
/// eased at both ends.
fn fade_at(t: f32) -> f32 {
    let x = (t / FADE_IN).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn hash(x: i32, y: i32, salt: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(374_761_393)
        ^ (y as u32).wrapping_mul(668_265_263)
        ^ salt.wrapping_mul(2_246_822_519);
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    h ^ (h >> 16)
}

/// The ripple field at (`u`, `v`) — a point measured from the centre of the
/// sky in units of its half-width — `t` seconds in, 0..=1. Two sources
/// circle the centre slowly, each sending rings outward; where their crests
/// meet they reinforce, where a crest meets a trough they cancel, so the
/// pattern turns and shifts as they move. A soft falloff keeps the edges dark.
fn ripple(u: f32, v: f32, t: f32) -> f32 {
    const WAVE: f32 = 34.0; // rings per unit of distance (radians)
    const SPEED: f32 = 4.0; // how fast the rings travel outward
    const ORBIT: f32 = 0.22; // the sources' distance from the centre
    let (s, c) = (t * 0.17).sin_cos();
    let wave = |sx: f32, sy: f32| {
        let r = ((u - sx).powi(2) + (v - sy).powi(2)).sqrt();
        (WAVE * r - SPEED * t).cos() / (1.0 + 1.5 * r)
    };
    let sum = wave(ORBIT * c, ORBIT * s) + wave(-ORBIT * c, -ORBIT * s);
    let r = (u * u + v * v).sqrt();
    let falloff = (-r * r * 1.1).exp();
    // Crests only: the troughs are the dark between the rings.
    (sum * 0.5).max(0.0).powf(1.2) * falloff * 1.5
}

/// The wordmark's colour at `x` (0 → 1 across it): a gradient that cycles
/// with the ripples, and the glint sweeping through once the scene has
/// faded in.
fn mark_colour(x: f32, t: f32, fade: f32, kind: ThemeKind) -> Rgb {
    if fade >= 1.0 && (x * 5.0 - t * 1.3).sin() > 0.94 {
        return GLINT;
    }
    hsv(hue_at(kind, t, x * MARK_SPAN), 0.55, 1.0)
}

/// One row of the wordmark, coloured, `wordmark_width()` columns wide. The
/// blocks build up `░ → ▒ → █` while the scene fades in.
fn wordmark_row(row: usize, t: f32, fade: f32, kind: ThemeKind) -> Line<'static> {
    let block = if fade < 0.45 { "░░" } else if fade < 0.8 { "▒▒" } else { "██" };
    let width = wordmark_width() as f32;
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut out = LineBuf::new();
    let mut col = 0;
    for (i, letter) in LETTERS.iter().enumerate() {
        if i > 0 {
            out.style(bold).spaces(LETTER_GAP);
            col += LETTER_GAP;
        }
        for ch in letter[row].chars() {
            if ch == '#' {
                let c = mark_colour(col as f32 / width, t, fade, kind);
                out.style(bold.patch(tui::fg(c))).push_str(block);
            } else {
                out.style(bold).push_str("  ");
            }
            col += 2;
        }
    }
    out.into_line()
}

/// `key label` pairs joined with a dim dot, as long as they fit `width`;
/// otherwise one pair per line.
fn key_lines(keys: &[(&str, &str)], width: usize, p: &Palette) -> Vec<Line<'static>> {
    let sep = "   ·   ";
    let one_line = keys.iter().map(|(k, l)| tui::width(k) + 1 + tui::width(l)).sum::<usize>()
        + tui::width(sep) * keys.len().saturating_sub(1);
    let styled = |(k, l): &(&str, &str)| {
        let mut b = LineBuf::new();
        b.styled(k, p.bold.patch(p.accent)).reset().push(' ').styled(l, p.dim);
        b.into_line()
    };
    if one_line <= width {
        let mut line = LineBuf::new();
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                line.styled(sep, p.dim);
            }
            line.append(styled(k));
        }
        vec![line.into_line()]
    } else {
        keys.iter().map(styled).collect()
    }
}

/// The text block: wordmark (or name), the situation, then the keys or the
/// path being typed, and the note. Each line is at most `width` columns.
fn text_lines(view: &View, width: usize, big: bool, t: f32, fade: f32) -> Vec<Line<'static>> {
    let p = view.palette;
    let mut lines = Vec::new();
    if big {
        for row in 0..5 {
            lines.push(wordmark_row(row, t, fade, view.kind));
        }
    } else {
        let mut l = LineBuf::new();
        l.styled("◆ ", p.accent.patch(p.bold)).styled("Keet", p.bold.patch(p.fg));
        lines.push(l.into_line());
    }
    lines.push(Line::default());
    let situation = "No music given, and no session to resume.";
    lines.push(tui::line(truncate_plain(situation, width), p.dim));
    lines.push(Line::default());
    match view.mode {
        Mode::Keys => {
            let mut keys = Vec::new();
            if view.native {
                keys.push(("p", "pick a folder"));
            }
            keys.push(("t", "type a path"));
            keys.push(("q", "quit"));
            lines.extend(key_lines(&keys, width, p));
        }
        Mode::Typing(field) => {
            // The field scrolled to keep the cursor in view, the cell under
            // the cursor reversed (it was always the end: the arrow keys
            // moved nothing).
            let prompt = "path › ";
            let room = width.saturating_sub(tui::width(prompt)); // columns, not bytes: › is 3
            let (before, at, after) = shown_path(field, room);
            let mut l = LineBuf::new();
            l.styled(prompt, p.accent).styled(&before, p.fg);
            l.style(p.fg.add_modifier(Modifier::REVERSED)).push_str(&at);
            l.styled(&after, p.fg);
            lines.push(l.into_line());
            let hint = "Enter open · Esc back · drop a folder here";
            lines.push(tui::line(truncate_plain(hint, width), p.dim));
        }
    }
    if let Some(note) = view.note {
        lines.push(Line::default());
        let colour = if note.error { p.danger } else { p.dim };
        lines.push(tui::line(truncate_plain(&note.text, width), colour));
    }
    lines
}

/// The typed path as drawn in `cols` columns: what comes before the cursor,
/// the cell under it (a space at the end), and the rest — scrolled to keep
/// the cursor in view (`TextInput::view`), never a bidi mark (the path keeps
/// them, a real name may have them; drawn, a right-to-left override
/// reversed the row in a bidi-aware terminal).
pub(crate) fn shown_path(field: &crate::text_input::TextInput, cols: usize) -> (String, String, String) {
    use unicode_segmentation::UnicodeSegmentation;
    let (shown, col) = field.view(cols);
    let shown = crate::ansi::sanitize_display(&shown);
    let mut used = 0;
    let mut split = shown.len();
    for (i, g) in shown.grapheme_indices(true) {
        if used >= col {
            split = i;
            break;
        }
        used += crate::ansi::grapheme_width(g);
    }
    let (before, rest) = shown.split_at(split);
    let mut gs = rest.graphemes(true);
    let at = gs.next().unwrap_or(" ").to_string();
    (before.to_string(), at, gs.collect())
}

/// Where a frame puts its text: the lines (each cut to the window and
/// centred in the block), the block's first row and columns, and the carve
/// — the block plus a margin round it, which the sky never paints.
struct Layout {
    text: Vec<Line<'static>>,
    top: usize,
    left: usize,
    width: usize,
    /// Columns `x0..x1`, rows `y0..y1`.
    carve: (usize, usize, usize, usize),
}

/// Columns and rows of clear margin round the text.
const CARVE_MARGIN: (usize, usize) = (3, 1);

fn layout(t: f32, width: usize, rows: usize, view: &View) -> Layout {
    let fade = fade_at(t);
    let big = width >= BIG_MIN.0 && rows + 1 >= BIG_MIN.1;
    let lines = text_lines(view, width, big, t, fade);
    let lines: Vec<Line<'static>> = lines.into_iter().take(rows).map(|l| tui::truncate_ellipsis(l, width)).collect();
    let block_w = lines.iter().map(Cols::cols).max().unwrap_or(0);
    // Each line centred in the block, padded to its width.
    let text = lines
        .into_iter()
        .map(|l| {
            let w = l.cols();
            let left = (block_w - w) / 2;
            let mut row = LineBuf::new();
            row.spaces(left).append(l).reset().spaces(block_w - w - left);
            row.into_line()
        })
        .collect::<Vec<_>>();
    // The text sits low; the ripples spread in the sky above it.
    let top = rows.saturating_sub(text.len() + rows / 6);
    let left = (width - block_w) / 2;
    let carve = (
        left.saturating_sub(CARVE_MARGIN.0),
        (left + block_w + CARVE_MARGIN.0).min(width),
        top.saturating_sub(CARVE_MARGIN.1),
        (top + text.len() + CARVE_MARGIN.1).min(rows),
    );
    Layout { text, top, left, width: block_w, carve }
}

/// One frame, `t` seconds after the screen opened, for a `width`×`height`
/// window: exactly `height - 1` lines (the bottom row stays empty), none
/// wider than `width`.
pub fn frame(t: f32, width: usize, height: usize, view: &View) -> Vec<Line<'static>> {
    let rows = height.saturating_sub(1).max(1);
    let width = width.max(1);
    let fade = fade_at(t);
    let lay = layout(t, width, rows, view);
    let (x0, x1, y0, y1) = lay.carve;
    let kind = view.kind;
    // The ripples centre in the sky above the text.
    let sky_h = lay.top.max(4) as f32;
    let geometry = (width as f32 / 2.0, sky_h / 2.0, (width as f32 * 0.5).max(1.0));

    let mut out = Vec::with_capacity(rows);
    for y in 0..rows {
        let mut line = LineBuf::new();
        let text_row = y.checked_sub(lay.top).and_then(|i| lay.text.get(i));
        let mut x = 0;
        while x < width {
            if let Some(row) = text_row.filter(|_| x == lay.left) {
                line.append(row.clone());
                x += lay.width.max(1);
                continue;
            }
            let carved = (x0..x1).contains(&x) && (y0..y1).contains(&y);
            match (!carved).then(|| sky_cell(x, y, t, fade, geometry, kind)).flatten() {
                Some((ch, c)) => line.fg(c).push(ch),
                None => line.reset().push(' '),
            };
            x += 1;
        }
        out.push(line.into_line());
    }
    out
}

/// What the sky shows at one cell: a ripple glyph, a star, or nothing.
fn sky_cell(x: usize, y: usize, t: f32, fade: f32, (cx, cy, scale): (f32, f32, f32), kind: ThemeKind) -> Option<(char, Rgb)> {
    let u = (x as f32 + 0.5 - cx) / scale;
    let v = (y as f32 + 0.5 - cy) * CELL_ASPECT / scale;
    let d = ripple(u, v, t) * fade;
    if d >= FLOOR {
        let i = (((d - FLOOR) / (1.0 - FLOOR)).clamp(0.0, 1.0) * (RAMP.len() - 1) as f32).round() as usize;
        let r = (u * u + v * v).sqrt().min(1.5);
        return Some((RAMP[i], field_colour(i, hue_at(kind, t, r * RADIAL_HUE))));
    }
    // Empty sky: sparse stars, each on its own twinkle.
    let h = hash(x as i32, y as i32, 7_919);
    if !h.is_multiple_of(61) {
        return None;
    }
    let phase = ((h >> 8) % 16) as f32 * 0.4;
    let tw = ((t * 1.8 + phase).sin() * 0.5 + 0.5) * fade;
    if tw < 0.4 {
        None
    } else if (h >> 4).is_multiple_of(97) {
        Some(('+', hsv(hue_at(kind, t, MARK_SPAN), 0.5, 1.0)))
    } else if tw > 0.85 {
        Some(('·', STAR_LIT))
    } else {
        Some(('.', STAR_DIM))
    }
}

/// A path as typed, pasted or dropped onto the terminal: surrounding quotes
/// removed, backslash-escaped spaces unescaped (Unix terminals drop paths
/// that way), and a leading `~` expanded. None when nothing is left.
pub fn parse_typed_path(s: &str) -> Option<PathBuf> {
    let mut p = s.trim().to_string();
    for q in ['"', '\''] {
        if p.len() >= 2 && p.starts_with(q) && p.ends_with(q) {
            p = p[1..p.len() - 1].to_string();
        }
    }
    // On Windows a backslash is the separator, not an escape.
    if !cfg!(windows) {
        let mut un = String::with_capacity(p.len());
        let mut chars = p.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(n) = chars.next() {
                    un.push(n);
                }
            } else {
                un.push(c);
            }
        }
        p = un;
    }
    let p = p.trim();
    if p.is_empty() {
        return None;
    }
    if p == "~" || p.starts_with("~/") || p.starts_with("~\\") {
        if let Some(home) = dirs_home() {
            return Some(home.join(p[1..].trim_start_matches(['/', '\\'])));
        }
    }
    Some(PathBuf::from(p))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Run the screen until a folder is chosen (Some) or the user quits (None).
/// Owns the terminal while it runs: raw mode, the alternate screen and a
/// hidden cursor, all given back before it returns. `check` says why a
/// path has nothing to play (Err): the screen stays, the path still typed
/// (it ended Keet with "No audio files found"). The folder dialog runs on
/// its own thread (`pick_folder`), the screen still drawn meanwhile.
pub fn run(
    kind: ThemeKind,
    native: bool,
    pick_folder: fn() -> Option<PathBuf>,
    check: impl Fn(&std::path::Path) -> Result<(), String>,
) -> Option<PathBuf> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal;

    if terminal::enable_raw_mode().is_err() {
        return None;
    }
    let mut stdout = std::io::stdout();
    crate::enter_alternate_screen(&mut stdout);
    let mut term = match crate::tui::terminal() {
        Ok(t) => t,
        Err(_) => {
            crate::leave_alternate_screen(&mut stdout);
            let _ = terminal::disable_raw_mode();
            return None;
        }
    };
    let palette = crate::theme::palette(kind);
    let opened = Instant::now();
    let mut mode = Mode::Keys;
    let mut note: Option<Note> = None;
    // The folder dialog's answer, while it is open.
    let mut picking: Option<std::sync::mpsc::Receiver<Option<PathBuf>>> = None;
    // A path to play, or the note saying why not.
    let take = |p: PathBuf| -> Result<PathBuf, Note> {
        if !p.exists() {
            return Err(Note { text: format!("Not found: {}", p.display()), error: true });
        }
        check(&p).map(|()| p).map_err(|why| Note { text: why, error: true })
    };

    let chosen = 'screen: loop {
        let view = View { kind, palette, native, mode: &mode, note: note.as_ref() };
        let t = opened.elapsed().as_secs_f32();
        crate::term::out!("\x1B[?2026h");
        let _ = term.draw(|f| {
            let area = f.area();
            let lines = frame(t, area.width as usize, area.height as usize, &view);
            tui::draw_lines(f.buffer_mut(), area, &lines);
        });
        crate::term::out!("\x1B[?2026l");
        crate::term::flush();

        // Wait out the rest of the frame, handling every key that arrives.
        let deadline = Instant::now() + FRAME;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if !crate::input::poll(left) {
                break;
            }
            let Ok(ev) = crate::input::read() else { break 'screen None };
            // A pasted or dropped path goes into the typed one.
            if let Event::Paste(text) = &ev {
                match &mut mode {
                    Mode::Typing(field) => field.insert(&crate::text_input::clean(text)),
                    // A fresh path: the spaces round it mean nothing either.
                    Mode::Keys => mode = Mode::Typing(crate::text_input::TextInput::with(crate::text_input::clean(text).trim())),
                }
                note = None;
                continue;
            }
            let Event::Key(k) = ev else { continue };
            if k.kind == KeyEventKind::Release {
                continue;
            }
            if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                break 'screen None;
            }
            // The dialog has the choice: keys wait for it.
            if picking.is_some() {
                continue;
            }
            match &mut mode {
                Mode::Keys => match k.code {
                    KeyCode::Char('q' | 'Q') | KeyCode::Esc => break 'screen None,
                    KeyCode::Char('t' | 'T') => {
                        mode = Mode::Typing(Default::default());
                        note = None;
                    }
                    KeyCode::Char('p' | 'P') if native => {
                        note = Some(Note { text: "Choosing a folder in the dialog…".into(), error: false });
                        let (tx, rx) = std::sync::mpsc::channel();
                        std::thread::spawn(move || {
                            let _ = tx.send(pick_folder());
                        });
                        picking = Some(rx);
                    }
                    _ => {}
                },
                Mode::Typing(field) => match field.handle(k) {
                    Edit::Cancel => {
                        mode = Mode::Keys;
                        note = None;
                    }
                    Edit::Submit => match parse_typed_path(field.text()).map(take) {
                        Some(Ok(p)) => break 'screen Some(p),
                        Some(Err(why)) => note = Some(why),
                        None => {}
                    },
                    _ => {}
                },
            }
        }
        if let Some(answer) = picking.as_ref().and_then(|rx| rx.try_recv().ok()) {
            picking = None;
            match answer.map(take) {
                Some(Ok(p)) => break 'screen Some(p),
                Some(Err(why)) => note = Some(why),
                None => note = Some(Note { text: "No folder chosen.".into(), error: false }),
            }
        }
    };

    crate::term::out!("\x1B[?25h");
    crate::term::flush();
    crate::leave_alternate_screen(&mut stdout);
    let _ = terminal::disable_raw_mode();
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_typed_path_can_be_edited_where_the_cursor_is() {
        use crate::text_input::TextInput;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let mut field = TextInput::new();
        for c in "/Users/me/Musc".chars() {
            field.handle(key(KeyCode::Char(c)));
        }
        field.handle(key(KeyCode::Left));
        field.handle(key(KeyCode::Char('i')));
        assert_eq!(field.text(), "/Users/me/Music", "Left moved the cursor, and `i` went there");
        // Drawn with the cursor where it is: on the `c`.
        field.handle(key(KeyCode::Left));
        let typing = Mode::Typing(field);
        let lines = text_lines(&view(&typing, None, ThemeKind::Classic), 40, false, 1.0, 1.0);
        let row = lines.iter().find(|l| tui::plain(l).starts_with("path ›")).expect("the prompt");
        let reversed: Vec<&str> = row.spans.iter().filter(|s| s.style.add_modifier.contains(Modifier::REVERSED)).map(|s| s.content.as_ref()).collect();
        assert_eq!(reversed, ["i"], "{row:?}");
    }

    #[test]
    fn the_typed_path_takes_altgr_and_backspace_takes_a_whole_character() {
        use crate::text_input::TextInput;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |c, m| KeyEvent::new(c, m);
        let mut f = TextInput::with("C:");
        // Windows reports AltGr as Ctrl+Alt: `\` on a German layout.
        f.handle(key(KeyCode::Char('\\'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert_eq!(f.text(), "C:\\", "AltGr types");
        // Backspace takes what shows as one character.
        let mut f = TextInput::with("/Mu\u{301}");
        f.handle(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(f.text(), "/M", "the accented u whole");
        let mut f = TextInput::with("/a\u{2764}\u{FE0F}");
        f.handle(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(f.text(), "/a", "the emoji with its selector");
    }

    fn view<'a>(mode: &'a Mode, note: Option<&'a Note>, kind: ThemeKind) -> View<'a> {
        View { kind, palette: crate::theme::palette(kind), native: true, mode, note }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(tui::plain).collect()
    }

    /// The typed path gets every column the prompt leaves (it was measured
    /// in bytes: `›` is 3 bytes in 1 column, and the path was cut 2 early).
    #[test]
    fn a_typed_path_that_fits_is_shown_whole() {
        let path = "/Music/Kind of Blue/12"; // 22 columns: "path › " + it + the cursor = 30
        let typing = Mode::Typing(crate::text_input::TextInput::with(path));
        let lines = plain(&text_lines(&view(&typing, None, ThemeKind::Classic), 30, false, 1.0, 1.0));
        let row = lines.iter().find(|l| l.starts_with("path ›")).expect("the prompt");
        assert!(row.contains(path), "cut though it fits: {row:?}");
    }

    /// A typed path that ratatui's `Line::width` measures narrower than it
    /// is drawn (halfwidth kana with sound marks) is laid out by the drawn
    /// width: mixing the two underflowed the centring and crashed the screen.
    #[test]
    fn a_path_of_halfwidth_kana_lays_out_without_overflow() {
        let typing = Mode::Typing(crate::text_input::TextInput::with(&format!("/{}", "\u{FF76}\u{FF9E}".repeat(60))));
        for kind in [ThemeKind::Classic, ThemeKind::Minimal, ThemeKind::HiFi] {
            for w in [20, 40, 60, 80, 120] {
                for h in [12, 24, 40] {
                    for l in frame(3.7, w, h, &view(&typing, None, kind)) {
                        assert!(crate::tui::Cols::cols(&l) <= w, "{kind:?} {w}x{h}: {l:?}");
                    }
                }
            }
        }
    }

    /// Every size a drag passes through: exactly height - 1 lines, none
    /// wider than the window, in every theme and both modes.
    #[test]
    fn every_size_fills_the_window_and_never_overflows() {
        let typing = Mode::Typing(crate::text_input::TextInput::with("/Users/someone/Music/a very long folder name/ünïcödé/日本語"));
        let note = Note { text: "Not found: /nowhere/at/all/with/a/long/path".into(), error: true };
        for kind in [ThemeKind::Classic, ThemeKind::Minimal, ThemeKind::HiFi] {
            for (mode, note) in [(&Mode::Keys, None), (&typing, Some(&note))] {
                for w in 1..=130 {
                    for h in 1..=45 {
                        let lines = frame(3.7, w, h, &view(mode, note, kind));
                        assert_eq!(lines.len(), h.saturating_sub(1).max(1), "{w}x{h}");
                        for l in &lines {
                            assert!(crate::tui::Cols::cols(l) <= w, "{w}x{h}: {} > {w}: {l:?}", crate::tui::Cols::cols(l));
                        }
                    }
                }
            }
        }
    }

    /// The same instant draws the same frame: the scene is time alone.
    #[test]
    fn a_frame_is_a_function_of_time() {
        let m = Mode::Keys;
        let v = view(&m, None, ThemeKind::Minimal);
        assert_eq!(frame(2.5, 100, 30, &v), frame(2.5, 100, 30, &v));
        assert_ne!(frame(2.5, 100, 30, &v), frame(2.6, 100, 30, &v), "and it moves");
    }

    /// The ripples and stars never touch the text or the margin round it,
    /// so the words always sit on a clear ground: every carved cell is
    /// either text or a space.
    #[test]
    fn nothing_is_drawn_round_the_text() {
        let typing = Mode::Typing(crate::text_input::TextInput::with("/Music/Kind of Blue"));
        let note = Note { text: "Not found: /Music/Kind of Blue".into(), error: true };
        for (mode, note) in [(&Mode::Keys, None), (&typing, Some(&note))] {
            let v = view(mode, note, ThemeKind::HiFi);
            for (w, h) in [(100, 34), (60, 20), (44, 12)] {
                for t in [0.5_f32, 2.0, 9.0, 31.0] {
                    let lay = layout(t, w, h - 1, &v);
                    let lines = plain(&frame(t, w, h, &v));
                    let (x0, x1, y0, y1) = lay.carve;
                    for (y, line) in lines.iter().enumerate().take(y1).skip(y0) {
                        let chars: Vec<char> = line.chars().collect();
                        let text: Vec<char> = match y.checked_sub(lay.top).and_then(|i| lay.text.get(i)) {
                            Some(row) => tui::plain(row).chars().collect(),
                            None => Vec::new(),
                        };
                        for x in x0..x1 {
                            let in_text = x >= lay.left && x < lay.left + text.len();
                            if !in_text {
                                assert_eq!(chars.get(x), Some(&' '), "{w}x{h} t={t} ({x},{y}): {line:?}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// Big windows get the block letters, small ones the one-line name; both
    /// always show the keys.
    #[test]
    fn small_windows_get_the_name_and_still_the_keys() {
        let m = Mode::Keys;
        let v = view(&m, None, ThemeKind::Classic);
        let big = plain(&frame(5.0, 80, 24, &v)).join("\n");
        assert!(big.contains("██") && !big.contains("◆ Keet"), "{big}");
        let small = plain(&frame(5.0, 44, 12, &v)).join("\n");
        assert!(small.contains("◆ Keet") && !small.contains("██"), "{small}");
        for screen in [big, small] {
            assert!(screen.contains("pick a folder") && screen.contains("type a path") && screen.contains("quit"));
        }
    }

    /// The scene starts black and fades in: the first frame draws only text.
    #[test]
    fn the_scene_fades_in_from_black() {
        let m = Mode::Keys;
        let v = view(&m, None, ThemeKind::Minimal);
        let first = plain(&frame(0.0, 100, 30, &v));
        let sky: String = first[..10].concat();
        assert!(sky.trim().is_empty(), "{sky:?}");
        let later = plain(&frame(4.0, 100, 30, &v));
        assert!(later[..10].iter().any(|l| !l.trim().is_empty()), "the ripples are up by then");
    }

    /// The colours start at the theme's own and go once round the wheel
    /// every CYCLE_SECS: the same cell differs half a cycle on, and is back
    /// where it began a whole cycle on.
    #[test]
    fn the_colours_cycle() {
        for kind in [ThemeKind::Classic, ThemeKind::Minimal, ThemeKind::HiFi] {
            assert_eq!(hue_at(kind, 0.0, 0.0), base_hue(kind));
            let at = |t: f32| field_colour(5, hue_at(kind, t, 0.0));
            assert_ne!(at(1.0), at(1.0 + CYCLE_SECS / 2.0), "{kind:?}");
            let (a, b) = (at(1.0), at(1.0 + CYCLE_SECS));
            let near = |x: u8, y: u8| x.abs_diff(y) <= 1;
            assert!(near(a.0, b.0) && near(a.1, b.1) && near(a.2, b.2), "{kind:?}: {a:?} vs {b:?}");
        }
        // The primaries land where they should.
        assert_eq!(hsv(0.0, 1.0, 1.0), (255, 0, 0));
        assert_eq!(hsv(120.0, 1.0, 1.0), (0, 255, 0));
        assert_eq!(hsv(240.0, 1.0, 1.0), (0, 0, 255));
        assert_eq!(hsv(360.0, 1.0, 1.0), (255, 0, 0));
    }

    /// A long typed path keeps the cursor's end on screen, cut between
    /// whole characters as drawn, never wider than its room.
    #[test]
    fn a_long_path_shows_its_end() {
        use crate::text_input::TextInput;
        let (b, a, rest) = shown_path(&TextInput::with("/music/albums/kind of blue"), 10);
        assert_eq!((b.as_str(), a.as_str(), rest.as_str()), ("… of blue", " ", ""));
        let couple = "\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F468}";
        for path in [format!("/a/{couple}{couple}b"), "/ｶﾞｶﾞｶﾞｶﾞ".to_string(), "/x\u{2764}\u{FE0F}y".to_string(), "/x/日本語の歌".to_string()] {
            for cols in 3..12 {
                let (b, a, rest) = shown_path(&TextInput::with(&path), cols);
                let w = crate::tui::width(&b) + crate::tui::width(&a) + crate::tui::width(&rest);
                assert!(w <= cols, "{b:?}{a:?}{rest:?} in {cols}");
                let kept = b.trim_start_matches('…');
                assert!(path.ends_with(kept), "{kept:?} is not a tail of {path:?}");
                assert!(!kept.starts_with('\u{FE0F}') && !kept.starts_with('\u{200D}') && !kept.starts_with('\u{FF9E}'), "{b:?} starts mid-character");
            }
        }
        assert!(!shown_path(&TextInput::with("~/\u{202E}x"), 40).0.contains('\u{202E}'), "no bidi mark drawn");
    }

    #[test]
    fn dropped_and_typed_paths_are_cleaned_up() {
        assert_eq!(parse_typed_path("  /Music/Jazz  "), Some(PathBuf::from("/Music/Jazz")));
        assert_eq!(parse_typed_path("'/Music/Kind of Blue'"), Some(PathBuf::from("/Music/Kind of Blue")));
        assert_eq!(parse_typed_path("\"/Music/x\""), Some(PathBuf::from("/Music/x")));
        assert_eq!(parse_typed_path("   "), None);
        assert_eq!(parse_typed_path("''"), None);
        if !cfg!(windows) {
            assert_eq!(parse_typed_path("/Music/Kind\\ of\\ Blue "), Some(PathBuf::from("/Music/Kind of Blue")));
        }
        if let Some(home) = dirs_home() {
            assert_eq!(parse_typed_path("~/Music"), Some(home.join("Music")));
            assert_eq!(parse_typed_path("~"), Some(home.join("")));
        }
    }

    /// Look at the scene: `cargo test welcome::tests::show -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn show() {
        let m = Mode::Keys;
        for (t, kind) in [(0.6_f32, ThemeKind::HiFi), (3.0, ThemeKind::Minimal), (8.0, ThemeKind::Classic)] {
            let v = view(&m, None, kind);
            for l in plain(&frame(t, 100, 32, &v)) {
                println!("|{l}");
            }
            println!();
        }
    }
}
