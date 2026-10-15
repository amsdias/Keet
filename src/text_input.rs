//! A one-line text field: the search query, the "save as" name and the path
//! prompt all edit through it. It keeps a cursor that moves inside the text
//! (arrows, Home/End, a word at a time with Ctrl or Alt) and the usual line
//! editing keys (Ctrl+A/E/U/K/W, Delete), takes pasted text whole, and shows
//! a window of the text that keeps the cursor in view. Pure: the caller
//! decides what Enter and Esc mean and draws the result.

use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What a key did to the field.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Edit {
    /// The text changed.
    Changed,
    /// Only the cursor moved.
    Moved,
    /// Enter.
    Submit,
    /// Esc.
    Cancel,
    /// Not a key the field uses (the caller may: Up/Down move a list).
    Ignored,
}

/// Pasted or dropped text as a field takes it: control characters at its
/// ends (a copied line's newline, a tab) gone, those inside (an escape
/// code's ESC) as spaces — kept, a newline stayed in a path ("not found") or
/// a search ("beatles " matched nothing), and the cursor drifted from what
/// ratatui drew. Nothing real goes: spaces stay ("~/My" + " Music"), and so
/// do bidi marks, which can be part of a folder name (Hebrew, Arabic). The
/// user's own paste, not untrusted display text.
pub fn clean(s: &str) -> String {
    s.trim_matches(char::is_control).chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TextInput {
    text: String,
    /// Byte offset of the cursor, always on a char boundary.
    cursor: usize,
}

impl TextInput {
    pub fn new() -> Self {
        Self::default()
    }

    /// A field holding `text`, the cursor at its end.
    pub fn with(text: &str) -> Self {
        Self { text: text.to_string(), cursor: text.len() }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Insert `s` at the cursor (a paste, or a dropped path). Control
    /// characters become spaces: a pasted newline must not submit the field.
    pub fn insert(&mut self, s: &str) {
        let clean = clean(s);
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.settle();
    }

    /// An edit can join characters into one (🇧 typed before 🇵, or the x
    /// deleted from 🇵x🇹, makes a flag): the cursor goes to its end, never
    /// inside it (where the next key split it again).
    fn settle(&mut self) {
        let mut c = GraphemeCursor::new(self.cursor, self.text.len(), true);
        if !c.is_boundary(&self.text, 0).unwrap_or(true) {
            self.cursor = self.next(self.cursor);
        }
    }

    /// The grapheme boundary before `at`: what is drawn as one character
    /// (👩‍❤️‍👨, ❤️, a letter and its marks) is stepped over and deleted
    /// whole, never left in pieces.
    fn prev(&self, at: usize) -> usize {
        let mut c = GraphemeCursor::new(at, self.text.len(), true);
        c.prev_boundary(&self.text, 0).ok().flatten().unwrap_or(0)
    }

    fn next(&self, at: usize) -> usize {
        let mut c = GraphemeCursor::new(at, self.text.len(), true);
        c.next_boundary(&self.text, 0).ok().flatten().unwrap_or(self.text.len())
    }

    /// The start of the word before `at` (skipping the spaces before it).
    fn word_back(&self, at: usize) -> usize {
        let mut i = at;
        while i > 0 && self.text[..i].ends_with(char::is_whitespace) {
            i = self.prev(i);
        }
        while i > 0 && !self.text[..i].ends_with(char::is_whitespace) {
            i = self.prev(i);
        }
        i
    }

    /// The end of the word after `at` (skipping the spaces before it).
    fn word_forward(&self, at: usize) -> usize {
        let mut i = at;
        while i < self.text.len() && self.text[i..].starts_with(char::is_whitespace) {
            i = self.next(i);
        }
        while i < self.text.len() && !self.text[i..].starts_with(char::is_whitespace) {
            i = self.next(i);
        }
        i
    }

    /// Apply one key press.
    pub fn handle(&mut self, key: KeyEvent) -> Edit {
        // Windows reports AltGr as Ctrl+Alt: with both, it is a character
        // (`@ [ ] { } \` on many layouts), not a command.
        let altgr = key.modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL) && !altgr;
        let alt = key.modifiers.contains(KeyModifiers::ALT) && !altgr;
        let moved = |s: &mut Self, to: usize| {
            s.cursor = to;
            Edit::Moved
        };
        match key.code {
            KeyCode::Enter => Edit::Submit,
            KeyCode::Esc => Edit::Cancel,
            KeyCode::Left if ctrl || alt => moved(self, self.word_back(self.cursor)),
            KeyCode::Right if ctrl || alt => moved(self, self.word_forward(self.cursor)),
            KeyCode::Left => moved(self, self.prev(self.cursor)),
            KeyCode::Right => moved(self, self.next(self.cursor)),
            KeyCode::Home => moved(self, 0),
            KeyCode::End => moved(self, self.text.len()),
            KeyCode::Char('a') if ctrl => moved(self, 0),
            KeyCode::Char('e') if ctrl => moved(self, self.text.len()),
            KeyCode::Char('b') if alt => moved(self, self.word_back(self.cursor)),
            KeyCode::Char('f') if alt => moved(self, self.word_forward(self.cursor)),
            KeyCode::Backspace if ctrl || alt => self.cut(self.word_back(self.cursor), self.cursor),
            KeyCode::Char('w') if ctrl => self.cut(self.word_back(self.cursor), self.cursor),
            KeyCode::Char('u') if ctrl => self.cut(0, self.cursor),
            KeyCode::Char('k') if ctrl => self.cut(self.cursor, self.text.len()),
            KeyCode::Backspace => self.cut(self.prev(self.cursor), self.cursor),
            KeyCode::Delete => self.cut(self.cursor, self.next(self.cursor)),
            // Alt-composed characters too (Alt+b/f, matched above, move by
            // word); Ctrl ones are commands.
            KeyCode::Char(c) if !ctrl => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.settle();
                Edit::Changed
            }
            _ => Edit::Ignored,
        }
    }

    /// Remove `from..to`; Moved (nothing to remove) when it is empty.
    fn cut(&mut self, from: usize, to: usize) -> Edit {
        if from >= to {
            return Edit::Moved;
        }
        self.text.replace_range(from..to, "");
        self.cursor = from;
        self.settle();
        Edit::Changed
    }

    /// What fits in `width` columns, and the cursor's column in it: the text
    /// scrolled so the cursor stays in view (with a column for the cursor at
    /// the very end), cut with `…` at either end where it runs on.
    pub fn view(&self, width: usize) -> (String, usize) {
        // Measured as ratatui draws it: grapheme by grapheme.
        let units: Vec<(&str, usize)> = self.text.graphemes(true).map(|g| (g, crate::ansi::grapheme_width(g))).collect();
        let w = |u: &(&str, usize)| u.1;
        let ci = self.text[..self.cursor].graphemes(true).count();
        if width == 0 {
            return (String::new(), 0);
        }
        if units.iter().map(w).sum::<usize>() < width {
            return (self.text.clone(), units[..ci].iter().map(w).sum());
        }
        // The first unit shown: as early as leaves the cursor's own column
        // on screen (after a leading `…` when the start is cut).
        // (A field of one or two columns has no room for the `…`.)
        let lead = |s: usize| usize::from(s > 0 && width >= 3);
        let mut start = 0;
        while start < ci && lead(start) + units[start..ci].iter().map(w).sum::<usize>() + 1 > width {
            start += 1;
        }
        let mut out = String::new();
        if lead(start) == 1 {
            out.push('…');
        }
        out.extend(units[start..ci].iter().map(|u| u.0));
        let col = lead(start) + units[start..ci].iter().map(w).sum::<usize>();
        // Then what follows, keeping a column for a trailing `…` while more
        // remains.
        let mut used = col;
        let mut i = ci;
        while i < units.len() {
            let more = usize::from(i + 1 < units.len());
            if used + w(&units[i]) + more > width {
                break;
            }
            out.push_str(units[i].0);
            used += w(&units[i]);
            i += 1;
        }
        if i < units.len() && used < width && width >= 3 {
            out.push('…');
        }
        (out, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(s: &str) -> TextInput {
        let mut t = TextInput::new();
        for c in s.chars() {
            t.handle(key(KeyCode::Char(c)));
        }
        t
    }

    #[test]
    fn a_character_drawn_as_one_is_edited_and_measured_as_one() {
        // 👩‍❤️‍👨 is six chars drawn as one 2-column glyph; ❤️ two chars;
        // ｶﾞ two chars drawn 2 columns (1 + 1).
        let couple = "\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F468}";
        let mut t = TextInput::with(&format!("a{couple}b"));
        assert_eq!(t.view(40), (format!("a{couple}b"), 4), "the cursor after 1 + 2 + 1 columns");
        t.handle(key(KeyCode::Left));
        t.handle(key(KeyCode::Left));
        assert_eq!(t.view(40).1, 1, "one Left steps over the whole emoji");
        t.handle(key(KeyCode::Delete));
        assert_eq!(t.text(), "ab", "Delete removes all of it");
        let mut t = TextInput::with("x\u{2764}\u{FE0F}");
        t.handle(key(KeyCode::Backspace));
        assert_eq!(t.text(), "x", "Backspace removes the emoji with its selector");
        let t = TextInput::with("\u{FF76}\u{FF9E}\u{FF76}\u{FF9E}");
        assert_eq!(t.view(40).1, 4, "ｶﾞ is drawn two columns");
        // Cut to a narrow field: never wider than it, the cursor inside it,
        // at the drawn width of what is before it.
        for text in [format!("{couple}{couple}abc{couple}"), "\u{FF76}\u{FF9E}".repeat(8), "日本語のパス/x".to_string()] {
            for w in 1..12 {
                let mut t = TextInput::with(&text);
                for _ in 0..12 {
                    let (shown, col) = t.view(w);
                    assert!(crate::tui::width(&shown) <= w, "{shown:?} in {w}");
                    assert!(col <= w, "{col} in {w}");
                    t.handle(key(KeyCode::Left));
                }
            }
        }
    }

    #[test]
    fn typing_that_joins_a_character_leaves_the_cursor_after_it() {
        // 🇵 alone, then 🇧 typed before it: the two are one flag, 🇧🇵, and the
        // cursor is after it (inside it, a typed x split the flag).
        let mut t = TextInput::with("\u{1F1F5}");
        t.handle(key(KeyCode::Home));
        t.handle(key(KeyCode::Char('\u{1F1E7}')));
        assert_eq!(t.view(40).1, 2);
        t.handle(key(KeyCode::Char('x')));
        assert_eq!(t.text(), "\u{1F1E7}\u{1F1F5}x");
        // A pasted half too.
        let mut t = TextInput::with("\u{1F1F5}");
        t.handle(key(KeyCode::Home));
        t.insert("\u{1F1E7}");
        t.handle(key(KeyCode::Char('x')));
        assert_eq!(t.text(), "\u{1F1E7}\u{1F1F5}x");
        // Deleting what stood between two halves joins them: 🇵x🇹, Backspace
        // after the x, is the flag 🇵🇹 with the cursor after it.
        let mut t = TextInput::with("\u{1F1F5}x\u{1F1F9}");
        t.handle(key(KeyCode::Left));
        t.handle(key(KeyCode::Backspace));
        t.handle(key(KeyCode::Char('y')));
        assert_eq!(t.text(), "\u{1F1F5}\u{1F1F9}y", "Backspace");
        let mut t = TextInput::with("\u{1F1F5}x\u{1F1F9}");
        t.handle(key(KeyCode::Home));
        t.handle(key(KeyCode::Right));
        t.handle(key(KeyCode::Delete));
        t.handle(key(KeyCode::Char('y')));
        assert_eq!(t.text(), "\u{1F1F5}\u{1F1F9}y", "Delete");
    }

    #[test]
    fn pasted_text_loses_line_breaks_at_its_ends_and_nothing_real() {
        // A copied line's newline goes (a search for "Beatles\n" matched
        // nothing as "beatles "); controls inside become spaces.
        assert_eq!(clean("Beatles\n"), "Beatles");
        assert_eq!(clean("\r\nmix\t"), "mix");
        assert_eq!(clean("a\nb\x1B[31mc"), "a b [31mc");
        // Real spaces stay ("~/My" + " Music" is "~/My Music"), and so do
        // bidi marks, which can be part of a real folder name.
        assert_eq!(clean(" Music"), " Music");
        assert_eq!(clean("\u{05E9}\u{200F}x"), "\u{05E9}\u{200F}x");
        let mut t = TextInput::with("mix");
        t.insert(" 2\n");
        assert_eq!(t.text(), "mix 2");
    }

    #[test]
    fn altgr_and_alt_composed_characters_are_typed() {
        // Windows reports AltGr as Ctrl+Alt: `@ [ ] { } \` on many layouts.
        let mut t = TextInput::new();
        for c in ['@', '\\', '{', '[', '€'] {
            t.handle(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL | KeyModifiers::ALT));
        }
        assert_eq!(t.text(), "@\\{[€");
        // An Alt-composed character (not one of the word keys) is typed too.
        t.handle(KeyEvent::new(KeyCode::Char('ç'), KeyModifiers::ALT));
        assert_eq!(t.text(), "@\\{[€ç");
        // Ctrl alone is a command, never typed; Alt+b/f still move by word.
        assert_eq!(t.handle(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)), Edit::Ignored);
        assert_eq!(t.handle(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)), Edit::Moved);
        assert_eq!(t.text(), "@\\{[€ç");
    }

    #[test]
    fn typing_inserts_at_the_cursor_not_the_end() {
        let mut t = typed("helo");
        t.handle(key(KeyCode::Left));
        t.handle(key(KeyCode::Char('l')));
        assert_eq!(t.text(), "hello");
        t.handle(key(KeyCode::Home));
        t.handle(key(KeyCode::Char('>')));
        assert_eq!(t.text(), ">hello");
        t.handle(key(KeyCode::End));
        assert_eq!(t.handle(key(KeyCode::Char('!'))), Edit::Changed);
        assert_eq!(t.text(), ">hello!");
    }

    #[test]
    fn deleting_works_on_both_sides_and_by_word_and_line() {
        let mut t = typed("one two three");
        assert_eq!(t.handle(ctrl('w')), Edit::Changed);
        assert_eq!(t.text(), "one two ");
        t.handle(key(KeyCode::Home));
        assert_eq!(t.handle(key(KeyCode::Delete)), Edit::Changed);
        assert_eq!(t.text(), "ne two ");
        assert_eq!(t.handle(key(KeyCode::Backspace)), Edit::Moved, "nothing before the cursor");
        t.handle(key(KeyCode::Right));
        t.handle(key(KeyCode::Right));
        t.handle(ctrl('k'));
        assert_eq!(t.text(), "ne");
        t.handle(ctrl('u'));
        assert_eq!(t.text(), "");
    }

    #[test]
    fn words_jump_with_ctrl_or_alt() {
        let mut t = typed("/music/some album");
        let word = |t: &mut TextInput, code, m| t.handle(KeyEvent::new(code, m));
        word(&mut t, KeyCode::Left, KeyModifiers::CONTROL);
        t.handle(key(KeyCode::Char('X')));
        assert_eq!(t.text(), "/music/some Xalbum");
        word(&mut t, KeyCode::Left, KeyModifiers::ALT);
        word(&mut t, KeyCode::Left, KeyModifiers::ALT);
        word(&mut t, KeyCode::Right, KeyModifiers::CONTROL);
        t.handle(key(KeyCode::Char('Y')));
        assert_eq!(t.text(), "/music/someY Xalbum");
    }

    #[test]
    fn multibyte_characters_move_and_delete_whole() {
        let mut t = typed("日本é");
        t.handle(key(KeyCode::Left));
        t.handle(key(KeyCode::Backspace));
        assert_eq!(t.text(), "日é");
        t.handle(key(KeyCode::Char('ü')));
        assert_eq!(t.text(), "日üé");
    }

    #[test]
    fn a_paste_goes_in_whole_and_cannot_submit() {
        let mut t = typed("ab");
        t.handle(key(KeyCode::Left));
        t.insert("/x y\nz");
        assert_eq!(t.text(), "a/x y zb");
    }

    #[test]
    fn enter_and_esc_are_the_callers_and_other_keys_pass_through() {
        let mut t = typed("q");
        assert_eq!(t.handle(key(KeyCode::Enter)), Edit::Submit);
        assert_eq!(t.handle(key(KeyCode::Esc)), Edit::Cancel);
        assert_eq!(t.handle(key(KeyCode::Up)), Edit::Ignored);
        assert_eq!(t.handle(ctrl('c')), Edit::Ignored, "Ctrl+C is never typed");
        assert_eq!(t.text(), "q");
    }

    #[test]
    fn the_view_keeps_the_cursor_inside_the_width() {
        let t = typed("/Users/someone/Music/a very long folder name/日本語");
        for width in 1..60 {
            let (shown, col) = t.view(width);
            let w = crate::tui::width(&shown);
            assert!(w <= width, "{width}: {shown:?} is {w} wide");
            assert!(col < width, "{width}: cursor at {col}");
        }
        // Short text shows whole, the cursor after it.
        assert_eq!(typed("abc").view(10), ("abc".to_string(), 3));
        // Long text at the end shows its end.
        let (shown, col) = typed("abcdefghij").view(6);
        assert_eq!(shown, "…ghij");
        assert_eq!(col, 5);
        // Cursor at the start of a long text shows its start.
        let mut s = typed("abcdefghij");
        s.handle(key(KeyCode::Home));
        assert_eq!(s.view(6), ("abcde…".to_string(), 0));
    }
}
