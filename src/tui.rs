//! The terminal UI's plumbing on ratatui: the backend, the styled-line
//! builder every renderer uses, fitting for styled lines, and the image
//! payloads placed over blank cells after each draw.
//!
//! ratatui keeps the whole screen as a buffer of cells and, after each draw,
//! sends the terminal only the cells that changed. Three things here keep
//! that compatible with how Keet has to talk to terminals:
//!
//! - The backend ([`KeetBackend`]) writes into `term`'s frame buffer, so the
//!   changed cells, the images placed over them and the synchronized-update
//!   markers around them still leave in one write (and `NO_COLOR` is still
//!   applied in one place, `term::flush`). It never asks the terminal where
//!   the cursor is: ratatui's `Terminal::clear` does, with `CSI 6n`, and the
//!   reply arrives on stdin as keystrokes (CLAUDE.md, "never query the
//!   terminal"). Its clear is `ESC[H ESC[J`, never `ESC[2J`, which ConPTY
//!   implements by scrolling the screen into scrollback.
//! - Images (the cover, the analysis spectrogram) in Kitty, Sixel or iTerm2
//!   are not cells. Their renderers leave the cells blank and hand the escape
//!   sequence to [`place_image`]; [`emit_images`] writes each at its position
//!   after ratatui's own output. A payload of `None` means "unchanged": the
//!   cells stay blank in both buffers, ratatui never writes them, and the
//!   image already on screen stays.
//! - Sixel and iTerm2 pixels ARE cell content to the terminal, but ratatui
//!   thinks those cells are blank. When such an image moves or goes away,
//!   the cells it covered may still show it while ratatui believes they are
//!   empty and never rewrites them; [`stale_image_cells`] tells the caller to
//!   clear and redraw the whole screen.

use std::cell::RefCell;
use std::io;

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::{Palette, Rgb};

/// The terminal Keet draws on.
pub type Term = ratatui::Terminal<KeetBackend>;

/// ratatui's crossterm backend writing into `term`'s frame buffer, with the
/// two behaviours Keet cannot have replaced (see the module docs).
pub struct KeetBackend {
    inner: CrosstermBackend<crate::term::Writer>,
}

/// A terminal for the current window. Creating one writes nothing.
pub fn terminal() -> io::Result<Term> {
    ratatui::Terminal::new(KeetBackend { inner: CrosstermBackend::new(crate::term::Writer) })
}

/// The window's size in cells. Some pseudo-terminals (`script`) report 0×0;
/// a frame laid out for nothing would draw nothing, so that reads as 80×24.
pub fn window() -> (u16, u16) {
    window_from(crossterm::terminal::size())
}

fn window_from(size: io::Result<(u16, u16)>) -> (u16, u16) {
    match size {
        Ok((w, h)) if w > 0 && h > 0 => (w, h),
        _ => (80, 24),
    }
}

impl Backend for KeetBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    /// Never asked of the terminal (see the module docs). ratatui only uses
    /// it to put the cursor back after a clear, which Keet does not need.
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(Position::ORIGIN)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.clear_region(ClearType::All)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        match clear_type {
            // Home + erase down, not ED2 (ConPTY scrolls the screen away).
            ClearType::All => {
                crate::term::out!("\x1B[0m\x1B[H\x1B[J");
                Ok(())
            }
            other => self.inner.clear_region(other),
        }
    }

    fn size(&self) -> io::Result<Size> {
        let (width, height) = window();
        Ok(Size { width, height })
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize { columns_rows: self.size()?, pixels: Size { width: 0, height: 0 } })
    }

    /// The frame leaves in `term::flush`, after the images placed over it.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

static MOUSE_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn mouse reporting on or off (crossterm's commands: escape sequences on
/// Unix, the console mode on Windows). Off only undoes an on, so leaving the
/// alternate screen can always call it. Whatever is queued for the frame goes
/// out first.
pub fn set_mouse(on: bool) {
    use std::sync::atomic::Ordering;
    if MOUSE_ON.swap(on, Ordering::Relaxed) == on {
        return;
    }
    crate::term::flush();
    let mut out = io::stdout();
    let _ = if on && !cfg!(windows) {
        // Clicks, drags (button-event tracking) and SGR coordinates — not
        // crossterm's any-motion tracking (?1003), which woke the loop for
        // every movement over the window. (Windows: the console mode.)
        io::Write::write_all(&mut out, mouse_on_sequence().as_bytes()).and_then(|_| io::Write::flush(&mut out))
    } else if on {
        crossterm::execute!(out, crossterm::event::EnableMouseCapture)
    } else {
        crossterm::execute!(out, crossterm::event::DisableMouseCapture)
    };
}

/// Mouse reporting as Keet asks for it (see `set_mouse`).
fn mouse_on_sequence() -> &'static str {
    "\x1B[?1000h\x1B[?1002h\x1B[?1006h"
}

/// Turn mouse reporting off on the way out, writing to `w` directly: the
/// panic hook calls this, and going through `term::flush` (as `set_mouse`
/// does) would borrow a frame buffer a panic may still hold — a second panic
/// in the hook aborts with the terminal in raw mode.
pub fn release_mouse(w: &mut impl io::Write) {
    use std::sync::atomic::Ordering;
    if MOUSE_ON.swap(false, Ordering::Relaxed) {
        let _ = crossterm::execute!(w, crossterm::event::DisableMouseCapture);
    }
}

/// A truecolor foreground.
pub fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// A style that only sets a truecolor foreground.
pub fn fg(c: Rgb) -> Style {
    Style::new().fg(rgb(c))
}

