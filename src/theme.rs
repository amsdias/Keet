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
    /// Background tint for the cursor row in lists. Empty string = no tint.
    pub cursor_bg: &'static str,
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
    cursor_bg: "",
};

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
    cursor_bg: "\x1B[48;2;24;27;28m",
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
    cursor_bg: "\x1B[48;2;31;20;8m",
};

pub fn palette(kind: ThemeKind) -> &'static Palette {
    match kind {
        ThemeKind::Classic => &CLASSIC_PAL,
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
    fn from_str_accepts_known_names_and_aliases() {
        assert_eq!(ThemeKind::from_str("minimal"), Some(ThemeKind::Minimal));
        assert_eq!(ThemeKind::from_str("HIFI"), Some(ThemeKind::HiFi));
        assert_eq!(ThemeKind::from_str("retro"), Some(ThemeKind::HiFi));
        assert_eq!(ThemeKind::from_str("nope"), None);
    }
}
