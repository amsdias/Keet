//! Shared ANSI-aware string helpers for the terminal renderers.
//!
//! One definition per behavior — these used to be copy-pasted across `ui.rs`,
//! `ui_minimal.rs`, `ui_hifi.rs`, and `library.rs`, and the copies had already
//! drifted (one terminated escapes on `m` only, the rest on any letter).
//!
//! Terminating on "the first ASCII letter" is right for CSI, but wrong for the
//! string-terminated families. A Kitty graphics APC — `ESC _ G a=d,… ESC \` —
//! ends at its `G`, after which the payload gets counted as visible text and
//! the closing `ESC \` then swallows everything after it. Measuring a line
//! that carried an album cover therefore returned nonsense. These walk the
//! sequence families properly.

/// How many chars an escape sequence starting at `bytes[i]` (an `\x1B`)
/// occupies, including the introducer.
///
/// - CSI (`ESC [`): parameters, then a final byte in `@`..`~`
/// - OSC/APC/DCS/PM (`ESC ] _ P ^`): string-terminated, by BEL or `ESC \`
/// - anything else: a two-character sequence
fn escape_len(chars: &[char], i: usize) -> usize {
    let next = match chars.get(i + 1) {
        Some(c) => *c,
        None => return 1,
    };
    match next {
        '[' => {
            let mut j = i + 2;
            while j < chars.len() && !matches!(chars[j], '@'..='~') {
                j += 1;
            }
            (j + 1).min(chars.len()) - i
        }
        ']' | '_' | 'P' | '^' => {
            let mut j = i + 2;
            while j < chars.len() {
                if chars[j] == '\u{7}' {
                    return j + 1 - i;
                }
                if chars[j] == '\x1B' && chars.get(j + 1) == Some(&'\\') {
                    return j + 2 - i;
                }
                j += 1;
            }
            chars.len() - i
        }
        _ => 2,
    }
}

/// Terminal columns one character occupies: 2 for CJK and most emoji, 0 for
/// combining marks and control characters, 1 otherwise.
fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Terminal columns a string occupies, skipping ANSI escape sequences of every
/// family (see [`escape_len`]). Counts COLUMNS, not chars: a CJK title of 35
/// chars draws ~70 columns, and a char count let it wrap.
pub(crate) fn visible_len(s: &str) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut n = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\x1B' {
            i += escape_len(&chars, i);
        } else {
            n += char_width(chars[i]);
            i += 1;
        }
    }
    n
}

/// Core of every truncation: keep escapes whole, keep printable characters
/// while they fit in `max_cols`, never split a wide character across the edge
/// (it is left out instead), and keep zero-width combining marks with their
/// base. Returns the cut string and whether anything printable was dropped.
fn cut_to_width(s: &str, max_cols: usize) -> (String, bool) {
    let chars: Vec<char> = s.chars().collect();
    let mut cols = 0usize;
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\x1B' {
            let n = escape_len(&chars, i);
            out.extend(&chars[i..(i + n).min(chars.len())]);
            i += n;
            continue;
        }
        let w = char_width(chars[i]);
        if cols + w > max_cols {
            return (out, true);
        }
        out.push(chars[i]);
        cols += w;
        i += 1;
    }
    (out, false)
}

/// Truncate to at most `max_width` columns, preserving every ANSI escape
/// sequence emitted before the cut point. No ellipsis — used where the caller
/// pads or frames the result itself.
pub(crate) fn truncate_ansi(s: &str, max_width: usize) -> String {
    cut_to_width(s, max_width).0
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

/// ANSI-aware truncation with a trailing ellipsis when it actually cuts:
/// escapes pass through (colors survive), only printable columns count against
/// `max`. Prevents a long name from wrapping (which would drift the caller's
/// line count) or overflowing a bordered frame.
pub(crate) fn truncate_visible(s: &str, max: usize) -> String {
    if visible_len(s) <= max {
        return s.to_string();
    }
    let mut out = cut_to_width(s, max.saturating_sub(1)).0; // room for the ellipsis
    out.push('…');
    out
}

/// Make untrusted text (tags, filenames, LRCLIB lyrics) safe to put in a frame
/// line: every control character — C0 (including ESC, `\n`, `\r`, tab), DEL
/// and C1 (including the 8-bit CSI U+009B) — becomes a space. Printed raw, a
/// newline added a physical row the FrameWriter never counted, and an ESC was
/// executed by the terminal: LRCLIB lyrics are user-submitted remote content,
/// and a query sequence smuggled in there has its reply delivered on stdin as
/// keystrokes (the exact hazard behind CLAUDE.md's "never query the terminal").
pub(crate) fn sanitize_display(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn truncate_visible_is_ansi_aware_with_ellipsis() {
        let s = "\x1B[2mabcdef\x1B[0m";
        assert_eq!(truncate_visible(s, 10), s, "no cut when it fits");
        let cut = truncate_visible(s, 4);
        assert_eq!(visible_len(&cut), 4, "3 kept chars + ellipsis");
        assert!(cut.ends_with('…'));
        assert!(cut.starts_with("\x1B[2m"));
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
            let v = truncate_visible(&format!("\x1B[2m{title}"), max);
            assert!(visible_len(&v) <= max.max(1), "truncate_visible({max}) drew {} cols", visible_len(&v));
        }
        // A wide char that would straddle the edge is left out, not split.
        assert_eq!(truncate_ansi("a日本", 2), "a");
        assert_eq!(truncate_plain("日本語の歌", 5), "日本…");
        // A combining mark stays with its base character.
        assert_eq!(truncate_ansi("e\u{301}x", 1), "e\u{301}");
    }

    #[test]
    fn truncate_visible_keeps_string_terminated_escapes_whole() {
        // It used to end an escape at the first ASCII letter — the old bug
        // escape_len exists to fix — so an APC's payload counted as text.
        let kitty = "\x1B_Ga=d,d=i,i=1,q=2\x1B\\";
        let cut = truncate_visible(&format!("{kitty}abcdef"), 4);
        assert!(cut.starts_with(kitty), "APC must survive whole: {cut:?}");
        assert_eq!(visible_len(&cut), 4);
    }

    #[test]
    fn untrusted_text_cannot_carry_control_characters() {
        // Tags and LRCLIB lyrics (user-submitted, remote) went straight into
        // frame lines: a `\n` added a physical row (FrameWriter drift) and an
        // ESC was executed — including query sequences whose replies arrive on
        // stdin as keystrokes.
        assert_eq!(sanitize_display("a\x1B[6nb"), "a [6nb");
        assert_eq!(sanitize_display("line1\nline2\r"), "line1 line2 ");
        assert_eq!(sanitize_display("tab\there"), "tab here");
        assert_eq!(sanitize_display("c1\u{9b}6n"), "c1 6n", "8-bit CSI too");
        assert_eq!(sanitize_display("日本語 é"), "日本語 é", "printable text untouched");
    }
}
