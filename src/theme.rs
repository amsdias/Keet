//! Theme palettes and selection.
//!
//! Three themes ship today: Classic (current green-on-default), Minimal
//! (warm-cyan editorial), HiFi (amber CRT studio-monitor). Themes are stored
//! on PlayerState as an AtomicU8 so the UI thread can cycle them without
//! locks. Renderers in `ui.rs` (and the upcoming `ui_minimal.rs` /
//! `ui_hifi.rs`) read the active theme once per frame and consult these
//! palettes/glyphs/casing rules.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ThemeKind {
    Classic = 0,
    Minimal = 1,
    HiFi = 2,
}

impl ThemeKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => ThemeKind::Minimal,
            2 => ThemeKind::HiFi,
            _ => ThemeKind::Classic,
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "classic" => Some(ThemeKind::Classic),
            "minimal" | "min" => Some(ThemeKind::Minimal),
            "hifi" | "retro" => Some(ThemeKind::HiFi),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ThemeKind::Classic => "classic",
            ThemeKind::Minimal => "minimal",
            ThemeKind::HiFi => "hifi",
        }
    }

    pub fn next(self) -> Self {
        match self {
            ThemeKind::Classic => ThemeKind::Minimal,
            ThemeKind::Minimal => ThemeKind::HiFi,
            ThemeKind::HiFi => ThemeKind::Classic,
        }
    }
}

/// Resolve the launch theme by priority: an explicit `--theme` flag wins, then a
/// config.json default (a persistent preference), then the resumed last-session
/// theme, and finally Classic.
pub fn resolve_theme(
    flag: Option<ThemeKind>,
    config: Option<ThemeKind>,
    resume: Option<ThemeKind>,
) -> ThemeKind {
    flag.or(config).or(resume).unwrap_or(ThemeKind::Classic)
}

/// ANSI color escapes for one theme. All strings include their full SGR
/// prefix and can be concatenated directly into output. Reset to clear.
#[allow(dead_code)]
pub struct Palette {
    pub fg: &'static str,
    pub dim: &'static str,
    pub rule: &'static str,    // very dim separator color
    pub accent: &'static str,
    pub good: &'static str,
    pub warn: &'static str,
    pub danger: &'static str,
    pub bold: &'static str,
    pub reset: &'static str,
    /// Highlight for the cursor row in lists: the accent as a background with
    /// dark text over it. Empty = reverse video. Apply it through
    /// [`cursor_row`], never by prefixing a coloured row.
    pub cursor_hl: &'static str,
}

/// Box-drawing glyph set. Currently unused — both renderers emit literal
/// `╔╗║═` glyphs inline; the constants are kept in case a future refactor
/// wants to drive the borders from a single table.
#[allow(dead_code)]
#[derive(Clone, Copy)]
pub struct Borders {
    pub h: char,
    pub v: char,
    pub tl: char,
    pub tr: char,
    pub bl: char,
    pub br: char,
}

#[allow(dead_code)]
impl Borders {
    pub const SINGLE: Self = Self { h: '─', v: '│', tl: '┌', tr: '┐', bl: '└', br: '┘' };
    pub const DOUBLE: Self = Self { h: '═', v: '║', tl: '╔', tr: '╗', bl: '╚', br: '╝' };
}

// Classic: the terminal's own ground and text, and three ANSI colours that
// each mean one thing — signal green (what is playing, where you are, keys),
// caution yellow (peaks, "not bit-perfect", EQ over headroom) and fault red
// (errors, the lit clip lamp). ANSI rather than truecolor on purpose: they
// follow the user's terminal palette, so Classic reads on a light background
// too, and stays distinct from Minimal's cyan and HiFi's amber.
const CLASSIC_PAL: Palette = Palette {
    fg: "\x1B[0m",
    dim: "\x1B[2m",
    rule: "\x1B[2;90m",
    accent: "\x1B[32m",
    good: "\x1B[32m",
    warn: "\x1B[33m",
    danger: "\x1B[31m",
    bold: "\x1B[1m",
    reset: "\x1B[0m",
    cursor_hl: "\x1B[42m\x1B[30m",
};

