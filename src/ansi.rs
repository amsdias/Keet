//! Shared ANSI-aware string helpers for the terminal renderers.
//!
//! One definition per behavior — these used to be copy-pasted across `ui.rs`,
//! `ui_minimal.rs`, `ui_hifi.rs`, and `library.rs`, and the copies had already
//! drifted (one terminated escapes on `m` only, the rest on any letter; an SGR
//! sequence always ends in `m`, but CSI sequences like cursor moves end in
//! other letters — the any-letter form is the safe superset).

/// Count the visible (printable) characters in a string, skipping ANSI escape
/// sequences. An escape runs from `\x1B` to the first ASCII letter.
pub(crate) fn visible_len(s: &str) -> usize {
    let mut n = 0usize;
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            if ch.is_ascii_alphabetic() {
                in_esc = false;
            }
        } else if ch == '\x1B' {
            in_esc = true;
        } else {
            n += 1;
        }
    }
    n
}

/// Truncate to at most `max_width` visible characters, preserving every ANSI
/// escape sequence emitted before the cut point. No ellipsis — used where the
/// caller pads or frames the result itself.
pub(crate) fn truncate_ansi(s: &str, max_width: usize) -> String {
    let mut visible = 0usize;
    let mut out = String::with_capacity(s.len());
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            out.push(ch);
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if ch == '\x1B' {
            in_escape = true;
            out.push(ch);
        } else {
            if visible >= max_width {
                break;
            }
            out.push(ch);
            visible += 1;
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