/// A styled line built a piece at a time, the way the renderers used to
/// build strings with colour codes in them: set a style, push text, set
/// another. Runs of one style become one span.
#[derive(Default)]
pub struct LineBuf {
    spans: Vec<Span<'static>>,
    /// (spans measured, their columns), caught up by `width()` when asked:
    /// a key bar asks after every hint, and measuring every span again each
    /// time made building one quadratic; measuring each span as it closed
    /// taxed every line drawn, though only key bars ask.
    measured: std::cell::Cell<(usize, usize)>,
    text: String,
    style: Style,
}

impl LineBuf {
    pub fn new() -> Self {
        Self::default()
    }

    fn end_span(&mut self) {
        if !self.text.is_empty() {
            self.spans.push(Span::styled(std::mem::take(&mut self.text), self.style));
        }
    }

    /// What the following text is drawn in.
    pub fn style(&mut self, style: Style) -> &mut Self {
        if style != self.style {
            self.end_span();
            self.style = style;
        }
        self
    }

    /// Truecolor foreground for what follows.
    pub fn fg(&mut self, c: Rgb) -> &mut Self {
        self.style(fg(c))
    }

    /// The terminal's own colours and no attributes for what follows.
    pub fn reset(&mut self) -> &mut Self {
        self.style(Style::new())
    }

    pub fn push(&mut self, c: char) -> &mut Self {
        self.text.push(c);
        self
    }

    pub fn push_str(&mut self, s: &str) -> &mut Self {
        self.text.push_str(s);
        self
    }

    /// `n` spaces in the current style.
    pub fn spaces(&mut self, n: usize) -> &mut Self {
        self.text.extend(std::iter::repeat_n(' ', n));
        self
    }

    /// `s` in `style`; the style then stays for what follows.
    pub fn styled(&mut self, s: &str, style: Style) -> &mut Self {
        self.style(style).push_str(s)
    }

    /// Every span of `line` (each in its own style, patched over the line's).
    pub fn append(&mut self, line: Line<'static>) -> &mut Self {
        self.end_span();
        let base = line.style;
        for span in line.spans {
            let style = base.patch(span.style);
            self.spans.push(Span::styled(span.content, style));
        }
        self
    }

    /// Columns so far.
    pub fn width(&self) -> usize {
        let (n, w) = self.measured.get();
        let w = w + self.spans[n..].iter().map(Cols::cols).sum::<usize>();
        self.measured.set((self.spans.len(), w));
        w + width(&self.text)
    }

    pub fn into_line(mut self) -> Line<'static> {
        self.end_span();
        Line::from(self.spans)
    }
}

impl From<LineBuf> for Line<'static> {
    fn from(b: LineBuf) -> Self {
        b.into_line()
    }
}

/// One span as a line.
pub fn line(text: impl Into<String>, style: Style) -> Line<'static> {
    Line::from(Span::styled(text.into(), style))
}

/// A line's text without its styles.
pub fn plain(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Columns a line or span takes AS DRAWN. Use it, not ratatui's own
/// `Line::width`/`Span::width`, for layout: those measure the whole string
/// at once and disagree with the drawing (per grapheme, `CellWidth`) on
/// halfwidth kana with sound marks, ｶﾞ (1 column measured, 2 drawn).
pub trait Cols {
    fn cols(&self) -> usize;
}

impl Cols for Span<'_> {
    fn cols(&self) -> usize {
        width(&self.content)
    }
}

impl Cols for Line<'_> {
    fn cols(&self) -> usize {
        self.spans.iter().map(Cols::cols).sum()
    }
}

/// Columns a string takes, measured as ratatui draws it.
pub fn width(s: &str) -> usize {
    crate::ansi::plain_width(s)
}

