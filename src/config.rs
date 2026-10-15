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
    /// oscilloscope | lissajous | polar | spectrogram | analysis | galaxy |
    /// cymatics`.
    #[serde(default)]
    pub viz: Option<String>,
    /// Frames per second while a visualization moves (10–60, default 30).
    /// Still screens draw only when something changes.
    #[serde(default)]
    pub viz_fps: Option<u32>,
    /// The visualization style: `dots` (braille) or `bars`.
    #[serde(default)]
    pub viz_style: Option<String>,
    /// The panels docked into the player at startup: any of `playlist`,
    /// `lyrics`.
    #[serde(default)]
    pub docked: Option<Vec<String>>,
    /// Mouse support (clicks, the wheel); off leaves the terminal's own
    /// text selection alone. Default on; `m` toggles it.
    #[serde(default)]
    pub mouse: Option<bool>,
    /// The terminal's cell size in pixels, `[width, height]`, for Sixel images
    /// (the analysis spectrogram, the cover): replaces Keet's own guess
    /// (`viz::sixel_cell`). Too big makes an image overflow its block.
    #[serde(default)]
    pub sixel_cell: Option<Vec<u32>>,
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
    /// Values taken, but not as written (a frame rate clamped to its
    /// range): each said on its own at startup.
    #[serde(skip)]
    pub adjusted: Vec<String>,
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
/// key (named in `problems`); parsed as one struct, a single bad value would
/// fail the whole file and silently reset every setting. A file
/// that is not JSON at all gives the defaults. Unknown keys are ignored. A
/// broken config never blocks startup.
fn parse(contents: &str) -> Config {
    let mut c = Config::default();
    // Windows editors (Notepad) save UTF-8 with a BOM, which serde rejects:
    // the whole file read as not JSON.
    let contents = strip_bom(contents);
    let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(contents) else {
        c.problems.push("the whole file (not valid JSON)".into());
        return c;
    };
    let mut problems = Vec::new();
    c.theme = field(&obj, "theme", "", &mut problems);
    c.viz = field(&obj, "viz", "", &mut problems);
    c.viz_fps = field(&obj, "viz_fps", "", &mut problems);
    // Clamped where it is applied (`set_viz_fps`); said here, like any other
    // value not taken as written (120 ran at 60 without a word).
    if let Some(fps) = c.viz_fps {
        let (lo, hi) = crate::state::VIZ_FPS_RANGE;
        let used = fps.clamp(lo, hi);
        if used != fps {
            c.adjusted.push(format!("viz_fps {fps} is outside {lo}–{hi}: {used} used"));
        }
    }
    c.mouse = field(&obj, "mouse", "", &mut problems);
    c.sixel_cell = field(&obj, "sixel_cell", "", &mut problems);
    c.viz_style = field(&obj, "viz_style", "", &mut problems);
    c.docked = field(&obj, "docked", "", &mut problems);
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

#[cfg(test)]
pub fn parse_for_tests(contents: &str) -> Config {
    parse(contents)
}

/// `sixel_cell` as a cell size, when it is one: two whole numbers, a width
/// of 4–64 px and a height of 8–128 px. Err names what is wrong.
pub fn sixel_cell(value: Option<&[u32]>) -> Result<Option<(usize, usize)>, String> {
    match value {
        None => Ok(None),
        Some(&[w, h]) if (4..=64).contains(&w) && (8..=128).contains(&h) => Ok(Some((w as usize, h as usize))),
        Some(_) => Err("sixel_cell (want [width, height] in pixels, e.g. [10, 20])".to_string()),
    }
}

/// Write `values` into `config.json` as the new defaults (`Shift+W`), keeping
/// every other key in the file. Refuses a file that is not a JSON object
/// rather than replace what the user wrote; writes the whole file or nothing
/// (`playlist::write_atomic`). Returns the file's path.
pub fn save_defaults(values: Vec<(&str, serde_json::Value)>) -> Result<std::path::PathBuf, String> {
    let dir = crate::playlist::keet_config_dir().ok_or("no config directory")?;
    let path = dir.join("config.json");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("config.json: {e}")),
    };
    let text = merge_defaults(existing.as_deref(), values)?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    crate::playlist::write_atomic(&path, text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(path)
}

