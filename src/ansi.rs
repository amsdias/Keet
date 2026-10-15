//! Shared width and cutting helpers for plain text (the renderers' styled
//! lines have theirs in `tui`, built on these), and `sanitize_display` for
//! untrusted text. They skip ANSI escapes when measuring, so a string that
//! carries one is still measured right.
//!
//! Terminating on "the first ASCII letter" is right for CSI, but wrong for the
//! string-terminated families. A Kitty graphics APC — `ESC _ G a=d,… ESC \` —
//! ends at its `G`, after which the payload gets counted as visible text and
//! the closing `ESC \` then swallows everything after it. Measuring a line
//! that carried an album cover therefore returned nonsense. These walk the
//! sequence families properly.

use ratatui::buffer::CellWidth;
use unicode_segmentation::UnicodeSegmentation;

/// How many BYTES an escape sequence starting at `s[at]` (an `\x1B`)
/// occupies, including the introducer. Every delimiter is ASCII, and no
/// byte of a multi-byte character is, so a walk over bytes never stops
/// inside a character.
///
/// - CSI (`ESC [`): parameters, then a final byte in `@`..`~`
/// - OSC/APC/DCS/PM (`ESC ] _ P ^`): string-terminated, by BEL or `ESC \`
/// - anything else: the ESC and the character after it
fn escape_len(s: &str, at: usize) -> usize {
    let b = s.as_bytes();
    let Some(&next) = b.get(at + 1) else { return 1 };
    match next {
        b'[' => {
            // Parameter and intermediate bytes are 0x20..=0x3F; anything else
            // ends the sequence. A broken one (no final byte) then stops
            // before the next ESC or control character instead of swallowing
            // the text after it, up to whatever letter came next.
            let mut j = at + 2;
            while j < b.len() && matches!(b[j], b' '..=b'?') {
                j += 1;
            }
            match b.get(j) {
                Some(b'@'..=b'~') => j + 1 - at,
                _ => j - at,
            }
        }
        b']' | b'_' | b'P' | b'^' => {
            let mut j = at + 2;
            while j < b.len() {
                if b[j] == 0x07 {
                    return j + 1 - at;
                }
                if b[j] == 0x1B && b.get(j + 1) == Some(&b'\\') {
                    return j + 2 - at;
                }
                j += 1;
            }
            b.len() - at
        }
        _ => 1 + s[at + 1..].chars().next().map_or(0, char::len_utf8),
    }
}

/// Columns `text` (plain, no escapes) takes AS RATATUI DRAWS IT: grapheme
/// by grapheme, each measured by ratatui's own `CellWidth`, a grapheme with
/// a control character in it not drawn at all (`Buffer::set_stringn`). Hand-
/// written rules for emoji sequences, flags, variation selectors and joiners
/// disagreed with the drawing in a new case every time one was fixed (a
/// joiner after CJK, between Sinhala letters, ⌚ + VS15); a measure that is
/// not the drawing's own ends up a column off somewhere.
pub(crate) fn plain_width(text: &str) -> usize {
    // The visualisations' lines (braille, blocks, ASCII) are all single
    // cells: splitting them into graphemes cost as much as drawing them.
    if text.chars().all(single_cell) {
        return text.chars().count();
    }
    text.graphemes(true).map(grapheme_width).sum()
}

/// A character that is one cell on its own and never part of a longer
/// grapheme: ASCII, box drawing, blocks, braille — what the visualisations
/// draw with — and accented Latin letters (Latin-1, Latin Extended-A: most
/// European titles), precomposed, so one char each. Not the soft hyphen
/// (U+00AD); not © ® ▪ ▫ ▶ ◀ ◻ ◼ (emoji glyphs: after a joiner they join
/// the emoji before them); not ◽ ◾ (two cells wide). Each is checked against
/// ratatui's drawing and the grapheme rules
/// (`tui::tests::every_single_cell_character_is_one_cell_as_drawn`); a mark
/// after one is not in the set, so such a string takes the full path.
pub(crate) fn single_cell(c: char) -> bool {
    matches!(
        c,
        ' '..='~'
            | '\u{00A0}'..='\u{00A8}'
            | '\u{00AA}'..='\u{00AC}'
            | '\u{00AF}'..='\u{017F}'
            | '\u{2500}'..='\u{25A9}'
            | '\u{25AC}'..='\u{25B5}'
            | '\u{25B7}'..='\u{25BF}'
            | '\u{25C1}'..='\u{25FA}'
            | '\u{25FF}'
            | '\u{2800}'..='\u{28FF}'
    )
}

