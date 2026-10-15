//! Key hints: the key bars every view draws (`␣ pause  ? keys  ←→ seek …`),
//! built so the mouse can press them. A bar is a styled line plus where each
//! key landed on it; the renderer that draws the bar registers those spots
//! (`Hit::Key`), and a click on one is handled exactly as that key typed
//! (`ui::mouse` queues it for `poll_input`).
//!
//! A hint's key text is read the way it reads on screen: `←→` is two keys,
//! each glyph its own target; `w/s` and `w s` are `w` and `s`; `⇧R` is
//! Shift+R; `Tab`, `Esc` and `Enter` are those keys; `␣` is Space. A label is
//! clickable too when its hint names a single key.

use crate::tui::Cols;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use crate::state::Hit;
use crate::tui::{self, LineBuf};

/// A key as a click sends it.
pub type Key = (KeyCode, KeyModifiers);

/// A drawn bar: the line, and each target's columns on it (`start`,
/// `width`) counted from the line's first column — a key, or what a hint
/// names instead (`bar_lines`).
pub struct Bar {
    pub line: Line<'static>,
    pub keys: Vec<(usize, usize, Hit)>,
    /// The columns it was laid out for (what is drawn of it): no target is
    /// registered past them.
    pub width: usize,
}

impl Bar {
    /// Register the keys for the mouse: the bar is drawn at column `x`,
    /// row `y` (of the window). Each cut to the bar's width: one wider than
    /// what was drawn reached into the neighbouring panel, where a click
    /// pressed it.
    pub fn register(&self, hits: &mut Vec<(Rect, Hit)>, x: u16, y: u16) {
        for &(start, width, hit) in &self.keys {
            let width = width.min(self.width.saturating_sub(start));
            if width > 0 {
                hits.push((Rect::new(x + start as u16, y, width as u16, 1), hit));
            }
        }
    }
}

/// How a theme draws its hints: the key, the label, and what goes between
/// two hints.
pub struct Look {
    pub key: Style,
    pub label: Style,
    pub sep: Line<'static>,
}

/// The keys in a hint's key text, each with its column offset and width
/// within it.
/// The key a glyph in key text stands for (None: a separator).
fn key_of_glyph(c: char) -> Option<KeyCode> {
    match c {
        '␣' => Some(KeyCode::Char(' ')),
        '←' => Some(KeyCode::Left),
        '→' => Some(KeyCode::Right),
        '↑' => Some(KeyCode::Up),
        '↓' => Some(KeyCode::Down),
        '↵' => Some(KeyCode::Enter),
        '−' => Some(KeyCode::Char('-')),
        // Separators between two keys ("w/s", "w s").
        ' ' | '/' => None,
        c => Some(KeyCode::Char(c)),
    }
}

pub fn keys_of(text: &str) -> Vec<(usize, usize, Key)> {
    let none = KeyModifiers::NONE;
    match text {
        "Tab" => return vec![(0, 3, (KeyCode::Tab, none))],
        "Esc" => return vec![(0, 3, (KeyCode::Esc, none))],
        "Enter" => return vec![(0, 5, (KeyCode::Enter, none))],
        "Space" => return vec![(0, 5, (KeyCode::Char(' '), none))],
        "/" => return vec![(0, 1, (KeyCode::Char('/'), none))],
        _ => {}
    }
    // Words ("any other key") are not keys: key texts never have two
    // letters in a row (`w s`, `⇧R`, `a/d`).
    if text.chars().zip(text.chars().skip(1)).any(|(a, b)| a.is_alphabetic() && b.is_alphabetic()) {
        return Vec::new();
    }
    // By grapheme, measured as drawn: a key with a variation selector or a
    // mark (↵ + VS16) is one target, as wide as it is drawn.
    use unicode_segmentation::UnicodeSegmentation;
    let units: Vec<(char, usize)> = text
        .graphemes(true)
        .map(|g| (g.chars().next().unwrap_or(' '), crate::ansi::grapheme_width(g)))
        .collect();
    let mut out = Vec::new();
    let mut col = 0;
    let mut i = 0;
    while i < units.len() {
        let (c, w) = units[i];
        if c == '⇧' {
            if let Some(&(l, lw)) = units.get(i + 1) {
                // The glyph's own key, shifted: `⇧↑` is Up, not a '↑' char
                // (a click on it pressed nothing).
                let code = match key_of_glyph(l) {
                    Some(KeyCode::Char(ch)) => KeyCode::Char(ch.to_ascii_uppercase()),
                    Some(code) => code,
                    None => KeyCode::Char(l),
                };
                out.push((col, w + lw, (code, KeyModifiers::SHIFT)));
                col += w + lw;
                i += 2;
                continue;
            }
        }
        if let Some(code) = key_of_glyph(c) {
            out.push((col, w, (code, none)));
        }
        col += w;
        i += 1;
    }
    out
}