/// `text` broken into lines of at most `width` columns, between words where
/// it can, through a word only when the word alone is wider than a line.
/// Measured and cut as ratatui draws it (`ansi::plain_width`, and a word
/// by `ansi::split_columns`, between graphemes: CJK is two columns, flags
/// and joined emoji are never split). Plain text: escapes are not interpreted (lyrics
/// are sanitized where they are read). Spaces at either end, and where a
/// row breaks, are dropped (a centred row is centred on its text); spaces
/// inside a row are kept as written, so a line reads the same at every
/// width. Always at least one line.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    use crate::ansi::{plain_width, split_columns};
    let width = width.max(1);
    let text = text.trim();
    if plain_width(text) <= width {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_w = 0;
    let mut rest = text;
    while !rest.is_empty() {
        // The next word and the spaces before it.
        let after_gap = rest.trim_start();
        let gap = &rest[..rest.len() - after_gap.len()];
        let end = after_gap.find(char::is_whitespace).unwrap_or(after_gap.len());
        let word = &after_gap[..end];
        rest = &after_gap[end..];
        let word_w = plain_width(word);
        let gap_w = if line.is_empty() { 0 } else { plain_width(gap) };
        if line_w + gap_w + word_w <= width {
            if !line.is_empty() {
                line.push_str(gap);
            }
            line.push_str(word);
            line_w += gap_w + word_w;
            continue;
        }
        // A new row for it, after the words so far; a word wider than a
        // whole row takes whole rows of it, the rest carried on to the next.
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        let mut pieces = split_columns(word, width);
        let tail = pieces.pop().unwrap_or_default();
        lines.extend(pieces.into_iter().map(String::from));
        line.push_str(tail);
        line_w = plain_width(tail);
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// `line` cut to at most `max` columns: whole characters only (a wide one
/// that would straddle the edge is left out), styles kept.
pub fn truncate(line: Line<'static>, max: usize) -> Line<'static> {
    if line.cols() <= max {
        return line;
    }
    let base = line.style;
    let mut out = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let w = span.cols();
        if used + w <= max {
            used += w;
            out.push(span);
            continue;
        }
        let cut = crate::ansi::truncate_ansi(&span.content, max - used);
        if !cut.is_empty() {
            out.push(Span::styled(cut, span.style));
        }
        break;
    }
    Line::from(out).style(base)
}

/// `line` cut to `max` columns with a trailing `…` when anything was cut
/// (in the style of what it follows), as `ansi::truncate_plain` does.
pub fn truncate_ellipsis(line: Line<'static>, max: usize) -> Line<'static> {
    if line.cols() <= max {
        return line;
    }
    if max <= 1 {
        return truncate(line, max);
    }
    let mut cut = truncate(line, max - 1);
    let style = cut.spans.last().map(|s| s.style).unwrap_or_default();
    cut.spans.push(Span::styled("…", style));
    cut
}

/// `line` cut or padded with spaces to exactly `width` columns.
pub fn pad(line: Line<'static>, width: usize) -> Line<'static> {
    let mut line = truncate(line, width);
    let fill = width.saturating_sub(line.cols());
    if fill > 0 {
        line.spans.push(Span::raw(" ".repeat(fill)));
    }
    line
}

/// As many whole items as fit in `width`, joined by `sep`; the first is cut
/// with an ellipsis when not even it fits (`ansi::fit_segments` for styled
/// lines: a narrow window drops whole items from the end).
pub fn fit_segments(items: &[Line<'static>], sep: &Line<'static>, width: usize) -> Line<'static> {
    let sep_w = sep.cols();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (i, item) in items.iter().enumerate() {
        let w = item.cols();
        if i == 0 {
            if w > width {
                return truncate_ellipsis(item.clone(), width);
            }
        } else if used + sep_w + w > width {
            break;
        } else {
            out.extend(sep.spans.iter().cloned());
            used += sep_w;
        }
        out.extend(item.spans.iter().cloned());
        used += w;
    }
    Line::from(out)
}

/// A list's cursor row: `row` as plain text on the theme's highlight, cut
/// (with an ellipsis) or padded to exactly `width` columns. The row's own
/// colours are dropped, so the highlight runs unbroken across it; under
/// NO_COLOR it is reverse video.
pub fn cursor_row(p: &Palette, row: &Line, width: usize) -> Line<'static> {
    let text = crate::ansi::truncate_plain(&plain(row), width);
    let fill = width.saturating_sub(self::width(&text));
    let style = if crate::term::no_color() { Style::new().add_modifier(Modifier::REVERSED) } else { p.cursor_hl };
    Line::from(Span::styled(format!("{text}{}", " ".repeat(fill)), style))
}

/// Draw `lines` into `area`, one per row from its top, each cut at its right
/// edge; lines past its bottom are dropped.
pub fn draw_lines(buf: &mut Buffer, area: Rect, lines: &[Line]) {
    for (i, line) in lines.iter().enumerate().take(area.height as usize) {
        set_line(buf, area.x, area.y + i as u16, line, area.width);
    }
}

use crate::ansi::single_cell;

/// `Buffer::set_line`, with a short cut: runs of [`single_cell`] characters
/// go straight into their cells. ratatui splits every span into grapheme
/// clusters to measure them, which for a screen of braille is most of the
/// cost of drawing it; anything else still goes through that path.
fn set_line(buf: &mut Buffer, mut x: u16, y: u16, line: &Line, max_width: u16) {
    let right = (x.saturating_add(max_width)).min(buf.area.right());
    for span in line {
        let style = line.style.patch(span.style);
        let text = span.content.as_ref();
        let mut rest = text;
        while !rest.is_empty() && x < right {
            let mut fast = rest.find(|c: char| !single_cell(c)).unwrap_or(rest.len());
            // What follows may combine with the run's last character (e +
            // U+0301 is one grapheme): leave that one to ratatui too.
            if fast < rest.len() {
                fast = rest[..fast].char_indices().last().map_or(0, |(i, _)| i);
            }
            for c in rest[..fast].chars() {
                if x >= right {
                    return;
                }
                buf[(x, y)].set_char(c).set_style(style);
                x += 1;
            }
            rest = &rest[fast..];
            if rest.is_empty() {
                break;
            }
            // Up to the next fast character after this one, through
            // ratatui's own path.
            let first = rest.chars().next().map_or(0, char::len_utf8);
            let mut slow = rest[first..].find(single_cell).map_or(rest.len(), |i| first + i);
            // Never in the middle of a grapheme: one can run on into a fast
            // character (a prepended sign joins the letter after it, an
            // emoji's glyph the emoji before a joiner). On to its end.
            let mut cursor = unicode_segmentation::GraphemeCursor::new(slow, rest.len(), true);
            if slow < rest.len() && !cursor.is_boundary(rest, 0).unwrap_or(true) {
                slow = cursor.next_boundary(rest, 0).ok().flatten().unwrap_or(rest.len());
            }
            let (pos, _) = buf.set_stringn(x, y, &rest[..slow], (right - x) as usize, style);
            // A character it could not fit (a wide one at the edge) ends the
            // SPAN, as ratatui's own `set_line` does (the next span goes on
            // in what is left): carrying on drew the span's next narrow
            // character in the gap ("日a" for 日本 in 3 columns).
            let fits = crate::ansi::plain_width(&rest[..slow]) <= (right - x) as usize;
            x = pos;
            if !fits {
                break;
            }
            rest = &rest[slow..];
        }
        if x >= right {
            return;
        }
    }
}

/// The part of `area` from row `from` down, `rows` rows tall (both clamped
/// to it).
pub fn rows(area: Rect, from: usize, rows: usize) -> Rect {
    let from = (from as u16).min(area.height);
    let h = (rows as u16).min(area.height - from);
    Rect { x: area.x, y: area.y + from, width: area.width, height: h }
}

/// `area` without its first `n` columns.
pub fn indent(area: Rect, n: u16) -> Rect {
    let n = n.min(area.width);
    Rect { x: area.x + n, width: area.width - n, ..area }
}

/// A scrollbar down the right column of `area` (a list's rows), for a list
/// of `total` items showing `area.height` of them from `first`; nothing
/// when they all fit.
pub fn scrollbar(f: &mut ratatui::Frame, area: Rect, total: usize, first: usize, thumb: Style, track: Style) {
    use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
    let visible = area.height as usize;
    if total <= visible || area.width == 0 {
        return;
    }
    // Positions are the possible first rows, so the thumb is the visible
    // share of the list, where it is.
    let mut state = ScrollbarState::new(total - visible + 1).position(first).viewport_content_length(visible);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(Some("│"))
        .track_style(track)
        .thumb_symbol("┃")
        .thumb_style(thumb);
    f.render_stateful_widget(bar, area, &mut state);
}

/// Which image a placement is: each is kept, moved or removed on its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageSlot {
    Cover,
    Viz,
}

struct Placement {
    slot: ImageSlot,
    area: Rect,
    payload: Option<String>,
}

#[derive(Default)]
struct Images {
    /// Placed during this draw.
    now: Vec<Placement>,
    /// Where each image sat after the last draw that emitted.
    last: Vec<(ImageSlot, Rect)>,
}

thread_local! {
    static IMAGES: RefCell<Images> = RefCell::new(Images::default());
}

/// Whether a clear leaves Sixel pixels behind: Sixel in Windows Terminal
/// (see `Player::draw_status`, which starts a fresh alternate screen there
/// when the window changes size).
pub fn sixel_survives_clear() -> bool {
    sixel_survives_clear_with(crate::cover::detect_protocol(), std::env::var_os("WT_SESSION").is_some())
}

fn sixel_survives_clear_with(protocol: crate::cover::GraphicsProtocol, windows_terminal: bool) -> bool {
    protocol == crate::cover::GraphicsProtocol::Sixel && windows_terminal
}

/// Drop the images placed in this draw that a dialog's box (`boxes`)
/// overlaps, and say which: a Kitty image would cover the dialog, and a
/// Sixel/iTerm2 one under it would leave a hole once it closes. One clear of
/// every box stays (all of them used to go: the cover beside a 90-column
/// lyrics panel blanked). Called at the END of the draw, once the frame
/// knows its layout (whether a panel docked or floats is settled while
/// drawing).
pub fn withhold_images_under(boxes: &[Rect]) -> Vec<ImageSlot> {
    IMAGES.with(|i| {
        let mut i = i.borrow_mut();
        let covered = |p: &Placement| boxes.iter().any(|b| !b.intersection(p.area).is_empty());
        let withheld: Vec<ImageSlot> = i.now.iter().filter(|p| covered(p)).map(|p| p.slot).collect();
        i.now.retain(|p| !covered(p));
        withheld
    })
}

/// An image over the cells of `area`, which the caller leaves blank: its
/// escape sequence, written with the cursor at the area's top left, or None
/// when the one already there is unchanged.
pub fn place_image(slot: ImageSlot, area: Rect, payload: Option<String>) {
    IMAGES.with(|i| i.borrow_mut().now.push(Placement { slot, area, payload }));
}

/// Whether a Sixel or iTerm2 image moved or went away in this draw (see the
/// module docs): the caller clears the screen and draws again. Kitty images
/// are overlays the renderers delete by id, and half-block is cells.
pub fn stale_image_cells() -> bool {
    use crate::cover::GraphicsProtocol as GP;
    if !matches!(crate::cover::detect_protocol(), GP::Sixel | GP::Iterm2) {
        return false;
    }
    IMAGES.with(|i| {
        let i = i.borrow();
        i.last.iter().any(|&(slot, area)| !i.now.iter().any(|p| p.slot == slot && p.area == area))
    })
}

/// Forget this draw's placements (a second draw follows).
pub fn discard_images() {
    IMAGES.with(|i| i.borrow_mut().now.clear());
}

/// Whether the last draw that emitted left an image on screen.
pub fn images_on_screen() -> bool {
    IMAGES.with(|i| !i.borrow().last.is_empty())
}

/// Whether this draw placed an image in `slot` (before `emit_images`;
/// withheld ones are not placed).
pub fn placed(slot: ImageSlot) -> bool {
    IMAGES.with(|i| i.borrow().now.iter().any(|p| p.slot == slot))
}

/// Forget where images were: after a full clear nothing is on screen.
pub fn forget_images() {
    IMAGES.with(|i| i.borrow_mut().last.clear());
}

/// Write this draw's image payloads at their places, after ratatui's output.
pub fn emit_images() {
    IMAGES.with(|i| {
        let mut i = i.borrow_mut();
        let now = std::mem::take(&mut i.now);
        i.last = now.iter().map(|p| (p.slot, p.area)).collect();
        crate::term::out!("{}", images_escape(now));
    });
}

/// The escapes that put `now`'s payloads at their places, the cursor saved
/// before and put back after (DECSC/DECRC): ratatui has already placed it in
/// the open text field, and an image left it on itself.
fn images_escape(now: Vec<Placement>) -> String {
    let mut out = String::new();
    for p in now {
        if let Some(payload) = p.payload {
            out.push_str(&format!("\x1B[0m\x1B[{};{}H{payload}", p.area.y + 1, p.area.x + 1));
        }
    }
    if out.is_empty() { out } else { format!("\x1B7{out}\x1B8") }
}

/// Test helpers: draw into a buffer and read it back.
#[cfg(test)]
pub mod testing {
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Frame;

    /// The buffer a draw of `w`×`h` produces.
    pub fn draw(w: u16, h: u16, f: impl FnOnce(&mut Frame)) -> Buffer {
        draw_with_cursor(w, h, f).0
    }

    /// `draw`, and where the frame put the terminal's cursor (None: hidden).
    pub fn draw_with_cursor(w: u16, h: u16, f: impl FnOnce(&mut Frame)) -> (Buffer, Option<(u16, u16)>) {
        let mut t = ratatui::Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        t.draw(f).expect("draw");
        super::discard_images();
        let b = t.backend();
        let cursor = b.cursor_visible().then(|| (b.cursor_position().x, b.cursor_position().y));
        (b.buffer().clone(), cursor)
    }

    /// Each row's text, trailing blanks dropped.
    pub fn rows(buf: &Buffer) -> Vec<String> {
        let a = buf.area;
        (a.top()..a.bottom())
            .map(|y| {
                let mut s = String::new();
                let mut x = a.left();
                while x < a.right() {
                    let sym = buf[(x, y)].symbol();
                    s.push_str(sym);
                    x += super::width(sym).max(1) as u16;
                }
                s.trim_end().to_string()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_leave_the_cursor_where_ratatui_put_it() {
        // ratatui has placed the text field's cursor when the images go out:
        // an image moved it onto itself, and typing went on with the cursor
        // shown over the spectrogram.
        let at = |x, y| Placement { slot: ImageSlot::Viz, area: Rect::new(x, y, 10, 4), payload: Some("IMG".into()) };
        let out = images_escape(vec![at(2, 3), at(20, 1)]);
        assert!(out.starts_with("\x1B7") && out.ends_with("\x1B8"), "saved and restored: {out:?}");
        assert!(out.contains("\x1B[4;3HIMG") && out.contains("\x1B[2;21HIMG"));
        assert_eq!(images_escape(vec![Placement { slot: ImageSlot::Cover, area: Rect::new(0, 0, 1, 1), payload: None }]), "", "nothing to send, nothing moved");
    }

    #[test]
    fn a_pty_that_reports_no_size_is_not_too_small() {
        // The too-small check reads the same size the frame is drawn at.
        for raw in [Ok((0, 0)), Ok((0, 40)), Err(io::Error::other("no tty"))] {
            let (w, h) = window_from(raw);
            assert!(!crate::ui::window_too_small(w as usize, h as usize));
        }
        assert_eq!(window_from(Ok((30, 8))), (30, 8), "a real small window stays small");
    }

    /// Text whose width hand-written rules got wrong, one way or another:
    /// emoji sequences, flags, variation selectors, joiners between letters
    /// and after CJK, Indic conjuncts, combining marks, halfwidth kana.
    const TRICKY: &[&str] = &[
        "plain", "日本語", "e\u{301}", "\u{2764}\u{FE0F}", "\u{231A}\u{FE0E}", "1\u{FE0F}\u{20E3}",
        "\u{1F1F5}\u{1F1F9}", "\u{1F1F5}\u{1F1F9}\u{1F1E7}", "\u{1F44D}\u{1F3FD}",
        "\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F468}", "\u{2764}\u{FE0F}\u{200D}\u{1F525}",
        "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}", "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        "\u{4E2D}\u{200D}x", "\u{4E2D}\u{200D}\u{4E2D}", "\u{1F44D}\u{200D}a", "a\u{200D}b",
        "\u{2764}\u{200D}x", "\u{0DC1}\u{0DCA}\u{200D}\u{0DBB}\u{0DD3}", "\u{0D28}\u{0D4D}\u{200D}\u{0D31}",
        "\u{0915}\u{094D}\u{0937}", "\u{FF76}\u{FF9E}", "\u{1F600}\u{FE0E}", "x\u{200D}",
        // The visualisations' characters (measured by a short cut), and
        // one with a mark on it (which must not take the short cut).
        "\u{28FF}\u{2801}\u{2502}\u{2588}\u{2580}", "\u{2588}\u{301}", "~ ", "\u{25FE} Intro \u{25FD}",
        "Björk Café Łódź", "n\u{303}\u{F1}\u{301}x", "soft\u{AD}hyphen", "\u{1F468}\u{200D}\u{A9} \u{25B6}",
        "\u{600}é \u{605}a",
    ];

    /// Columns ratatui actually draws `s` across: the answer every measure
    /// must give.
    fn drawn(s: &str) -> usize {
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 200, 1));
        buf.set_stringn(0, 0, s, 200, ratatui::style::Style::new()).0 as usize
    }

    /// Time the per-frame text work the review rounds argued about, so the
    /// next one is settled by numbers: `cargo test --release
    /// tui::tests::cost -- --ignored --nocapture`. On 2026-10-15 (Apple
    /// silicon), per frame: a 200×60 braille screen 38 µs (0.11% of a core at
    /// 30 fps); measuring and cutting 60 rows 17 µs ASCII or accented, 98 µs
    /// CJK, 8 µs styled; 60 `LineBuf` rows 6 µs; a 12-hint key bar 2.5 µs;
    /// Classic's half-block cover, rebuilt from its pixels, 4.6 µs (0.01%:
    /// not worth caching its lines).
    #[test]
    #[ignore]
    fn cost() {
        let braille: String = (0..200).map(|i| char::from_u32(0x2800 + (i * 7 % 256)).unwrap()).collect();
        let ascii = "Artist Name - Some Song Title (Remastered 2011)";
        let accented = "Björk - Jóga (Live at Café Ñandú, Zürich, Łódź)";
        let cjk = "坂本龍一 - 戦場のメリークリスマス (Live)";
        let styled = "\x1B[1;32mArtist Name\x1B[0m · \x1B[2mSome Song Title\x1B[0m";
        let hints: Vec<(&str, String)> = (0..12).map(|i| ("k", format!("label {i}"))).collect();
        let look = crate::hints::Look { key: Style::new().bold(), label: Style::new(), sep: Line::raw(" · ") };
        let frames = 300u32;
        let report = |what: &str, start: std::time::Instant| {
            let per = start.elapsed() / frames;
            println!("{what:<44} {per:>10.1?} per frame, {:.2}% of a core at 30 fps", per.as_secs_f64() * 30.0 * 100.0);
        };
        let t = std::time::Instant::now();
        for _ in 0..frames {
            for _ in 0..60 {
                std::hint::black_box(width(std::hint::black_box(&braille)));
            }
        }
        report("measure a 200x60 braille screen", t);
        for (name, row) in [("ascii", ascii), ("accented", accented), ("cjk", cjk), ("styled", styled)] {
            let t = std::time::Instant::now();
            for _ in 0..frames {
                for _ in 0..60 {
                    std::hint::black_box(crate::ansi::truncate_plain(std::hint::black_box(row), 30));
                    std::hint::black_box(crate::ansi::visible_len(std::hint::black_box(row)));
                }
            }
            report(&format!("measure + cut 60 {name} rows"), t);
        }
        let t = std::time::Instant::now();
        for _ in 0..frames {
            for i in 0..60 {
                let mut l = LineBuf::new();
                l.push_str("  ").styled(&format!("{i:02}"), Style::new().dim()).push_str("  ").styled(accented, Style::new());
                std::hint::black_box(l.into_line());
            }
        }
        report("build 60 playlist rows (LineBuf)", t);
        let t = std::time::Instant::now();
        for _ in 0..frames {
            std::hint::black_box(crate::hints::bar_lines(std::hint::black_box(&hints), &look, 60));
        }
        report("a 12-hint key bar, wrapped", t);
        // Classic's 20×10 half-block cover (20×20 pixels), as drawn a frame.
        let size = crate::cover::CoverSize { cols: 20, rows: 10 };
        let pixels: Vec<u8> = (0..20 * 20 * 3).map(|i| (i * 37 % 256) as u8).collect();
        let img = crate::cover::CoverImage::HalfBlock { width: 20, height: 20, pixels, size };
        let t = std::time::Instant::now();
        for _ in 0..frames {
            std::hint::black_box(crate::cover::draw(Some(std::hint::black_box(&img)), size, false));
        }
        report("a half-block cover", t);
    }

    #[test]
    fn the_mouse_reports_clicks_and_drags_not_every_movement() {
        // Any-motion tracking (?1003, crossterm's capture) woke the loop for
        // every movement over the window.
        let on = mouse_on_sequence();
        assert!(on.contains("\x1B[?1000h") && on.contains("\x1B[?1002h") && on.contains("\x1B[?1006h"), "{on:?}");
        assert!(!on.contains("1003"), "{on:?}");
    }

    #[test]
    fn only_images_a_dialog_covers_are_withheld() {
        discard_images();
        let cover = Rect::new(2, 1, 20, 10);
        let viz = Rect::new(2, 14, 120, 20);
        place_image(ImageSlot::Cover, cover, Some("C".into()));
        place_image(ImageSlot::Viz, viz, Some("V".into()));
        // A 90-wide panel centred in a 200-wide window: over the viz, clear
        // of the cover at the top left.
        let panel = Rect::new(55, 1, 90, 38);
        assert_eq!(withhold_images_under(&[panel]), [ImageSlot::Viz]);
        IMAGES.with(|i| {
            let i = i.borrow();
            assert_eq!(i.now.iter().map(|p| p.slot).collect::<Vec<_>>(), [ImageSlot::Cover], "the cover stays");
        });
        // A box over both: both go.
        place_image(ImageSlot::Viz, viz, Some("V".into()));
        assert_eq!(withhold_images_under(&[Rect::new(0, 0, 200, 50)]).len(), 2);
        // No dialog: nothing withheld.
        place_image(ImageSlot::Cover, cover, None);
        assert!(withhold_images_under(&[]).is_empty());
        discard_images();
    }

    #[test]
    fn every_single_cell_character_is_one_cell_as_drawn() {
        // The short cut counts these instead of measuring them: each must
        // be drawn one cell wide (◽ ◾ sit among the geometric shapes and are
        // two).
        let mut wrong = Vec::new();
        for c in (0u32..0x3000).filter_map(char::from_u32).filter(|&c| crate::ansi::single_cell(c)) {
            if drawn(&c.to_string()) != 1 {
                wrong.push(c);
            }
        }
        assert!(wrong.is_empty(), "not one cell: {wrong:?}");
        // Nor part of a longer grapheme: after a joiner an emoji's glyph
        // (an Extended_Pictographic char: © ® ▶ ◻ …) joins the one before,
        // and the fast path would draw it in a cell of its own.
        use unicode_segmentation::UnicodeSegmentation;
        let mut joins = Vec::new();
        for c in (0u32..0x3000).filter_map(char::from_u32).filter(|&c| crate::ansi::single_cell(c)) {
            for before in ["a\u{200D}", "\u{1F468}\u{200D}"] {
                if format!("{before}{c}").graphemes(true).count() != 2 {
                    joins.push(c);
                }
            }
        }
        joins.dedup();
        assert!(joins.is_empty(), "join the grapheme before them: {joins:?}");
    }

    #[test]
    fn text_is_measured_and_cut_as_ratatui_draws_it() {
        for &t in TRICKY {
            let s = format!("{t}{t} {t}");
            assert_eq!(width(&s), drawn(&s), "tui::width {s:?}");
            assert_eq!(crate::ansi::visible_len(&s), drawn(&s), "visible_len {s:?}");
            assert_eq!(Line::raw(s.clone()).cols(), drawn(&s), "Line::cols {s:?}");
            for w in 1..10 {
                let rows = crate::ansi::split_columns(&s, w);
                assert_eq!(rows.concat(), s, "nothing lost");
                for r in &rows {
                    // A row wider than the width is one grapheme wider than
                    // it, alone (👨‍© is drawn 3 wide).
                    let alone = unicode_segmentation::UnicodeSegmentation::graphemes(*r, true).count() == 1;
                    assert!(drawn(r) <= w || alone, "{r:?} in {w}");
                    assert_eq!(drawn(r), crate::ansi::visible_len(r), "{r:?}");
                }
                for r in wrap(&s, w) {
                    let alone = unicode_segmentation::UnicodeSegmentation::graphemes(r.as_str(), true).count() == 1;
                    assert!(drawn(&r) <= w || alone, "wrap {r:?} is {} in {w}", drawn(&r));
                }
                // A cut keeps as many whole characters as fit, and no more.
                let cut = crate::ansi::truncate_ansi(&s, w);
                assert!(s.starts_with(&cut) && drawn(&cut) <= w, "cut {cut:?} in {w}");
                let next = &s[cut.len()..];
                if let Some(g) = unicode_segmentation::UnicodeSegmentation::graphemes(next, true).next() {
                    assert!(drawn(&format!("{cut}{g}")) > w || g.contains(char::is_control), "cut {cut:?} in {w} left out {g:?}");
                }
            }
        }
    }

    #[test]
    fn a_line_bufs_running_width_is_the_finished_lines() {
        let mut l = LineBuf::new();
        let check = |l: &LineBuf| {
            let done = Line::from(l.spans.clone()).cols() + width(&l.text);
            assert_eq!(l.width(), done);
        };
        l.push_str("ab").styled("日本", Style::new().bold());
        check(&l);
        l.push('e').push('\u{301}'); // a mark pushed after its base
        check(&l);
        l.append(Line::from(vec![Span::raw("\u{1F1F5}\u{1F1F9}"), Span::styled("ｶﾞ", Style::new().italic())]));
        check(&l);
        l.reset().spaces(3).styled("x", Style::new().dim());
        check(&l);
        let w = l.width();
        assert_eq!(l.into_line().cols(), w);
    }

    #[test]
    fn wrapping_breaks_between_words_and_measures_columns() {
        assert_eq!(wrap("the quick brown fox", 10), ["the quick", "brown fox"]);
        assert_eq!(wrap("short", 10), ["short"]);
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(wrap("abcdefghij klm", 4), ["abcd", "efgh", "ij", "klm"]);
        // Two columns a character: four fit in eight columns.
        assert_eq!(wrap("日本語の歌詞です", 8), ["日本語の", "歌詞です"]);
        for line in wrap("a fairly long line of lyrics that keeps going on", 7) {
            assert!(width(&line) <= 7, "{line:?}");
        }
        // A line that fits keeps its inner spacing; the ends are dropped, so
        // every row starts at its text (and a centred one is centred on it).
        assert_eq!(wrap("  oh   oh  ", 20), ["oh   oh"]);
        assert_eq!(wrap("  (backing vocals here)", 12), ["(backing", "vocals here)"]);
        // Wrapped, a row keeps its inner spacing too; the break takes the
        // spaces it falls on.
        assert_eq!(wrap("so   loud   oh", 9), ["so   loud", "oh"]);
        // An emoji keeps its variation selector: the pair moves on whole.
        assert_eq!(wrap("abc❤\u{FE0F}", 4), ["abc", "❤\u{FE0F}"]);
        assert_eq!(wrap("❤\u{FE0F}", 1), ["❤\u{FE0F}"]);
        // A joined emoji holding a VS16 (👩‍❤️‍👨) is one 2-column glyph: it
        // fits where two columns are left, and ratatui, which draws it,
        // agrees on the width.
        let couple = "\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F468}";
        assert_eq!(width(couple), 2);
        assert_eq!(wrap(&format!("ab {couple}"), 5), [format!("ab {couple}")]);
        assert_eq!(wrap(&format!("aaaaaaaa{couple}"), 9), ["aaaaaaaa", couple]);
        // Joiners between letters (Sinhala ශ්‍රී, a‍b): every row fits as
        // ratatui measures it, at every width.
        let sinhala = "\u{0DC1}\u{0DCA}\u{200D}\u{0DBB}\u{0DD3}";
        let text = format!("{sinhala}{sinhala}{sinhala} a\u{200D}b\u{200D}c\u{200D}d\u{200D}e\u{200D}f {couple}{couple} x");
        for w in 2..30 {
            for row in wrap(&text, w) {
                assert!(width(&row) <= w, "{row:?} is {} columns in {w}", width(&row));
            }
        }
        // Flags are never split in two: 🇵🇹 is one 2-column glyph.
        let flags = "🇵🇹🇧🇷🇯🇵🇫🇷";
        for line in wrap(flags, 3) {
            assert_eq!(line.chars().count() % 2, 0, "half a flag in {line:?}");
        }
        assert_eq!(wrap(flags, 4), ["🇵🇹🇧🇷", "🇯🇵🇫🇷"]);
        assert_eq!(wrap("  abcdefghij more", 10), ["abcdefghij", "more"]);
        // Even in a 1-column panel a flag stays whole.
        assert_eq!(wrap("🇵🇹", 1), ["🇵🇹"]);
        assert_eq!(wrap("a 🇵🇹 b", 1), ["a", "🇵🇹", "b"]);
        // A combining mark stays with its letter.
        assert_eq!(wrap("e\u{301}e\u{301}e\u{301}", 2), ["e\u{301}e\u{301}", "e\u{301}"]);
        // Whatever the text, a cut word loses nothing and never panics.
        let odd = "\x1B[31mabcdefghij\x1B[0m";
        assert_eq!(wrap(odd, 4).concat(), odd);
    }

    #[test]
    fn a_line_buffer_merges_runs_of_one_style() {
        let red = Style::new().fg(Color::Red);
        let mut b = LineBuf::new();
        b.style(red).push('a').push('b').style(red).push('c');
        b.reset().push_str("  ").fg((1, 2, 3)).push('x');
        let line = b.into_line();
        assert_eq!(line.spans.len(), 3, "{line:?}");
        assert_eq!(plain(&line), "abc  x");
        assert_eq!(line.spans[0].style, red);
        assert_eq!(line.spans[2].style, Style::new().fg(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn truncation_keeps_whole_wide_characters_and_styles() {
        let red = Style::new().fg(Color::Red);
        let l = Line::from(vec![Span::styled("ab", red), Span::raw("日本語")]);
        assert_eq!(crate::tui::Cols::cols(&l), 8);
        let cut = truncate(l.clone(), 5);
        assert_eq!(plain(&cut), "ab日");
        assert_eq!(cut.spans[0].style, red);
        let cut = truncate_ellipsis(l, 6);
        assert_eq!(plain(&cut), "ab日…");
        assert!(crate::tui::Cols::cols(&cut) <= 6);
    }

    #[test]
    fn fitting_drops_whole_items_from_the_end() {
        let items: Vec<Line<'static>> = ["one", "two", "three"].map(Line::raw).to_vec();
        let sep = Line::raw(" · ");
        assert_eq!(plain(&fit_segments(&items, &sep, 40)), "one · two · three");
        assert_eq!(plain(&fit_segments(&items, &sep, 12)), "one · two");
        assert_eq!(plain(&fit_segments(&items, &sep, 2)), "o…");
    }

    #[test]
    fn padding_makes_a_line_exactly_its_width() {
        for w in [0, 3, 10] {
            assert_eq!(crate::tui::Cols::cols(&pad(Line::raw("hello"), w)), w);
        }
    }

    #[test]
    fn the_cursor_row_spans_its_width_in_the_highlight() {
        let p = crate::theme::palette(crate::theme::ThemeKind::Minimal);
        let row = Line::from(vec![Span::styled("07", p.dim), Span::raw("  Title")]);
        let c = cursor_row(p, &row, 20);
        assert_eq!(crate::tui::Cols::cols(&c), 20);
        assert_eq!(c.spans.len(), 1);
        assert_eq!(c.spans[0].style, p.cursor_hl);
        assert_eq!(crate::tui::Cols::cols(&cursor_row(p, &Line::raw("x".repeat(50)), 20)), 20);
    }

    #[test]
    fn the_fast_path_draws_what_ratatui_draws() {
        let red = Style::new().fg(Color::Red);
        let lines = [
            Line::from(vec![Span::styled("⣿⣀ ab─│", red), Span::raw("日本e\u{301}x"), Span::raw("🇵🇹 end")]),
            Line::from(vec![Span::raw("░▒▓█"), Span::styled("…é", red), Span::raw(" ⠂ ◾ ◽x◼")]),
            Line::raw("a long line that runs past the edge of the area"),
            Line::raw("#\u{FE0F}\u{20E3} a\u{200D}b 👨\u{200D}👩 x\u{0}y ⣿\u{301} 👨\u{200D}© ▶\u{FE0F}"),
            // A prepended sign joins the letter after it (one grapheme).
            Line::raw("\u{600}é \u{605}a\u{600}\u{600}b x"),
            // A wide character that does not fit ends its SPAN there: the
            // rest of the span is not drawn after the gap; the next span is.
            Line::raw("日本語abc"),
            Line::from(vec![Span::raw("ab"), Span::raw("日x")]),
            Line::from(vec![Span::raw("ab日"), Span::raw("x")]),
        ];
        for w in [0u16, 1, 2, 3, 4, 5, 9, 14, 40] {
            let area = Rect::new(0, 0, 40, 8);
            let mut ours = Buffer::empty(area);
            let mut theirs = Buffer::empty(area);
            draw_lines(&mut ours, Rect::new(1, 0, w, 8), &lines);
            for (i, l) in lines.iter().enumerate() {
                theirs.set_line(1, i as u16, l, w);
            }
            assert_eq!(ours, theirs, "width {w}");
        }
    }

    #[test]
    fn the_scrollbar_thumb_is_the_visible_share_where_it_is() {
        let thumb = |total: usize, first: usize| {
            let buf = testing::draw(4, 10, |f| scrollbar(f, Rect::new(0, 0, 4, 10), total, first, Style::new(), Style::new()));
            let col: Vec<String> = (0..10).map(|y| buf[(3, y)].symbol().to_string()).collect();
            let rows: Vec<usize> = (0..10).filter(|&y| col[y] == "┃").collect();
            (rows, col)
        };
        // 100 items, 10 shown: a one-row thumb at the top, then the bottom.
        assert_eq!(thumb(100, 0).0, vec![0]);
        assert_eq!(thumb(100, 90).0, vec![9]);
        // 20 items, 10 shown, halfway: half the track, in its lower half.
        assert_eq!(thumb(20, 10).0, (5..10).collect::<Vec<_>>());
        // Everything fits: no bar at all.
        assert!(thumb(10, 0).1.iter().all(|s| *s == " "));
    }

    #[test]
    fn only_sixel_in_windows_terminal_needs_a_fresh_screen() {
        use crate::cover::GraphicsProtocol as GP;
        assert!(sixel_survives_clear_with(GP::Sixel, true));
        assert!(!sixel_survives_clear_with(GP::Sixel, false), "elsewhere a clear removes Sixel");
        assert!(!sixel_survives_clear_with(GP::Kitty, true));
        assert!(!sixel_survives_clear_with(GP::HalfBlock, true));
    }

    #[test]
    fn a_clear_never_asks_the_terminal_anything() {
        let mut b = KeetBackend { inner: CrosstermBackend::new(crate::term::Writer) };
        assert_eq!(b.get_cursor_position().unwrap(), Position::ORIGIN);
    }
}