/// Columns one grapheme takes as drawn.
pub(crate) fn grapheme_width(g: &str) -> usize {
    if g.contains(char::is_control) { 0 } else { usize::from(g.cell_width()) }
}

/// `s` as plain-text runs and escape sequences (true), in order, without
/// copying or collecting it.
fn segments(s: &str) -> impl Iterator<Item = (bool, &str)> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let rest = s.get(pos..).filter(|r| !r.is_empty())?;
        let len = if rest.starts_with('\x1B') { escape_len(s, pos) } else { rest.find('\x1B').unwrap_or(rest.len()) };
        pos += len;
        Some((rest.starts_with('\x1B'), &rest[..len]))
    })
}

/// Terminal columns a string occupies, skipping ANSI escape sequences of every
/// family (see [`escape_len`]). Counts COLUMNS, not chars: a CJK title of 35
/// chars draws ~70 columns, and a char count let it wrap.
pub(crate) fn visible_len(s: &str) -> usize {
    if !s.contains('\x1B') {
        return plain_width(s);
    }
    segments(s).filter(|(esc, _)| !esc).map(|(_, t)| plain_width(t)).sum()
}

/// Core of every truncation: keep escapes whole, keep whole graphemes while
/// they fit in `max_cols` (a wide one that would straddle the edge is left
/// out, a combining mark stays with its base). Returns the cut string and
/// whether anything printable was dropped.
fn cut_to_width(s: &str, max_cols: usize) -> (String, bool) {
    // Plain single-cell text (most titles, every playlist row each frame):
    // one column a char, no grapheme splitting.
    if s.chars().all(single_cell) {
        return match s.char_indices().nth(max_cols) {
            Some((end, _)) => (s[..end].to_string(), true),
            None => (s.to_string(), false),
        };
    }
    let mut cols = 0usize;
    let mut out = String::with_capacity(s.len());
    let mut styled = false;
    for (esc, text) in segments(s) {
        if esc {
            out.push_str(text);
            styled = true;
            continue;
        }
        for g in text.graphemes(true) {
            let w = grapheme_width(g);
            if cols + w > max_cols {
                // The cut drops the line's closing reset along with its tail; put
                // one back, or the colour runs on into whatever is drawn next.
                if styled {
                    out.push_str("\x1B[0m");
                }
                return (out, true);
            }
            out.push_str(g);
            cols += w;
        }
    }
    (out, false)
}

/// Truncate to at most `max_width` columns, preserving every ANSI escape
/// sequence emitted before the cut point. No ellipsis — used where the caller
/// pads or frames the result itself.
pub(crate) fn truncate_ansi(s: &str, max_width: usize) -> String {
    cut_to_width(s, max_width).0
}

/// Plain text cut into rows of at most `width` columns, in one pass, between
/// graphemes (a flag, a joined emoji, an emoji and its variation selector, a
/// letter and its marks stay whole); one wider than a row (a 2-column glyph
/// in a 1-column row) takes a row alone. Every row is a slice of `s`, and
/// together they are all of it. Escapes are not interpreted: give it plain
/// text.
pub(crate) fn split_columns(s: &str, width: usize) -> Vec<&str> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let (mut start, mut cols) = (0, 0);
    for (i, g) in s.grapheme_indices(true) {
        let w = grapheme_width(g);
        if w > 0 && cols > 0 && cols + w > width {
            rows.push(&s[start..i]);
            start = i;
            cols = 0;
        }
        cols += w;
    }
    if start < s.len() || rows.is_empty() {
        rows.push(&s[start..]);
    }
    rows
}

