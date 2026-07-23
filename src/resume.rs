use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::playlist::keet_config_dir;

#[derive(Serialize, Deserialize)]
pub struct ResumeState {
    pub source_paths: Vec<String>,
    pub track_path: String,
    pub position_secs: f64,
    pub shuffle: bool,
    /// Legacy field: older state files stored a bool before `repeat_mode` was added.
    /// Read-only for back-compat; no longer written. Use `repeat_mode` instead.
    #[serde(default, skip_serializing)]
    pub repeat: bool,
    pub volume: u32,
    pub eq_preset: String,
    pub effects_preset: String,
    #[serde(default)]
    pub repeat_mode: Option<String>,
    #[serde(default)]
    pub rg_mode: Option<String>,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub exclusive: Option<bool>,
    #[serde(default)]
    pub crossfeed_preset: Option<String>,
    #[serde(default)]
    pub balance: Option<i32>,
    #[serde(default)]
    pub theme: Option<String>,
    /// Live 10-band EQ gains, saved when the EQ was edited to Custom.
    #[serde(default)]
    pub eq_gains: Option<Vec<f32>>,
    #[serde(default)]
    pub eq_custom: Option<bool>,
    /// Parametric extension of `eq_gains`: per-band filter type (snake_case
    /// names), frequency (Hz) and Q. Absent in pre-parametric files — those
    /// bands fall back to the graphic defaults on load.
    #[serde(default)]
    pub eq_types: Option<Vec<String>>,
    #[serde(default)]
    pub eq_freqs: Option<Vec<f32>>,
    #[serde(default)]
    pub eq_qs: Option<Vec<f32>>,
    /// Flat pre-filter gain (dB) carried by the active preset (AutoEq preamp).
    #[serde(default)]
    pub eq_preamp: Option<f32>,
}

fn state_file_path() -> Option<PathBuf> {
    keet_config_dir().map(|d| d.join("state.json"))
}

pub fn save_state(state: &ResumeState) {
    if let Some(path) = state_file_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(state) {
            // Write to a sibling temp file then rename, so a crash mid-write
            // can't leave a truncated state.json behind. fs::rename is atomic
            // on the same filesystem on both POSIX and Windows.
            let tmp_path = path.with_extension("json.tmp");
            if std::fs::write(&tmp_path, json).is_ok() {
                let _ = std::fs::rename(&tmp_path, &path);
            }
        }
    }
}

pub fn load_state() -> Option<ResumeState> {
    let path = state_file_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

#[cfg(test)]
mod resume_tests {
    use super::*;

    #[test]
    fn old_state_json_loads_and_parametric_fields_roundtrip() {
        // A pre-parametric state file: gains only, no type/freq/Q keys.
        let old = r#"{
            "source_paths": ["/music"],
            "track_path": "/music/a.flac",
            "position_secs": 1.5,
            "shuffle": false,
            "volume": 90,
            "eq_preset": "Flat",
            "effects_preset": "None",
            "eq_gains": [1.0, -2.0],
            "eq_custom": true
        }"#;
        let mut rs: ResumeState = serde_json::from_str(old).expect("old format must load");
        assert_eq!(rs.eq_types, None);
        assert_eq!(rs.eq_freqs, None);
        assert_eq!(rs.eq_qs, None);
        assert_eq!(rs.eq_preamp, None);

        // The parametric fields survive a save/load roundtrip.
        rs.eq_types = Some(vec!["low_shelf".into(), "peak".into()]);
        rs.eq_freqs = Some(vec![105.0, 3300.0]);
        rs.eq_qs = Some(vec![0.71, 2.0]);
        rs.eq_preamp = Some(-6.4);
        let json = serde_json::to_string(&rs).unwrap();
        let back: ResumeState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.eq_types, Some(vec!["low_shelf".to_string(), "peak".to_string()]));
        assert_eq!(back.eq_freqs, Some(vec![105.0, 3300.0]));
        assert_eq!(back.eq_qs, Some(vec![0.71, 2.0]));
        assert_eq!(back.eq_preamp, Some(-6.4));
        assert_eq!(back.eq_gains, Some(vec![1.0, -2.0]));
    }
}
