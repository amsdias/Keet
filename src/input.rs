//! Terminal input for every loop that reads it (the player's, the welcome
//! screen's): crossterm's events, with a paste made whole on Windows.
//!
//! Elsewhere bracketed paste makes a paste — or a folder dropped on the
//! terminal — one `Event::Paste`. crossterm reads console input records on
//! Windows and never makes one: the terminal's markers (`ESC[200~` …
//! `ESC[201~`) and the text between them arrive as keystrokes, so a dropped
//! `D:\Queen` was typed as commands (its Shift+Q quit, a `p` opened the
//! folder picker). Here the markers and what they enclose become the paste
//! again. A console that sends no markers (the old console host types a
//! drop) is caught by `burst`: a run of characters already waiting, a path
//! separator among them, is more than anyone types between two reads.

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::time::Duration;

thread_local! {
    /// Events read ahead to see whether they were a paste, and were not.
    static AHEAD: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
}

/// Whether an event is waiting, within `timeout` (`crossterm::event::poll`).
pub fn poll(timeout: Duration) -> bool {
    AHEAD.with(|a| !a.borrow().is_empty()) || event::poll(timeout).unwrap_or(false)
}

/// The next event (`crossterm::event::read`), a paste whole on Windows.
pub fn read() -> std::io::Result<Event> {
    if let Some(e) = AHEAD.with(|a| a.borrow_mut().pop_front()) {
        return Ok(e);
    }
    let first = event::read()?;
    if !cfg!(windows) {
        return Ok(first);
    }
    let mut next = |wait| event::poll(wait).unwrap_or(false).then(event::read).and_then(Result::ok);
    let (ev, rest) = assemble(first, &mut next);
    AHEAD.with(|a| a.borrow_mut().extend(rest));
    Ok(ev)
}

/// `first`, or the paste it starts, and the events read to find out that
/// were not part of it (in order). `next(wait)` is the next event within
/// `wait`.
fn assemble(first: Event, next: &mut impl FnMut(Duration) -> Option<Event>) -> (Event, Vec<Event>) {
    match text_of(&first) {
        Typed::Char('\x1b') => marked(first, next),
        Typed::Char(c) if !c.is_control() => burst(first, c, next),
        _ => (first, Vec::new()),
    }
}

/// How long each key of a start marker may take behind the one before.
/// The terminal writes the marker at once; an Esc tap waits this long.
const MARKER_WAIT: Duration = Duration::from_millis(10);

/// How long the text of a marked paste may pause before it is taken as
/// ended: an end marker lost on the way must not hold the keys forever.
const PASTE_WAIT: Duration = Duration::from_millis(100);

const START: &str = "\x1b[200~";
const END: &str = "\x1b[201~";

/// Characters in a run, at least, for `burst` to take it as a paste.
const BURST_MIN: usize = 3;

/// What a key event types (the characters a paste is made of).
enum Typed {
    Char(char),
    /// A key's release: Windows reports one for every key.
    Release,
    Other,
}

fn text_of(ev: &Event) -> Typed {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Release => Typed::Release,
        Event::Key(k) => match k.code {
            KeyCode::Char(c) => Typed::Char(c),
            KeyCode::Esc => Typed::Char('\x1b'),
            KeyCode::Enter => Typed::Char('\n'),
            KeyCode::Tab => Typed::Char('\t'),
            _ => Typed::Other,
        },
        _ => Typed::Other,
    }
}

/// An Esc: the start of a marked paste, or an Esc.
fn marked(first: Event, next: &mut impl FnMut(Duration) -> Option<Event>) -> (Event, Vec<Event>) {
    let mut seen = Vec::new();
    for want in START.chars().skip(1) {
        loop {
            let Some(ev) = next(MARKER_WAIT) else { return (first, seen) };
            let t = text_of(&ev);
            seen.push(ev);
            match t {
                Typed::Release => continue,
                Typed::Char(c) if c == want => break,
                _ => return (first, seen),
            }
        }
    }
    // Everything up to the end marker is the text; anything else that came
    // meanwhile (a resize) is kept for after it.
    let mut text = String::new();
    let mut after = Vec::new();
    while !text.ends_with(END) {
        let Some(ev) = next(PASTE_WAIT) else { break };
        match text_of(&ev) {
            Typed::Char(c) => text.push(c),
            Typed::Release => {}
            Typed::Other => after.push(ev),
        }
    }
    let text = text.strip_suffix(END).map(str::to_string).unwrap_or(text);
    (Event::Paste(text), after)
}

