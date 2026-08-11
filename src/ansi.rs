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

/// Count the visible (printable) characters in a string, skipping ANSI escape
/// sequences of every family (see [`escape_len`]).
pub(crate) fn visible_len(s: &str) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut n = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\x1B' {
            i += escape_len(&chars, i);
        } else {
            n += 1;
            i += 1;
        }
    }
    n
}

/// Truncate to at most `max_width` visible characters, preserving every ANSI
/// escape sequence emitted before the cut point. No ellipsis — used where the
/// caller pads or frames the result itself.
pub(crate) fn truncate_ansi(s: &str, max_width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut visible = 0usize;
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\x1B' {
            let n = escape_len(&chars, i);
            out.extend(&chars[i..(i + n).min(chars.len())]);
            i += n;
        } else {
            if visible >= max_width {
                break;
            }
            out.push(chars[i]);
            visible += 1;
            i += 1;
        }
    }
    out
}

/// Plain-text truncation with a trailing ellipsis when it actually cuts.
pub(crate) fn truncate_plain(s: &str, max_width: usize) -> String {
    if s.chars().count() <= max_width {
        s.to_string()
    } else if max_width > 1 {
        let mut out: String = s.chars().take(max_width - 1).collect();
        out.push('…');
        out
    } else {
        s.chars().take(max_width).collect()
    }
}

/// ANSI-aware truncation with a trailing ellipsis when it actually cuts:
/// escapes pass through (colors survive), only printable chars count against
/// `max`. Prevents a long name from wrapping (which would drift the caller's
/// line count) or overflowing a bordered frame.
pub(crate) fn truncate_visible(s: &str, max: usize) -> String {
    if visible_len(s) <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1); // room for the ellipsis
    let mut out = String::new();
    let mut n = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            out.push(c);
            if c.is_ascii_alphabetic() {
                in_esc = false;
            }
        } else if c == '\x1B' {
            in_esc = true;
            out.push(c);
        } else if n < keep {
            out.push(c);
            n += 1;
        }
    }
    out.push('…');
    out
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
}
