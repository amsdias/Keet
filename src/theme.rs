//! Theme palettes and selection.
//!
//! Three themes ship today: Classic (current green-on-default), Minimal
//! (warm-cyan editorial), HiFi (amber CRT studio-monitor). Themes are stored
//! on PlayerState as an AtomicU8 so the UI thread can cycle them without
//! locks. Renderers read the active theme once per frame and draw in these
//! palettes' ratatui styles.

use ratatui::style::{Color, Modifier, Style};

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

/// One theme's styles. Each is complete on its own (a span takes exactly
/// one); combine them with `Style::patch` where a piece of text is, say, both
/// bold and the accent.
pub struct Palette {
    pub fg: Style,
    pub dim: Style,
    pub rule: Style, // very dim separator color
    pub accent: Style,
    pub good: Style,
    pub warn: Style,
    pub danger: Style,
    pub bold: Style,
    /// Highlight for the cursor row in lists: the accent as a background with
    /// dark text over it (Classic: dark text on its green). Apply it through
    /// [`crate::tui::cursor_row`], never by restyling a coloured row.
    pub cursor_hl: Style,
}

const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);

const fn rgb(r: u8, g: u8, b: u8) -> Style {
    Style::new().fg(Color::Rgb(r, g, b))
}

// Classic: the terminal's own ground and text, and three ANSI colours that
// each mean one thing — signal green (what is playing, where you are, keys),
// caution yellow (peaks, "not bit-perfect", EQ over headroom) and fault red
// (errors, the lit clip lamp). ANSI rather than truecolor on purpose: they
// follow the user's terminal palette, so Classic reads on a light background
// too, and stays distinct from Minimal's cyan and HiFi's amber.
const CLASSIC_PAL: Palette = Palette {
    fg: Style::new(),
    dim: DIM,
    rule: DIM.fg(Color::DarkGray),
    accent: Style::new().fg(Color::Green),
    good: Style::new().fg(Color::Green),
    warn: Style::new().fg(Color::Yellow),
    danger: Style::new().fg(Color::Red),
    bold: BOLD,
    cursor_hl: Style::new().bg(Color::Green).fg(Color::Black),
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

/// Classic in the user's own truecolor highlight, warning and error colours.
/// The cursor bar is the highlight as a background, its text dark or light
/// by the highlight's brightness so it reads on any choice.
fn classic_truecolor_palette(hl: (u8, u8, u8), warn: (u8, u8, u8), err: (u8, u8, u8)) -> Palette {
    let fg = |(r, g, b): (u8, u8, u8)| rgb(r, g, b);
    let (r, g, b) = hl;
    let luma = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
    let text = if luma > 140.0 { Color::Rgb(14, 16, 20) } else { Color::Rgb(245, 245, 245) };
    Palette {
        accent: fg(hl),
        good: fg(hl),
        warn: fg(warn),
        danger: fg(err),
        cursor_hl: Style::new().bg(Color::Rgb(r, g, b)).fg(text),
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
    fg: Style::new(),
    dim: DIM,
    rule: rgb(35, 38, 42),
    accent: rgb(154, 220, 208),
    good: rgb(154, 220, 208),
    warn: rgb(233, 196, 106),
    danger: rgb(224, 122, 122),
    bold: BOLD,
    cursor_hl: Style::new().bg(Color::Rgb(154, 220, 208)).fg(Color::Rgb(16, 20, 22)),
};

// HiFi: amber palette per the design handoff. Truecolor throughout because
// the studio-monitor look depends on the specific oranges; 256-color falls
// back gracefully (most modern terminals support truecolor).
const HIFI_PAL: Palette = Palette {
    fg: rgb(240, 200, 120),
    dim: rgb(122, 94, 58),
    rule: rgb(58, 42, 20),
    accent: rgb(255, 179, 71).add_modifier(Modifier::BOLD),
    good: rgb(168, 200, 122),
    warn: rgb(224, 122, 74),
    // A real red: the peak meter goes amber → orange (warn) → red, and its
    // danger was the warn orange in bold.
    danger: rgb(232, 72, 56).add_modifier(Modifier::BOLD),
    bold: BOLD,
    cursor_hl: Style::new().bg(Color::Rgb(255, 179, 71)).fg(Color::Rgb(31, 20, 8)),
};

pub fn palette(kind: ThemeKind) -> &'static Palette {
    match kind {
        ThemeKind::Classic => CLASSIC_TRUECOLOR.get().unwrap_or(&CLASSIC_PAL),
        ThemeKind::Minimal => &MINIMAL_PAL,
        ThemeKind::HiFi => &HIFI_PAL,
    }
}

/// An RGB colour.
pub type Rgb = (u8, u8, u8);

/// Where a theme sits on the hue wheel (degrees): Classic's green,
/// Minimal's cyan, HiFi's amber. The truecolor art (the first-launch
/// ripples, the galaxy) starts from here.
pub fn base_hue(kind: ThemeKind) -> f32 {
    match kind {
        ThemeKind::Classic => 140.0,
        ThemeKind::Minimal => 175.0,
        ThemeKind::HiFi => 36.0,
    }
}

/// HSV (hue in degrees, saturation and value 0..=1) to RGB.
pub fn hsv(h: f32, s: f32, v: f32) -> Rgb {
    let c = v * s;
    let hp = h.rem_euclid(360.0) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let byte = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (byte(r), byte(g), byte(b))
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
        assert_eq!(p.accent.fg, Some(Color::Rgb(125, 211, 184)));
        assert_eq!(p.warn.fg, Some(Color::Rgb(233, 182, 92)));
        assert_eq!(p.danger.fg, Some(Color::Rgb(240, 122, 120)));
        assert_eq!((p.fg, p.dim, p.rule), (CLASSIC_PAL.fg, CLASSIC_PAL.dim, CLASSIC_PAL.rule));
        // The cursor text flips with the highlight's brightness.
        assert_eq!(p.cursor_hl.fg, Some(Color::Rgb(14, 16, 20)), "light highlight, dark text");
        let dark = classic_truecolor_palette((40, 60, 140), (0, 0, 0), (0, 0, 0));
        assert_eq!(dark.cursor_hl.fg, Some(Color::Rgb(245, 245, 245)), "dark highlight, light text");
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
    fn the_cursor_highlight_is_the_accent_as_a_background() {
        assert_eq!(palette(ThemeKind::Minimal).cursor_hl.bg, Some(Color::Rgb(154, 220, 208)));
        assert_eq!(palette(ThemeKind::HiFi).cursor_hl.bg, Some(Color::Rgb(255, 179, 71)));
        assert_eq!(palette(ThemeKind::Classic).cursor_hl.bg, Some(Color::Green));
    }

    #[test]
    fn from_str_accepts_known_names_and_aliases() {
        assert_eq!(ThemeKind::from_str("minimal"), Some(ThemeKind::Minimal));
        assert_eq!(ThemeKind::from_str("HIFI"), Some(ThemeKind::HiFi));
        assert_eq!(ThemeKind::from_str("retro"), Some(ThemeKind::HiFi));
        assert_eq!(ThemeKind::from_str("nope"), None);
    }
}
