use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::playlist::keet_config_dir;

#[derive(Serialize, Deserialize)]
pub struct ResumeState {
    pub source_paths: Vec<String>,
    pub track_path: String,
    pub position_secs: f64,
    pub shuffle: bool,
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
            let _ = std::fs::write(&path, json);
        }
    }
}

pub fn load_state() -> Option<ResumeState> {
    let path = state_file_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}