/// The new `config.json` text: `existing` (None = no file yet) with `values`
/// set over it, every other key kept, pretty-printed.
fn merge_defaults(existing: Option<&str>, values: Vec<(&str, serde_json::Value)>) -> Result<String, String> {
    let mut obj = match existing {
        None => serde_json::Map::new(),
        Some(text) => match serde_json::from_str::<serde_json::Value>(strip_bom(text)) {
            Ok(serde_json::Value::Object(o)) => o,
            _ => return Err("config.json is not a JSON object: fix or remove it first".into()),
        },
    };
    for (key, value) in values {
        obj.insert(key.to_string(), value);
    }
    let mut text = serde_json::to_string_pretty(&serde_json::Value::Object(obj)).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(text)
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

/// A JSON file Keet reads (presets, state.json, lyrics offsets) as text,
/// without the UTF-8 byte-order mark Windows editors (Notepad) put in front:
/// serde rejects it, and the file read as not JSON at all.
pub(crate) fn read_json_text(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(strip_bom(&text).to_string())
}

/// JSON text without a leading UTF-8 byte order mark (Windows editors add
/// one; serde rejects it). The one copy of the rule: `parse`, the
/// `Shift+W` merge and `read_json_text` each had their own.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Every `*.json` in `~/.config/keet/<subdir>/` (`%APPDATA%\keet\<subdir>\`
/// on Windows) that parses as a preset, sorted by name. The one loader for EQ,
/// effects and crossfeed presets — each kept its own copy, two of them
/// building the config path by hand. Names are shown on screen, so they are
/// sanitised like any other untrusted text (an ESC in one would be executed).
pub fn load_presets<T: serde::de::DeserializeOwned>(subdir: &str, name: fn(&mut T) -> &mut String) -> Vec<T> {
    let Some(dir) = crate::playlist::keet_config_dir().map(|d| d.join(subdir)) else {
        return Vec::new();
    };
    let mut presets: Vec<(String, T)> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")))
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
    fn a_frame_rate_out_of_range_is_named_with_what_is_used() {
        for (fps, used) in [(120, 60), (5, 10)] {
            let c = parse(&format!(r#"{{"viz_fps":{fps}}}"#));
            assert_eq!(c.viz_fps, Some(fps), "still applied, clamped");
            assert!(c.problems.is_empty(), "taken, not ignored: {:?}", c.problems);
            assert_eq!(c.adjusted, [format!("viz_fps {fps} is outside 10–60: {used} used")]);
        }
        assert!(parse(r#"{"viz_fps":45}"#).adjusted.is_empty());
    }

    #[test]
    fn parse_reads_all_defaults_and_they_resolve() {
        let c = parse(
            r#"{"theme":"hifi","viz":"analysis","viz_fps":45,"rg_mode":"album","eq":"Vocal","crossfeed":"medium"}"#,
        );
        assert_eq!(c.viz.as_deref(), Some("analysis"));
        assert_eq!(c.viz_fps, Some(45));
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

    #[test]
    fn saving_defaults_keeps_the_other_keys_and_reads_back() {
        let old = "\u{feff}{\"theme\": \"classic\", \"classic_colors\": {\"highlight\": \"#00ff00\"}, \"future\": 1}";
        let text = merge_defaults(
            Some(old),
            vec![
                ("theme", serde_json::json!("hifi")),
                ("viz_style", serde_json::json!("dots")),
                ("docked", serde_json::json!(["lyrics", "playlist"])),
            ],
        )
        .unwrap();
        let c = parse(&text);
        assert!(c.problems.is_empty(), "{:?}", c.problems);
        assert_eq!(c.theme.as_deref(), Some("hifi"));
        assert_eq!(c.viz_style.as_deref(), Some("dots"));
        assert_eq!(c.docked, Some(vec!["lyrics".to_string(), "playlist".to_string()]));
        assert_eq!(c.classic_colors.highlight.as_deref(), Some("#00ff00"));
        assert!(text.contains("\"future\": 1"));
        // No file yet: just the values.
        assert!(parse(&merge_defaults(None, vec![("eq", serde_json::json!("Vocal"))]).unwrap()).eq.is_some());
        // A file that is not a JSON object is never replaced.
        assert!(merge_defaults(Some("{ not json"), vec![]).is_err());
        assert!(merge_defaults(Some("[1, 2]"), vec![]).is_err());
    }

    #[test]
    fn a_sixel_cell_is_two_plausible_pixel_sizes() {
        assert_eq!(parse(r#"{"sixel_cell": [10, 20]}"#).sixel_cell, Some(vec![10, 20]));
        assert_eq!(sixel_cell(Some(&[10, 20])), Ok(Some((10, 20))));
        assert_eq!(sixel_cell(None), Ok(None));
        for bad in [&[10][..], &[10, 20, 30], &[0, 20], &[10, 500]] {
            assert!(sixel_cell(Some(bad)).is_err(), "{bad:?}");
        }
        // The wrong type costs only this key.
        let c = parse(r#"{"sixel_cell": "10x20", "theme": "hifi"}"#);
        assert_eq!(c.sixel_cell, None);
        assert_eq!(c.theme.as_deref(), Some("hifi"));
    }
}
