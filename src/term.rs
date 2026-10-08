//! The main thread's terminal output, batched into one write per frame.
//!
//! Rust's stdout is line-buffered, so every frame line (each starts with
//! "\n") cost its own write syscall: 30-60 per frame at 20 fps, through
//! ConPTY on Windows. Everything the UI draws goes through [`out!`] into one
//! buffer instead, and [`flush`] hands the whole frame to the terminal in a
//! single write — which also keeps a frame from ever reaching the screen
//! half-drawn. All main-thread terminal output must use it: a direct `print!`
//! would land before the buffered text queued ahead of it.

use std::cell::RefCell;
use std::io::Write as _;

thread_local! {
    static BUF: RefCell<String> = RefCell::new(String::with_capacity(64 * 1024));
}

/// Queue text for the terminal (see [`out!`]).
pub fn push(args: std::fmt::Arguments) {
    // Tests render thousands of frames and never flush: drop their output
    // rather than let the buffer grow without bound.
    if cfg!(test) {
        return;
    }
    BUF.with(|b| {
        let _ = std::fmt::Write::write_fmt(&mut *b.borrow_mut(), args);
    });
}

/// Throw away everything queued (a frame a panic interrupted half-built:
/// written out on the way down, it left a garbled screen under the message).
pub fn discard() {
    BUF.with(|b| b.borrow_mut().clear());
}

/// Write everything queued in one go, then flush the terminal.
pub fn flush() {
    BUF.with(|b| {
        let mut b = b.borrow_mut();
        let mut stdout = std::io::stdout().lock();
        if !b.is_empty() {
            if no_color() {
                let _ = stdout.write_all(strip_colour(&b).as_bytes());
            } else {
                let _ = stdout.write_all(b.as_bytes());
            }
            b.clear();
        }
        let _ = stdout.flush();
    });
}

/// `NO_COLOR` (https://no-color.org): set to anything non-empty, Keet draws
/// no colour. Bold, dim and reverse stay — with colour gone they, and the
/// state glyphs (▶ ⏸ ● ○ ✓ ▲), carry every state.
pub fn no_color() -> bool {
    static NO_COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *NO_COLOR.get_or_init(|| std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()))
}

/// Remove every colour from SGR sequences (`ESC [ … m`), keeping the other
/// attributes. Applied to the whole frame at the one place it is written, so
/// no renderer has to know. Image payloads (Sixel DCS, Kitty APC) carry no
/// `ESC [` inside, so they pass through untouched.
pub fn strip_colour(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("\x1B[") {
        out.push_str(&rest[..i]);
        let seq = &rest[i + 2..];
        // Parameters, then one final byte in 0x40..=0x7E.
        let end = seq.find(|c: char| ('\x40'..='\x7E').contains(&c));
        match end {
            Some(e) if seq.as_bytes()[e] == b'm' => {
                let kept = sgr_without_colour(&seq[..e]);
                // A sequence that was only colour goes; an empty one resets.
                if !kept.is_empty() || seq[..e].is_empty() {
                    out.push_str("\x1B[");
                    out.push_str(&kept);
                    out.push('m');
                }
                rest = &seq[e + 1..];
            }
            Some(e) => {
                out.push_str(&rest[i..i + 2 + e + 1]);
                rest = &seq[e + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The non-colour parameters of one SGR parameter list ("1;38;2;1;2;3" → "1").
fn sgr_without_colour(params: &str) -> String {
    let p: Vec<&str> = params.split(';').collect();
    let mut kept = Vec::new();
    let mut i = 0;
    while i < p.len() {
        let n: u32 = p[i].parse().unwrap_or(0);
        match n {
            // Extended colour: 38/48/58 ; 5 ; n  or  ; 2 ; r ; g ; b
            38 | 48 | 58 => {
                i += match p.get(i + 1).copied() {
                    Some("5") => 3,
                    Some("2") => 5,
                    _ => 1,
                };
                continue;
            }
            30..=37 | 39 | 40..=47 | 49 | 59 | 90..=97 | 100..=107 => {}
            _ => kept.push(p[i]),
        }
        i += 1;
    }
    kept.join(";")
}

/// `print!` into the frame buffer.
macro_rules! out {
    ($($arg:tt)*) => { $crate::term::push(format_args!($($arg)*)) };
}
pub(crate) use out;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_goes_and_attributes_stay() {
        assert_eq!(strip_colour("\x1B[1;38;2;255;179;71mKEET\x1B[0m"), "\x1B[1mKEET\x1B[0m");
        assert_eq!(strip_colour("\x1B[32m▶\x1B[0m \x1B[2mdim\x1B[m"), "▶\x1B[0m \x1B[2mdim\x1B[m");
        assert_eq!(strip_colour("\x1B[48;5;17;7mx"), "\x1B[7mx");
        assert_eq!(strip_colour("\x1B[2;90m─"), "\x1B[2m─");
    }

    #[test]
    fn discard_drops_a_half_built_frame() {
        BUF.with(|b| b.borrow_mut().push_str("half a frame"));
        discard();
        assert!(BUF.with(|b| b.borrow().is_empty()));
    }

    #[test]
    fn other_sequences_and_image_payloads_pass_through() {
        let s = "\x1B[3F\x1B[K\x1B[?2026h\x1BPq#0;2;100;0;0~~\x1B\\\x1B_Gf=100;AAAA\x1B\\";
        assert_eq!(strip_colour(s), s);
        assert_eq!(strip_colour("plain text"), "plain text");
        assert_eq!(strip_colour("cut off \x1B[3"), "cut off \x1B[3");
    }
}