/// An RGB colour from `#RRGGBB` (or `RRGGBB`). None for anything else.
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let h = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// The truecolor Classic chosen in config.json (`classic_use_truecolor`), set
/// once at startup; unset = the ANSI palette.
static CLASSIC_TRUECOLOR: std::sync::OnceLock<Palette> = std::sync::OnceLock::new();

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Classic in the user's own truecolor highlight, warning and error colours.
/// The cursor bar is the highlight as a background, its text dark or light
/// by the highlight's brightness so it reads on any choice.
fn classic_truecolor_palette(hl: (u8, u8, u8), warn: (u8, u8, u8), err: (u8, u8, u8)) -> Palette {
    let fg = |(r, g, b): (u8, u8, u8)| leak(format!("\x1B[38;2;{r};{g};{b}m"));
    let (r, g, b) = hl;
    let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
    let text = if luma > 140.0 { "\x1B[38;2;14;16;20m" } else { "\x1B[38;2;245;245;245m" };
    Palette {
        accent: fg(hl),
        good: fg(hl),
        warn: fg(warn),
        danger: fg(err),
        cursor_hl: leak(format!("\x1B[48;2;{r};{g};{b}m{text}")),
        ..CLASSIC_PAL
    }
}

/// Switch Classic to truecolor for this run. Called once, before the first frame.
pub fn use_classic_truecolor(hl: (u8, u8, u8), warn: (u8, u8, u8), err: (u8, u8, u8)) {
    let _ = CLASSIC_TRUECOLOR.set(classic_truecolor_palette(hl, warn, err));
}

// Minimal: warm cyan accent on the terminal default background. Truecolor for
// the accent so it lands on #9adcd0 regardless of palette mapping; fg/dim use
// terminal defaults so the theme inherits the user's chosen background.
const MINIMAL_PAL: Palette = Palette {
    fg: "\x1B[0m",
    dim: "\x1B[2m",
    rule: "\x1B[38;2;35;38;42m",
    accent: "\x1B[38;2;154;220;208m",
    good: "\x1B[38;2;154;220;208m",
    warn: "\x1B[38;2;233;196;106m",
    danger: "\x1B[38;2;224;122;122m",
    bold: "\x1B[1m",
    reset: "\x1B[0m",
    cursor_hl: "\x1B[48;2;154;220;208m\x1B[38;2;16;20;22m",
};

// HiFi: amber palette per the design handoff. Truecolor throughout because
// the studio-monitor look depends on the specific oranges; 256-color falls
// back gracefully (most modern terminals support truecolor).
const HIFI_PAL: Palette = Palette {
    fg: "\x1B[38;2;240;200;120m",
    dim: "\x1B[38;2;122;94;58m",
    rule: "\x1B[38;2;58;42;20m",
    accent: "\x1B[1;38;2;255;179;71m",
    good: "\x1B[38;2;168;200;122m",
    warn: "\x1B[38;2;224;122;74m",
    danger: "\x1B[1;38;2;224;122;74m",
    bold: "\x1B[1m",
    reset: "\x1B[0m",
    cursor_hl: "\x1B[48;2;255;179;71m\x1B[38;2;31;20;8m",
};

/// A list's cursor row: `row` as plain text on the theme's highlight, cut or
/// padded to exactly `width` columns. The colours inside `row` are dropped on
/// purpose — every cell of a coloured row ends in a reset, and a reset also
/// clears the background, so prefixing one tinted only the first cell (the
/// track number) and left the rest of the row looking unselected.
pub fn cursor_row(p: &Palette, row: &str, width: usize) -> String {
    let plain = crate::ansi::truncate_plain(&crate::ansi::strip_ansi(row), width);
    let pad = " ".repeat(width.saturating_sub(crate::ansi::visible_len(&plain)));
    // NO_COLOR removes the accent background, so the cursor is reverse video.
    if p.cursor_hl.is_empty() || crate::term::no_color() {
        format!("\x1B[7m{plain}{pad}\x1B[27m")
    } else {
        format!("{}{plain}{pad}{}", p.cursor_hl, p.reset)
    }
}

pub fn palette(kind: ThemeKind) -> &'static Palette {
    match kind {
        ThemeKind::Classic => CLASSIC_TRUECOLOR.get().unwrap_or(&CLASSIC_PAL),
        ThemeKind::Minimal => &MINIMAL_PAL,
        ThemeKind::HiFi => &HIFI_PAL,
    }
}

