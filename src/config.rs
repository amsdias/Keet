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
    /// Default UI theme: `"classic" | "minimal" | "hifi"`. Applied on every
    /// launch, overriding the resumed last-session theme; `--theme` still wins.
    #[serde(default)]
    pub theme: Option<String>,
}

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
}