/// As many whole hints of `items` (key text, label) as fit in `width`
/// columns, joined by the look's separator; with every key's spot.
pub fn bar<H: Hint>(items: &[H], look: &Look, width: usize) -> Bar {
    bar_taking(items, look, width).0
}

/// A hint as a bar takes it: `(key text, label)`, its keys its targets, or
/// `(key text, label, target)`, which may name what a click on it does
/// instead (`Some`: a docked panel's `focus` hint is the panel). Key and
/// label are any string type (`&str`, a label built at run time as a
/// `String`), taken as they are: a bar is drawn every frame, and copying
/// owned labels into borrowed ones allocated a list per frame.
pub trait Hint {
    fn parts(&self) -> (&str, &str, Option<Hit>);
}

impl<K: AsRef<str>, L: AsRef<str>> Hint for (K, L) {
    fn parts(&self) -> (&str, &str, Option<Hit>) {
        (self.0.as_ref(), self.1.as_ref(), None)
    }
}

impl<K: AsRef<str>, L: AsRef<str>> Hint for (K, L, Option<Hit>) {
    fn parts(&self) -> (&str, &str, Option<Hit>) {
        (self.0.as_ref(), self.1.as_ref(), self.2)
    }
}

/// `bar`, and how many of `items` it took.
fn bar_taking<H: Hint>(items: &[H], look: &Look, width: usize) -> (Bar, usize) {
    let mut line = LineBuf::new();
    let mut keys = Vec::new();
    let mut taken = 0;
    let sep_cols = look.sep.cols();
    for (i, (key, label, target)) in items.iter().map(Hint::parts).enumerate() {
        // Each measured once (the hint's spot, its label's, the fit).
        let (key_w, label_w) = (tui::width(key), tui::width(label));
        let item_w = key_w + if label.is_empty() { 0 } else { 1 + label_w };
        let sep_w = if i > 0 { sep_cols } else { 0 };
        if line.width() + sep_w + item_w > width {
            break;
        }
        taken += 1;
        if i > 0 {
            line.append(look.sep.clone());
        }
        let x = line.width();
        match target {
            // A hint that names its own target is that target, all of it,
            // whatever its key text reads as.
            Some(hit) => keys.push((x, item_w, hit)),
            None => {
                let ks = keys_of(key);
                for &(off, w, (code, mods)) in &ks {
                    keys.push((x + off, w, Hit::Key(code, mods)));
                }
                if let [(_, _, (code, mods))] = ks[..] {
                    if !label.is_empty() {
                        keys.push((x + key_w + 1, label_w, Hit::Key(code, mods)));
                    }
                }
            }
        }
        line.styled(key, look.key);
        if !label.is_empty() {
            line.reset().push(' ').styled(label, look.label);
        }
    }
    (Bar { line: line.into_line(), keys, width }, taken)
}

/// All of `items`, as many whole hints per line as fit in `width` columns:
/// the bar wrapped onto as many lines as it takes (a hint wider than the
/// whole width gets a line of its own and is cut by the window edge).
pub fn bar_lines<H: Hint>(items: &[H], look: &Look, width: usize) -> Vec<Bar> {
    let mut out = Vec::new();
    let mut rest = items;
    while !rest.is_empty() {
        let (b, taken) = bar_taking(rest, look, width);
        // A hint too wide for any line gets one of its own (cut by the
        // window edge), so this cannot loop forever.
        let (b, taken) = if taken == 0 {
            // Clickable only where it is drawn (`register` cuts to `width`).
            let (mut b, taken) = bar_taking(&rest[..1], look, usize::MAX);
            b.width = width;
            (b, taken)
        } else {
            (b, taken)
        };
        out.push(b);
        rest = &rest[taken..];
    }
    out
}