#[allow(dead_code)]
pub fn borders(kind: ThemeKind) -> Borders {
    match kind {
        ThemeKind::HiFi => Borders::DOUBLE,
        _ => Borders::SINGLE,
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;

    #[test]
    fn hex_colours_parse_with_or_without_the_hash() {
        assert_eq!(parse_hex("#7DD3B8"), Some((125, 211, 184)));
        assert_eq!(parse_hex("e9b65c"), Some((233, 182, 92)));
        assert_eq!(parse_hex(" #F07A78 "), Some((240, 122, 120)));
        for bad in ["", "#fff", "#12345G", "#1234567", "green", "#ééé"] {
            assert_eq!(parse_hex(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_truecolor_classic_uses_the_three_colours_and_keeps_the_rest() {
        let p = classic_truecolor_palette((125, 211, 184), (233, 182, 92), (240, 122, 120));
        assert_eq!(p.accent, "\x1B[38;2;125;211;184m");
        assert_eq!(p.warn, "\x1B[38;2;233;182;92m");
        assert_eq!(p.danger, "\x1B[38;2;240;122;120m");
        assert_eq!((p.fg, p.dim, p.rule), (CLASSIC_PAL.fg, CLASSIC_PAL.dim, CLASSIC_PAL.rule));
        // The cursor text flips with the highlight's brightness.
        assert!(p.cursor_hl.ends_with("\x1B[38;2;14;16;20m"), "light highlight, dark text");
        let dark = classic_truecolor_palette((40, 60, 140), (0, 0, 0), (0, 0, 0));
        assert!(dark.cursor_hl.ends_with("\x1B[38;2;245;245;245m"), "dark highlight, light text");
    }

    #[test]
    fn resolve_theme_priority_flag_then_config_then_resume_then_classic() {
        let c = || ThemeKind::Classic;
        let m = || ThemeKind::Minimal;
        let h = || ThemeKind::HiFi;

        // --theme flag beats everything.
        assert_eq!(resolve_theme(Some(h()), Some(m()), Some(c())), ThemeKind::HiFi);
        // No flag → config default (beats the resumed last-session theme).
        assert_eq!(resolve_theme(None, Some(m()), Some(h())), ThemeKind::Minimal);
        // No flag, no config → resumed last-session theme.
        assert_eq!(resolve_theme(None, None, Some(h())), ThemeKind::HiFi);
        // Nothing set → Classic.
        assert_eq!(resolve_theme(None, None, None), ThemeKind::Classic);
    }

    #[test]
    fn cursor_row_highlights_the_whole_width_in_the_accent() {
        for kind in [ThemeKind::Classic, ThemeKind::Minimal, ThemeKind::HiFi] {
            let p = palette(kind);
            // Coloured input: an inner reset used to end the highlight after
            // the first cell, leaving only the number tinted.
            let row = cursor_row(p, "\x1B[2m07\x1B[0m  Title  \x1B[2mArtist\x1B[0m", 30);
            assert_eq!(crate::ansi::visible_len(&row), 30, "{kind:?}: {row:?}");
            let body = row.trim_end_matches("\x1B[27m").trim_end_matches(p.reset);
            assert!(!body.contains("\x1B[0m"), "{kind:?}: reset inside the highlight: {row:?}");
            assert!(row.contains("07  Title  Artist"), "{kind:?}: {row:?}");
            // Too long: cut, never wider than the row.
            let long = cursor_row(p, &"x".repeat(50), 30);
            assert_eq!(crate::ansi::visible_len(&long), 30);
        }
        // Minimal and HiFi highlight in their accent colour as a background.
        assert!(palette(ThemeKind::Minimal).cursor_hl.contains("48;2;154;220;208"));
        assert!(palette(ThemeKind::HiFi).cursor_hl.contains("48;2;255;179;71"));
    }

    #[test]
    fn from_str_accepts_known_names_and_aliases() {
        assert_eq!(ThemeKind::from_str("minimal"), Some(ThemeKind::Minimal));
        assert_eq!(ThemeKind::from_str("HIFI"), Some(ThemeKind::HiFi));
        assert_eq!(ThemeKind::from_str("retro"), Some(ThemeKind::HiFi));
        assert_eq!(ThemeKind::from_str("nope"), None);
    }
}