/// A character key: the first of a pasted run typed out by the console, or
/// a key. Only what is ALREADY waiting counts, a person types nothing in
/// between two reads; and only a run with a path separator in it — keys
/// typed during a slow frame can wait two or three at a time, and a drop
/// is a path.
fn burst(first: Event, c: char, next: &mut impl FnMut(Duration) -> Option<Event>) -> (Event, Vec<Event>) {
    let mut text = String::from(c);
    let mut seen = Vec::new();
    // The event that ended the run, kept for after a paste.
    let mut tail = None;
    while let Some(ev) = next(Duration::ZERO) {
        match text_of(&ev) {
            Typed::Release => seen.push(ev),
            Typed::Char(c) if !c.is_control() => {
                text.push(c);
                seen.push(ev);
            }
            _ => {
                tail = Some(ev);
                break;
            }
        }
    }
    if text.chars().count() >= BURST_MIN && text.contains(['\\', '/', ':']) {
        return (Event::Paste(text), tail.into_iter().collect());
    }
    seen.extend(tail);
    (first, seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn release(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Release))
    }

    /// Keys as Windows reports them: a press and a release per character,
    /// capitals with Shift.
    fn typed(text: &str) -> Vec<Event> {
        let mut out = Vec::new();
        for c in text.chars() {
            let code = match c {
                '\x1b' => KeyCode::Esc,
                '\n' => KeyCode::Enter,
                c => KeyCode::Char(c),
            };
            let m = if c.is_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
            out.push(Event::Key(KeyEvent::new(code, m)));
            out.push(Event::Key(KeyEvent::new_with_kind(code, m, KeyEventKind::Release)));
        }
        out
    }

    /// `assemble` over `events`, all already waiting; then what `read`
    /// would hand out next, in order.
    fn run(events: Vec<Event>) -> Vec<Event> {
        let mut queue: VecDeque<Event> = events.into();
        let mut ahead: VecDeque<Event> = VecDeque::new();
        let mut out = Vec::new();
        loop {
            // What `read` hands out from AHEAD is never assembled again.
            if let Some(e) = ahead.pop_front() {
                out.push(e);
                continue;
            }
            let Some(first) = queue.pop_front() else { break };
            let (ev, rest) = assemble(first, &mut |_| queue.pop_front());
            out.push(ev);
            ahead.extend(rest);
        }
        out
    }

    fn pastes(events: &[Event]) -> Vec<&str> {
        events.iter().filter_map(|e| if let Event::Paste(t) = e { Some(t.as_str()) } else { None }).collect()
    }

    fn keys(events: &[Event]) -> String {
        events.iter().filter_map(|e| match e {
            Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Char(c) => Some(c),
                KeyCode::Esc => Some('⎋'),
                _ => Some('?'),
            },
            _ => None,
        }).collect()
    }

    #[test]
    fn a_bracketed_paste_typed_as_keys_is_one_paste() {
        // Windows Terminal with ?2004h, read through console records.
        let out = run(typed("\x1b[200~D:\\Queen\x1b[201~"));
        assert_eq!(pastes(&out), ["D:\\Queen"]);
        assert_eq!(keys(&out), "", "no key of it acts: {out:?}");
    }

    #[test]
    fn what_follows_a_paste_still_comes() {
        let mut events = typed("\x1b[200~/x\x1b[201~");
        events.push(Event::Resize(80, 24));
        events.extend(typed("q"));
        let out = run(events);
        assert_eq!(pastes(&out), ["/x"]);
        assert!(out.contains(&Event::Resize(80, 24)));
        assert_eq!(keys(&out), "q");
    }

    #[test]
    fn a_pasted_line_break_stays_in_the_text() {
        let out = run(typed("\x1b[200~a\nb\x1b[201~"));
        assert_eq!(pastes(&out), ["a\nb"]);
    }

    #[test]
    fn an_esc_tap_is_still_an_esc() {
        // Alone, with its release, and with a key typed after it.
        let out = run(typed("\x1b"));
        assert_eq!(keys(&out), "⎋");
        assert!(out.contains(&release(KeyCode::Esc)), "its release is kept");
        let out = run(typed("\x1bq"));
        assert_eq!(keys(&out), "⎋q");
        assert!(pastes(&out).is_empty());
        // Esc then `[` (a terminal's own sequence, Alt+[): passed on as is.
        let out = run(typed("\x1b[A"));
        assert_eq!(keys(&out), "⎋[A");
    }

    #[test]
    fn a_paste_that_never_ends_ends_with_what_came() {
        let out = run(typed("\x1b[200~/Music/half"));
        assert_eq!(pastes(&out), ["/Music/half"]);
    }

    #[test]
    fn a_dropped_path_typed_without_markers_is_one_paste() {
        // The old console host types a drop: every key already waiting.
        let out = run(typed("D:\\Queen"));
        assert_eq!(pastes(&out), ["D:\\Queen"]);
        assert_eq!(keys(&out), "");
        let out = run(typed("/Users/me/Music"));
        assert_eq!(pastes(&out), ["/Users/me/Music"]);
    }

    #[test]
    fn keys_waiting_without_a_path_in_them_stay_keys() {
        // Typed while a frame was drawn: two or three keys can be waiting.
        for text in ["ll", "qq", "abc", "zzzz"] {
            let out = run(typed(text));
            assert!(pastes(&out).is_empty(), "{text}: {out:?}");
            assert_eq!(keys(&out), text);
        }
        // And a key on its own, with a separator: `/` opens a search.
        let out = run(typed("/"));
        assert!(pastes(&out).is_empty());
        assert_eq!(keys(&out), "/");
    }

    #[test]
    fn a_dropped_path_ends_at_the_first_key_that_is_not_text() {
        let mut events = typed("C:\\a");
        events.push(press(KeyCode::Enter));
        let out = run(events);
        assert_eq!(pastes(&out), ["C:\\a"]);
        assert!(out.contains(&press(KeyCode::Enter)), "kept, after the paste: {out:?}");
    }
}