/// Plain-text truncation to `max_width` columns, with a trailing ellipsis
/// when it actually cuts.
pub(crate) fn truncate_plain(s: &str, max_width: usize) -> String {
    if visible_len(s) <= max_width {
        s.to_string()
    } else if max_width > 1 {
        let mut out = cut_to_width(s, max_width - 1).0;
        out.push('…');
        out
    } else {
        cut_to_width(s, max_width).0
    }
}

/// Join `segments` with `sep`, keeping as many as fit in `width` columns and
/// dropping the rest WHOLE from the end — cutting the joined line mid-way left
/// half a word or a separator dangling at the edge. A first segment too long
/// on its own is cut with an ellipsis. Segments may carry colour escapes.
pub(crate) fn fit_segments(segments: &[String], sep: &str, width: usize) -> String {
    let mut out = String::new();
    for (i, seg) in segments.iter().enumerate() {
        let candidate = if i == 0 { seg.clone() } else { format!("{out}{sep}{seg}") };
        if visible_len(&candidate) > width {
            if i == 0 {
                return truncate_plain(seg, width);
            }
            break;
        }
        out = candidate;
    }
    out
}

/// Make untrusted text (tags, filenames, LRCLIB lyrics) safe to put in a frame
/// line: every control character — C0 (including ESC, `\n`, `\r`, tab), DEL
/// and C1 (including the 8-bit CSI U+009B) — becomes a space. Printed raw, a
/// newline added a physical row the FrameWriter never counted, and an ESC was
/// executed by the terminal: LRCLIB lyrics are user-submitted remote content,
/// and a query sequence smuggled in there has its reply delivered on stdin as
/// keystrokes (the exact hazard behind CLAUDE.md's "never query the terminal").
///
/// Bidirectional controls (embeddings, overrides, isolates, LRM/RLM) are
/// dropped: they take no column, but in a bidi-aware terminal an override in
/// a title (U+202E) reverses everything after it on the row — the separators,
/// the time, the next column — and an unclosed one runs to the end of the line.
/// Right-to-left SCRIPT is left alone; the terminal orders it itself.
pub(crate) fn sanitize_display(s: &str) -> String {
    s.chars()
        .filter(|c| !is_bidi_control(*c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_split_as_they_did_char_by_char() {
        // What the char-by-char parser this replaced made of each (multi-byte
        // text inside and after escapes, broken and unterminated sequences,
        // every family).
        let expected: &[(&str, &[(bool, &str)])] = &[
            ("plain", &[(false, "plain")]),
            ("\u{1b}[31mred\u{1b}[0m", &[(true, "\u{1b}[31m"), (false, "red"), (true, "\u{1b}[0m")]),
            ("a\u{1b}[1;32mé日本\u{1b}[0m", &[(false, "a"), (true, "\u{1b}[1;32m"), (false, "é日本"), (true, "\u{1b}[0m")]),
            ("\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}", &[(true, "\u{1b}]8;;http://x\u{7}"), (false, "link"), (true, "\u{1b}]8;;\u{7}")]),
            ("\u{1b}_Ga=d,i=1;payload\u{1b}\\after", &[(true, "\u{1b}_Ga=d,i=1;payload\u{1b}\\"), (false, "after")]),
            ("\u{1b}P1$r\u{1b}\\", &[(true, "\u{1b}P1$r\u{1b}\\")]),
            ("\u{1b}^privacy\u{7}x", &[(true, "\u{1b}^privacy\u{7}"), (false, "x")]),
            ("\u{1b}[12", &[(true, "\u{1b}[12")]),
            ("\u{1b}[12é", &[(true, "\u{1b}[12"), (false, "é")]),
            ("\u{1b}[3\u{1b}[0mx", &[(true, "\u{1b}[3"), (true, "\u{1b}[0m"), (false, "x")]),
            ("\u{1b}é", &[(true, "\u{1b}é")]),
            ("\u{1b}日本", &[(true, "\u{1b}日"), (false, "本")]),
            ("end\u{1b}", &[(false, "end"), (true, "\u{1b}")]),
            ("\u{1b}]unterminated é", &[(true, "\u{1b}]unterminated é")]),
            ("\u{1b}[?25lé\u{1b}[?25h", &[(true, "\u{1b}[?25l"), (false, "é"), (true, "\u{1b}[?25h")]),
            ("\u{1b}(B", &[(true, "\u{1b}("), (false, "B")]),
            ("\u{1b}_payload with 日本 inside\u{1b}\\ok", &[(true, "\u{1b}_payload with 日本 inside\u{1b}\\"), (false, "ok")]),
            ("ü\u{1b}[38;2;1;2;3mü", &[(false, "ü"), (true, "\u{1b}[38;2;1;2;3m"), (false, "ü")]),
        ];
        for &(s, want) in expected {
            assert_eq!(segments(s).collect::<Vec<_>>(), want, "{s:?}");
        }
    }

    #[test]
    fn segments_drop_whole_from_the_end_never_leaving_a_separator() {
        let segs = ["track 1 of 4", "16-bit stereo", "44.1k", "eq Rock"].map(String::from);
        assert_eq!(fit_segments(&segs, "  ·  ", 100), "track 1 of 4  ·  16-bit stereo  ·  44.1k  ·  eq Rock");
        // "eq Rock" no longer fits: it goes, and its separator with it.
        assert_eq!(fit_segments(&segs, "  ·  ", 40), "track 1 of 4  ·  16-bit stereo  ·  44.1k");
        assert_eq!(fit_segments(&segs, "  ·  ", 20), "track 1 of 4");
        // Even the first does not fit: it is cut (with an ellipsis).
        assert_eq!(fit_segments(&segs, "  ·  ", 8), "track 1…");
        assert_eq!(fit_segments(&segs, "  ·  ", 0), "");
    }

    #[test]
    fn a_broken_csi_stops_at_the_next_escape() {
        // "ESC [ 3" never finished: the next sequence and the text stay.
        assert_eq!(visible_len("\x1B[3\x1B[1mhello"), 5);
    }

    #[test]
    fn bidi_controls_are_dropped_from_untrusted_text() {
        assert_eq!(sanitize_display("ab\u{202E}cd\u{202C}"), "abcd");
        assert_eq!(sanitize_display("\u{2067}x\u{2069}\u{200F}"), "x");
        assert_eq!(sanitize_display("שלום"), "שלום", "right-to-left script stays");
    }

    #[test]
    fn flags_measure_as_one_glyph_and_are_never_split() {
        // A flag is TWO regional indicators drawn as one 2-column glyph.
        let pt = "\u{1F1F5}\u{1F1F9}"; // 🇵🇹
        assert_eq!(visible_len(pt), 2);
        assert_eq!(visible_len(&format!("{pt}{pt}")), 4, "two flags");
        assert_eq!(truncate_ansi(&format!("{pt}{pt}"), 3), pt, "the cut never halves a flag");
        assert_eq!(truncate_ansi(&format!("a{pt}"), 2), "a", "nor leaves half of one");
    }

    #[test]
    fn emoji_sequences_measure_as_drawn() {
        // Counted char by char, these measured wider or narrower than the two
        // columns a terminal draws, and the frame drifted.
        assert_eq!(visible_len("\u{2764}\u{FE0F}"), 2, "VS16 makes ❤ an emoji (2 cols)");
        assert_eq!(visible_len("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"), 2, "ZWJ family");
        assert_eq!(visible_len("\u{1F44D}\u{1F3FD}"), 2, "skin-tone modifier");
        assert_eq!(visible_len("a\u{2764}\u{FE0F}b"), 4);
        // Cutting never splits a sequence.
        assert_eq!(truncate_ansi("\u{1F468}\u{200D}\u{1F469}x", 2), "\u{1F468}\u{200D}\u{1F469}");
    }

    #[test]
    fn a_joiner_between_letters_joins_nothing() {
        // Sinhala and Malayalam write the zero-width joiner inside words
        // (ශ්‍රී): the letter after it is a letter of its own, as wide as
        // ratatui draws it. Only an emoji joins the one after it.
        for text in ["\u{0DC1}\u{0DCA}\u{200D}\u{0DBB}\u{0DD3}", "a\u{200D}b", "\u{0D28}\u{0D4D}\u{200D}\u{0D31}"] {
            let drawn = crate::tui::width(text);
            assert_eq!(visible_len(text), drawn, "{text:?}");
        }
        assert_eq!(visible_len("a\u{200D}b"), 2);
        assert_eq!(split_columns("ab\u{200D}cd", 2), ["ab\u{200D}", "cd"]);
        // A text-style ❤ (no VS16) is no emoji to join either.
        assert_eq!(visible_len("\u{2764}\u{200D}x"), 2);
        // Emoji still join, as ratatui measures them: 👩‍❤️‍👨, ❤️‍🔥, 👩🏽‍💻, 👨‍👩‍👧.
        for text in [
            "\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F468}",
            "\u{2764}\u{FE0F}\u{200D}\u{1F525}",
            "\u{1F469}\u{1F3FD}\u{200D}\u{1F4BB}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
        ] {
            assert_eq!(visible_len(text), 2, "{text:?}");
            assert_eq!(crate::tui::width(text), 2, "{text:?}");
        }
    }

    #[test]
    fn a_joined_emoji_with_a_variation_selector_is_one_glyph() {
        // 👩‍❤️‍👨: the ❤'s VS16 sits inside the joined sequence, which is drawn
        // as one 2-column glyph. Counted as ❤ + VS16 it measured 3 columns,
        // and a cut could fall between the ❤ and its selector.
        let couple = "\u{1F469}\u{200D}\u{2764}\u{FE0F}\u{200D}\u{1F468}";
        assert_eq!(visible_len(couple), 2);
        assert_eq!(visible_len("\u{2764}\u{FE0F}\u{200D}\u{1F525}"), 2, "❤️‍🔥");
        assert_eq!(visible_len(&format!("{couple}{couple}")), 4, "two of them");
        assert_eq!(visible_len(&format!("{couple}\u{2764}\u{FE0F}")), 4, "the join ends with the glyph");
        assert_eq!(truncate_ansi(&format!("{couple}x"), 2), couple);
        let word = format!("aaaaaaaa{couple}");
        assert_eq!(split_columns(&word, 10), [word.as_str()], "it fits in 10");
        assert_eq!(split_columns(&word, 9), ["aaaaaaaa", couple]);
    }

    #[test]
    fn a_cut_line_does_not_bleed_its_colour_into_the_next() {
        // The cut dropped the line's closing reset along with its tail, so its
        // colour (or background) ran on into whatever the terminal drew next.
        let cut = truncate_ansi("\x1B[32mgreen text\x1B[0m", 5);
        assert!(cut.ends_with("\x1B[0m"), "{cut:?}");
        assert_eq!(visible_len(&cut), 5);
        // Nothing dropped, nothing added.
        assert_eq!(truncate_ansi("\x1B[32mok\x1B[0m", 5), "\x1B[32mok\x1B[0m");
        assert_eq!(truncate_ansi("plain text", 5), "plain");
    }

    #[test]
    fn visible_len_handles_string_terminated_escapes() {
        // Kitty graphics ride an APC: ESC _ G <payload> ESC \\ . Terminating the
        // escape at the first ASCII letter stopped at the `G`, counted the
        // payload as text, and then let the closing ESC \\ swallow the rest —
        // so a row carrying an album cover measured short. Found when an empty
        // 18-column cover slot reported 15.
        let kitty = "\x1B_Ga=d,d=i,i=1,q=2\x1B\\";
        assert_eq!(visible_len(kitty), 0, "the whole APC is invisible");
        assert_eq!(visible_len(&format!("{kitty}{}", " ".repeat(18))), 18);
        assert_eq!(visible_len(&format!("{}{kitty}ab", " ".repeat(3))), 5);

        // OSC (iTerm2 inline images) terminates on BEL as well as ESC \\ .
        assert_eq!(visible_len("\x1B]1337;File=inline=1\x07xy"), 2);
        assert_eq!(visible_len("\x1B]0;title\x1B\\xy"), 2);

        // Truncation must keep whole sequences, never cut one in half.
        let line = format!("{kitty}abcdef");
        let cut = truncate_ansi(&line, 3);
        assert_eq!(visible_len(&cut), 3);
        assert!(cut.starts_with(kitty), "escape must survive intact: {cut:?}");

        // An unterminated sequence must not panic or count garbage.
        assert_eq!(visible_len("\x1B_Gnever-closed"), 0);
    }

    #[test]
    fn visible_len_skips_sgr_and_non_sgr_escapes() {
        assert_eq!(visible_len("plain"), 5);
        assert_eq!(visible_len("\x1B[1;32mhi\x1B[0m"), 2);
        // Non-SGR CSI (ends in a letter other than 'm') — the library.rs copy
        // used to keep eating after 'J' because it only terminated on 'm'.
        assert_eq!(visible_len("\x1B[2Jab"), 2);
        assert_eq!(visible_len(""), 0);
    }

    #[test]
    fn truncate_ansi_cuts_visible_chars_and_keeps_escapes() {
        let s = "\x1B[31mabcdef\x1B[0m";
        let cut = truncate_ansi(s, 3);
        assert_eq!(visible_len(&cut), 3);
        assert!(cut.starts_with("\x1B[31m"), "leading escape preserved: {cut:?}");
        // No cut when it already fits (trailing escape intact).
        assert_eq!(truncate_ansi(s, 10), s);
    }

    #[test]
    fn truncate_plain_adds_ellipsis_only_when_cutting() {
        assert_eq!(truncate_plain("hello", 10), "hello");
        assert_eq!(truncate_plain("hello", 4), "hel…");
        assert_eq!(truncate_plain("hello", 1), "h");
    }

    #[test]
    fn widths_are_display_columns_not_chars() {
        // CJK and most emoji take two terminal columns. Counting chars let a
        // 35-char Japanese title "fit" 35 columns while drawing ~70, wrapping
        // the row and drifting every frame after it.
        assert_eq!(visible_len("日本語"), 6);
        assert_eq!(visible_len("\x1B[1m日本\x1B[0mab"), 6);
        // Combining marks add no width: e + U+0301 is one column.
        assert_eq!(visible_len("e\u{301}"), 1);
        // Control characters draw nothing.
        assert_eq!(visible_len("a\tb"), 2);
    }

    #[test]
    fn truncation_never_exceeds_the_column_budget() {
        let title = "日本語の歌のタイトルですとても長い";
        for max in 0..30 {
            let p = truncate_plain(title, max);
            assert!(visible_len(&p) <= max, "truncate_plain({max}) drew {} cols: {p}", visible_len(&p));
            let a = truncate_ansi(&format!("\x1B[1m{title}\x1B[0m"), max);
            assert!(visible_len(&a) <= max, "truncate_ansi({max}) drew {} cols", visible_len(&a));
        }
        // A wide char that would straddle the edge is left out, not split.
        assert_eq!(truncate_ansi("a日本", 2), "a");
        assert_eq!(truncate_plain("日本語の歌", 5), "日本…");
        // A combining mark stays with its base character.
        assert_eq!(truncate_ansi("e\u{301}x", 1), "e\u{301}");
    }

    #[test]
    fn untrusted_text_cannot_carry_control_characters() {
        // Tags and LRCLIB lyrics (user-submitted, remote) went straight into
        // frame lines: a `\n` added a physical row and an ESC was executed — including query sequences whose replies arrive on
        // stdin as keystrokes.
        assert_eq!(sanitize_display("a\x1B[6nb"), "a [6nb");
        assert_eq!(sanitize_display("line1\nline2\r"), "line1 line2 ");
        assert_eq!(sanitize_display("tab\there"), "tab here");
        assert_eq!(sanitize_display("c1\u{9b}6n"), "c1 6n", "8-bit CSI too");
        assert_eq!(sanitize_display("日本語 é"), "日本語 é", "printable text untouched");
    }
}
