//! Persistent user preferences from `~/.config/keet/config.json` (or
//! `%APPDATA%\keet\config.json` on Windows). Distinct from the resume session
//! state in `state.json`: these are defaults that apply on *every* launch. A
//! missing or malformed file falls back to defaults — a broken config never
//! blocks startup.

use serde::Deserialize;

/// User preferences. Every field is optional so a partial `config.json` is
/// valid; absent keys fall back to Keet's built-in defaults / resume state.
#[derive(Deserialize, Default, Debug)]
pub struct Config {
    /// Default UI theme: `"classic" | "minimal" | "hifi"`.
    #[serde(default)]
    pub theme: Option<String>,
    /// Default visualization: `none | vu | spectrum | spectrum-vertical |
    /// oscilloscope | lissajous | spectrogram | analysis`.
    #[serde(default)]
    pub viz: Option<String>,
    /// Default ReplayGain mode: `track | album | off`.
    #[serde(default)]
    pub rg_mode: Option<String>,
    /// Default EQ preset name (built-in or custom).
    #[serde(default)]
    pub eq: Option<String>,
    /// Default crossfeed preset name: `off | light | medium | strong`.
    #[serde(default)]
    pub crossfeed: Option<String>,
    /// Classic in truecolor (`classic_colors`) instead of the terminal's ANSI
    /// green / yellow / red. Off by default: ANSI follows the terminal's own
    /// palette, light backgrounds included.
    #[serde(default)]
    pub classic_use_truecolor: bool,
    #[serde(default)]
    pub classic_colors: ClassicColors,
    /// Keys that were present but unreadable (wrong type), skipped one by
    /// one; shown on the status line at startup.
    #[serde(skip)]
    pub problems: Vec<String>,
}

/// Classic's three truecolor roles as `#RRGGBB`; any left out (or unreadable)
/// keeps its default.
#[derive(Deserialize, Default, Debug)]
pub struct ClassicColors {
    #[serde(default)]
    pub highlight: Option<String>,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Classic's truecolor defaults (the Nocturne mock-ups' mint, amber, coral).
pub const CLASSIC_DEFAULTS: [(u8, u8, u8); 3] = [(125, 211, 184), (233, 182, 92), (240, 122, 120)];

impl ClassicColors {
    /// The three colours, each its configured value or the default, plus the
    /// names of any that were set but could not be read.
    pub fn resolve(&self) -> ([(u8, u8, u8); 3], Vec<&'static str>) {
        let mut out = CLASSIC_DEFAULTS;
        let mut bad = Vec::new();
        let fields = [("highlight", &self.highlight), ("warning", &self.warning), ("error", &self.error)];
        for (i, (name, value)) in fields.into_iter().enumerate() {
            if let Some(v) = value {
                match crate::theme::parse_hex(v) {
                    Some(rgb) => out[i] = rgb,
                    None => bad.push(name),
                }
            }
        }
        (out, bad)
    }
}

// Each field applies on every launch, overriding the resumed last-session value;
// an explicit CLI flag for that setting still wins. See main.rs.

/// Parse config JSON key by key. A value of the wrong type costs only that
/// key (named in `problems`): parsed as one struct, a single bad value failed
/// the whole file and every setting silently went back to its default. A file
/// that is not JSON at all gives the defaults. Unknown keys are ignored. A
/// broken config never blocks startup.
fn parse(contents: &str) -> Config {
    let mut c = Config::default();
    // Windows editors (Notepad) save UTF-8 with a BOM, which serde rejects:
    // the whole file read as not JSON.
    let contents = contents.strip_prefix('\u{feff}').unwrap_or(contents);
    let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(contents) else {
        c.problems.push("the whole file (not valid JSON)".into());
        return c;
    };
    let mut problems = Vec::new();
    c.theme = field(&obj, "theme", "", &mut problems);
    c.viz = field(&obj, "viz", "", &mut problems);
    c.rg_mode = field(&obj, "rg_mode", "", &mut problems);
    c.eq = field(&obj, "eq", "", &mut problems);
    c.crossfeed = field(&obj, "crossfeed", "", &mut problems);
    c.classic_use_truecolor = field(&obj, "classic_use_truecolor", "", &mut problems).unwrap_or(false);
    match obj.get("classic_colors") {
        Some(serde_json::Value::Object(colors)) => {
            c.classic_colors = ClassicColors {
                highlight: field(colors, "highlight", "classic_colors.", &mut problems),
                warning: field(colors, "warning", "classic_colors.", &mut problems),
                error: field(colors, "error", "classic_colors.", &mut problems),
            };
        }
        Some(_) => problems.push("classic_colors (wrong type)".into()),
        None => {}
    }
    c.problems = problems;
    c
}

/// One key of an object: its value, or None when absent; a value of the wrong
/// type is None too, and its name (with `prefix`) goes in `problems`.
fn field<T: serde::de::DeserializeOwned>(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    prefix: &str,
    problems: &mut Vec<String>,
) -> Option<T> {
    let v = obj.get(key)?;
    match serde_json::from_value(v.clone()) {
        Ok(t) => Some(t),
        Err(_) => {
            problems.push(format!("{prefix}{key} (wrong type)"));
            None
        }
    }
}

/// Load `config.json` from the keet config dir. Returns defaults if the file is
/// missing or can't be parsed.
pub fn load() -> Config {
    let Some(path) = crate::playlist::keet_config_dir().map(|d| d.join("config.json")) else {
        return Config::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => parse(&s),
        Err(_) => Config::default(),
    }
}

