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
}

// Each field applies on every launch, overriding the resumed last-session value;
// an explicit CLI flag for that setting still wins. See main.rs.

/// Parse config JSON, falling back to defaults on any error (malformed JSON, a
/// broken config never blocks startup). Unknown keys are ignored.
fn parse(contents: &str) -> Config {
    serde_json::from_str(contents).unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;

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