/// `bar` moved right by `n` columns of padding (the margin the line starts
/// with), keys and all.
pub fn indent(mut b: Bar, n: usize) -> Bar {
    let mut line = LineBuf::new();
    line.spaces(n).append(b.line);
    b.line = line.into_line();
    for k in &mut b.keys {
        k.0 += n;
    }
    b.width = b.width.saturating_add(n);
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(c: char) -> Key {
        (KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn hit((code, mods): Key) -> Hit {
        Hit::Key(code, mods)
    }

    #[test]
    fn key_texts_read_as_they_read_on_screen() {
        assert_eq!(keys_of("␣"), vec![(0, 1, k(' '))]);
        assert_eq!(
            keys_of("←→"),
            vec![(0, 1, (KeyCode::Left, KeyModifiers::NONE)), (1, 1, (KeyCode::Right, KeyModifiers::NONE))]
        );
        assert_eq!(keys_of("w/s"), vec![(0, 1, k('w')), (2, 1, k('s'))]);
        assert_eq!(keys_of("a d"), vec![(0, 1, k('a')), (2, 1, k('d'))]);
        assert_eq!(keys_of("+−"), vec![(0, 1, k('+')), (1, 1, k('-'))]);
        assert_eq!(keys_of("⇧R"), vec![(0, 2, (KeyCode::Char('R'), KeyModifiers::SHIFT))]);
        assert_eq!(keys_of("Tab"), vec![(0, 3, (KeyCode::Tab, KeyModifiers::NONE))]);
        assert_eq!(keys_of("/"), vec![(0, 1, k('/'))]);
        assert!(keys_of("any other key").is_empty());
    }

    #[test]
    fn a_wrapped_bar_keeps_every_hint_on_some_line() {
        let look = Look { key: Style::new(), label: Style::new(), sep: Line::raw("  ") };
        let items = [("␣", "pause"), ("←→", "seek"), ("q", "quit"), ("k", "a very long label indeed")];
        let lines = bar_lines(&items, &look, 16);
        let text: Vec<String> = lines.iter().map(|b| tui::plain(&b.line)).collect();
        assert_eq!(text, ["␣ pause  ←→ seek", "q quit", "k a very long label indeed"]);
        // Keys are counted from their own line's start.
        assert_eq!(lines[1].keys[0], (0, 1, hit(k('q'))));
        // Wide enough: one line.
        assert_eq!(bar_lines(&items, &look, 200).len(), 1);
    }

    #[test]
    fn a_bar_keeps_whole_hints_and_knows_where_each_key_is() {
        let look = Look { key: Style::new(), label: Style::new(), sep: Line::raw("  ") };
        let b = bar(&[("␣", "pause"), ("←→", "seek"), ("q", "quit")], &look, 20);
        assert_eq!(tui::plain(&b.line), "␣ pause  ←→ seek");
        // ␣ and its label; ← and → but not the label (two keys); q is cut.
        assert_eq!(
            b.keys,
            vec![
                (0, 1, hit(k(' '))),
                (2, 5, hit(k(' '))),
                (9, 1, Hit::Key(KeyCode::Left, KeyModifiers::NONE)),
                (10, 1, Hit::Key(KeyCode::Right, KeyModifiers::NONE)),
            ]
        );
        let shifted = indent(b, 2);
        assert_eq!(shifted.keys[0].0, 2);
        assert_eq!(tui::plain(&shifted.line), "  ␣ pause  ←→ seek");
    }

    #[test]
    fn a_key_with_a_selector_or_a_mark_is_one_target_as_wide_as_drawn() {
        // ↵ + VS16 is one grapheme: one Enter target, as wide as drawn (the
        // selector alone used to be a key of its own). Likewise ❤ + VS16,
        // drawn two columns.
        for key in ["\u{21B5}\u{FE0F}", "\u{2764}\u{FE0F}", "e\u{301}"] {
            let k = keys_of(key);
            assert_eq!(k.len(), 1, "{key:?}: {k:?}");
            assert_eq!((k[0].0, k[0].1), (0, crate::tui::width(key)), "{key:?}");
            // What follows it starts after it, as drawn.
            let w = crate::tui::width(key);
            let k = keys_of(&format!("{key}/z"));
            assert_eq!(k.iter().map(|&(x, w, _)| (x, w)).collect::<Vec<_>>(), [(0, w), (w + 1, 1)], "{key:?}");
        }
        assert_eq!(keys_of("\u{21B5}\u{FE0F}")[0].2, (KeyCode::Enter, KeyModifiers::NONE));
        // Unchanged: ⇧R is one target, w/s two.
        assert_eq!(keys_of("⇧R").len(), 1);
        assert_eq!(keys_of("w/s").iter().map(|&(x, w, _)| (x, w)).collect::<Vec<_>>(), [(0, 1), (2, 1)]);
    }

    #[test]
    fn a_bar_registers_no_target_past_its_width() {
        let look = Look { key: Style::new(), label: Style::new(), sep: Line::raw("  ") };
        // Laid out for 10 columns, a one-key hint whose label runs past them.
        let mut b = bar_lines(&[("↵", "follow the song again")], &look, 10).remove(0);
        // Even with a key nudged past the edge by hand: register clips.
        b.keys.push((12, 3, Hit::Key(KeyCode::Char('z'), KeyModifiers::NONE)));
        let b = indent(b, 2);
        let mut hits = Vec::new();
        b.register(&mut hits, 5, 0);
        assert!(!hits.is_empty());
        for (r, _) in &hits {
            assert!(r.right() <= 5 + 2 + 10, "{r:?} past the bar's 12 columns at x=5");
        }
    }

    #[test]
    fn a_hint_wider_than_the_bar_is_clickable_only_where_drawn() {
        let look = Look { key: Style::new(), label: Style::new(), sep: Line::raw("  ") };
        // A one-key hint (its label clickable too) wider than 8 columns: a
        // row of its own, cut at the edge, and so are its targets.
        let rows = bar_lines(&[("↵", "follow the song again"), ("0", "reset")], &look, 8);
        assert!(rows[0].keys.iter().any(|&(x, _, _)| x > 0), "the label is a target");
        for b in rows {
            let mut hits = Vec::new();
            b.register(&mut hits, 0, 0);
            for (r, _) in &hits {
                assert!(r.right() <= 8, "a click target past the bar's edge: {hits:?}");
            }
        }
    }

    #[test]
    fn shift_with_an_arrow_or_enter_is_that_key_shifted() {
        let shift = KeyModifiers::SHIFT;
        assert_eq!(keys_of("⇧↑")[0].2, (KeyCode::Up, shift));
        assert_eq!(keys_of("⇧←")[0].2, (KeyCode::Left, shift));
        assert_eq!(keys_of("⇧↵")[0].2, (KeyCode::Enter, shift));
        assert_eq!(keys_of("⇧R")[0].2, (KeyCode::Char('R'), shift), "letters as before");
        assert_eq!(keys_of("⇧k")[0].2, (KeyCode::Char('K'), shift));
    }

    #[test]
    fn a_hint_can_name_its_own_target() {
        let look = Look { key: Style::new(), label: Style::new(), sep: Line::raw("  ") };
        let panel = Hit::Panel(crate::state::ViewMode::Lyrics);
        // Two hints on the same key: only the one that names a target is it.
        let items = [("y", "focus", Some(panel)), ("k", "float", None), ("y", "again", None)];
        let b = &bar_lines(&items, &look, 80)[0];
        assert_eq!(b.keys[0], (0, 7, panel), "key and label, one target");
        assert_eq!(b.keys[1], (9, 1, hit(k('k'))));
        assert_eq!(b.keys[3], (18, 1, hit(k('y'))), "the other y stays a key");
        // Key text that reads as no key still takes its target.
        let b = &bar_lines(&[("any key", "here", Some(panel))], &look, 80)[0];
        assert_eq!(b.keys, [(0, 12, panel)]);
        // Labels built at run time give the same bar, without a copy.
        let owned: Vec<(&str, String, Option<Hit>)> = items.iter().map(|&(k, l, h)| (k, l.to_string(), h)).collect();
        let (a, b) = (&bar_lines(&items, &look, 12), &bar_lines(&owned, &look, 12));
        assert_eq!(a.len(), b.len());
        for (a, b) in a.iter().zip(b) {
            assert_eq!((&a.line, &a.keys), (&b.line, &b.keys));
        }
    }
}