/// Every `*.json` in `~/.config/keet/<subdir>/` (`%APPDATA%\keet\<subdir>\`
/// on Windows) that parses as a preset, sorted by name. The one loader for EQ,
/// effects and crossfeed presets — each kept its own copy, two of them
/// building the config path by hand. Names are shown on screen, so they are
/// sanitised like any other untrusted text (an ESC in one would be executed).
/// A JSON file Keet reads (presets, state.json, lyrics offsets) as text,
/// without the UTF-8 byte-order mark Windows editors (Notepad) put in front:
/// serde rejects it, and the file read as not JSON at all.
pub(crate) fn read_json_text(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_string(),
        None => text,
    })
}

pub fn load_presets<T: serde::de::DeserializeOwned>(subdir: &str, name: fn(&mut T) -> &mut String) -> Vec<T> {
    let Some(dir) = crate::playlist::keet_config_dir().map(|d| d.join(subdir)) else {
        return Vec::new();
    };
    let mut presets: Vec<(String, T)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| read_json_text(&p))
        .filter_map(|text| serde_json::from_str::<T>(&text).ok())
        .map(|mut p| {
            let n = name(&mut p);
            *n = crate::ansi::sanitize_display(n);
            (n.clone(), p)
        })
        .collect();
    presets.sort_by(|a, b| a.0.cmp(&b.0));
    presets.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_files_load_with_a_byte_order_mark() {
        let dir = std::env::temp_dir().join(format!("keet-bom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("p.json");
        std::fs::write(&f, "\u{feff}{\"name\": \"x\"}").unwrap();
        let text = read_json_text(&f).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok(), "{text:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_reads_theme_and_tolerates_junk() {
        assert_eq!(parse(r#"{"theme": "minimal"}"#).theme.as_deref(), Some("minimal"));
        // Missing theme key → None (falls back to resume/default downstream).
        assert_eq!(parse(r#"{}"#).theme, None);
        // Unknown keys ignored.
        assert_eq!(parse(r#"{"theme": "hifi", "future": 1}"#).theme.as_deref(), Some("hifi"));
        // Malformed JSON → defaults, never a panic.
        assert_eq!(parse("not json").theme, None);
    }

    #[test]
    fn one_wrongly_typed_value_costs_only_itself() {
        // serde rejected the whole file over one bad value, and every setting
        // silently went back to its default.
        let c = parse(r#"{"theme": "minimal", "classic_use_truecolor": "yes", "rg_mode": 3, "eq": "Vocal"}"#);
        assert_eq!(c.theme.as_deref(), Some("minimal"));
        assert_eq!(c.eq.as_deref(), Some("Vocal"));
        assert!(!c.classic_use_truecolor);
        assert_eq!(c.rg_mode, None);
        assert_eq!(c.problems, ["rg_mode (wrong type)", "classic_use_truecolor (wrong type)"]);
        let c = parse(r##"{"classic_colors": {"highlight": 7, "error": "#F07A78"}}"##);
        assert_eq!(c.classic_colors.error.as_deref(), Some("#F07A78"));
        assert_eq!(c.problems, ["classic_colors.highlight (wrong type)"]);
        assert_eq!(parse("not json").problems, ["the whole file (not valid JSON)"]);
        assert!(parse(r#"{"future": 1}"#).problems.is_empty(), "unknown keys are fine");
        let bom = parse("\u{feff}{\"theme\": \"hifi\"}");
        assert_eq!((bom.theme.as_deref(), bom.problems.len()), (Some("hifi"), 0), "a BOM is not an error");
    }

    #[test]
    fn classic_truecolor_is_off_unless_asked_and_colours_fall_back_one_by_one() {
        let c = parse(r#"{}"#);
        assert!(!c.classic_use_truecolor);
        assert_eq!(c.classic_colors.resolve(), (CLASSIC_DEFAULTS, vec![]));

        let c = parse(r##"{"classic_use_truecolor": true, "classic_colors": {"highlight": "#7FB2F0", "error": "red"}}"##);
        assert!(c.classic_use_truecolor);
        let (rgb, bad) = c.classic_colors.resolve();
        assert_eq!(rgb, [(127, 178, 240), CLASSIC_DEFAULTS[1], CLASSIC_DEFAULTS[2]]);
        assert_eq!(bad, ["error"], "an unreadable colour is reported and keeps its default");
    }

    #[test]
    fn parse_reads_all_defaults_and_they_resolve() {
        let c = parse(
            r#"{"theme":"hifi","viz":"analysis","rg_mode":"album","eq":"Vocal","crossfeed":"medium"}"#,
        );
        assert_eq!(c.viz.as_deref(), Some("analysis"));
        assert_eq!(c.rg_mode.as_deref(), Some("album"));
        assert_eq!(c.eq.as_deref(), Some("Vocal"));
        assert_eq!(c.crossfeed.as_deref(), Some("medium"));
        // The enum-backed ones resolve.
        assert!(matches!(
            crate::state::VizMode::from_str("analysis"),
            Some(crate::state::VizMode::SpectrogramAnalysis)
        ));
        assert!(matches!(
            crate::state::RgMode::from_str("album"),
            Some(crate::state::RgMode::Album)
        ));
        assert!(crate::state::VizMode::from_str("nope").is_none());
    }
}
