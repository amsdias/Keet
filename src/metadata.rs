use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Value};
use symphonia::core::probe::Hint;

struct CachedMeta {
    display: String,
    search_text: String,
    #[allow(dead_code)]
    artist: Option<String>,
    #[allow(dead_code)]
    title: Option<String>,
    #[allow(dead_code)]
    rg_track_gain: Option<f32>,
    #[allow(dead_code)]
    rg_track_peak: Option<f32>,
    #[allow(dead_code)]
    rg_album_gain: Option<f32>,
    #[allow(dead_code)]
    rg_album_peak: Option<f32>,
}

pub struct MetadataCache {
    entries: Mutex<Vec<Option<CachedMeta>>>,
    pub cancel: AtomicBool,
}

impl MetadataCache {
    pub fn new(len: usize) -> Arc<Self> {
        let entries: Vec<Option<CachedMeta>> = (0..len).map(|_| None).collect();
        Arc::new(Self {
            entries: Mutex::new(entries),
            cancel: AtomicBool::new(false),
        })
    }

    pub fn display_name(&self, index: usize, path: &Path) -> String {
        let entries = self.entries.lock().unwrap();
        if let Some(Some(meta)) = entries.get(index) {
            meta.display.clone()
        } else {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }
    }

    pub fn search_matches(&self, index: usize, path: &Path, query: &str) -> bool {
        if query.is_empty() {
            return false;
        }
        let entries = self.entries.lock().unwrap();
        if let Some(Some(meta)) = entries.get(index) {
            meta.search_text.contains(query)
        } else {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
                .contains(query)
        }
    }

    fn set(&self, index: usize, meta: CachedMeta) {
        let mut entries = self.entries.lock().unwrap();
        if index < entries.len() {
            entries[index] = Some(meta);
        }
    }

    pub fn reindex(&self, new_playlist: &[PathBuf], old_playlist: &[PathBuf]) {
        let mut entries = self.entries.lock().unwrap();
        let mut map: HashMap<PathBuf, CachedMeta> = HashMap::new();
        for (i, path) in old_playlist.iter().enumerate() {
            if let Some(meta) = entries.get_mut(i).and_then(|e| e.take()) {
                map.insert(path.clone(), meta);
            }
        }
        let new_entries: Vec<Option<CachedMeta>> = new_playlist
            .iter()
            .map(|p| map.remove(p))
            .collect();
        *entries = new_entries;
    }

    pub fn remove_at(&self, index: usize) {
        let mut entries = self.entries.lock().unwrap();
        if index < entries.len() {
            entries.remove(index);
        }
    }

    pub fn is_set(&self, index: usize) -> bool {
        let entries = self.entries.lock().unwrap();
        entries.get(index).map(|e| e.is_some()).unwrap_or(false)
    }
}

/// Parse a ReplayGain gain string like "-7.2 dB" or "-7.2" into an f32 dB value.
pub fn parse_rg_gain_value(s: &str) -> Option<f32> {
    let s = s.trim();
    let s = s.strip_suffix(" dB")
        .or_else(|| s.strip_suffix(" db"))
        .or_else(|| s.strip_suffix("dB"))
        .or_else(|| s.strip_suffix("db"))
        .unwrap_or(s);
    s.trim().parse::<f32>().ok()
}

fn parse_rg_peak_value(s: &str) -> Option<f32> {
    s.trim().parse::<f32>().ok()
}

fn read_metadata_full(path: &Path) -> Option<CachedMeta> {
    let file = File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension() {
        hint.with_extension(ext.to_str().unwrap_or(""));
    }
    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?;

    let mut title: Option<String> = None;
    let mut artist: Option<String> = None;
    let mut rg_track_gain: Option<f32> = None;
    let mut rg_track_peak: Option<f32> = None;
    let mut rg_album_gain: Option<f32> = None;
    let mut rg_album_peak: Option<f32> = None;

    if let Some(rev) = probed.format.metadata().current() {
        for tag in rev.tags() {
            match tag.std_key {
                Some(StandardTagKey::TrackTitle) => {
                    if let Value::String(ref s) = tag.value { title = Some(s.clone()); }
                }
                Some(StandardTagKey::Artist) => {
                    if let Value::String(ref s) = tag.value { artist = Some(s.clone()); }
                }
                _ => {}
            }
            let key_lower = tag.key.to_lowercase();
            if let Value::String(ref s) = tag.value {
                match key_lower.as_str() {
                    "replaygain_track_gain" if rg_track_gain.is_none() => {
                        rg_track_gain = parse_rg_gain_value(s);
                    }
                    "replaygain_track_peak" if rg_track_peak.is_none() => {
                        rg_track_peak = parse_rg_peak_value(s);
                    }
                    "replaygain_album_gain" if rg_album_gain.is_none() => {
                        rg_album_gain = parse_rg_gain_value(s);
                    }
                    "replaygain_album_peak" if rg_album_peak.is_none() => {
                        rg_album_peak = parse_rg_peak_value(s);
                    }
                    _ => {}
                }
            }
        }
    }

    if title.is_none() || artist.is_none() {
        if let Some(meta) = probed.metadata.get() {
            if let Some(rev) = meta.current() {
                for tag in rev.tags() {
                    match tag.std_key {
                        Some(StandardTagKey::TrackTitle) if title.is_none() => {
                            if let Value::String(ref s) = tag.value { title = Some(s.clone()); }
                        }
                        Some(StandardTagKey::Artist) if artist.is_none() => {
                            if let Value::String(ref s) = tag.value { artist = Some(s.clone()); }
                        }
                        _ => {}
                    }
                    let key_lower = tag.key.to_lowercase();
                    if let Value::String(ref s) = tag.value {
                        match key_lower.as_str() {
                            "replaygain_track_gain" if rg_track_gain.is_none() => {
                                rg_track_gain = parse_rg_gain_value(s);
                            }
                            "replaygain_track_peak" if rg_track_peak.is_none() => {
                                rg_track_peak = parse_rg_peak_value(s);
                            }
                            "replaygain_album_gain" if rg_album_gain.is_none() => {
                                rg_album_gain = parse_rg_gain_value(s);
                            }
                            "replaygain_album_peak" if rg_album_peak.is_none() => {
                                rg_album_peak = parse_rg_peak_value(s);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    let display = match (&artist, &title) {
        (Some(a), Some(t)) => format!("{} - {}", a, t),
        (None, Some(t)) => t.clone(),
        (Some(a), None) => a.clone(),
        (None, None) => return None,
    };

    let filename = path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    let search_text = format!("{}\0{}", display.to_lowercase(), filename);

    Some(CachedMeta {
        display,
        search_text,
        artist,
        title,
        rg_track_gain,
        rg_track_peak,
        rg_album_gain,
        rg_album_peak,
    })
}

pub fn read_metadata_display(path: &Path) -> Option<String> {
    read_metadata_full(path).map(|m| m.display)
}

pub fn spawn_metadata_scan(
    playlist: Vec<PathBuf>,
    cache: Arc<MetadataCache>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        for (i, path) in playlist.iter().enumerate() {
            if cache.cancel.load(Ordering::Relaxed) {
                break;
            }
            if cache.is_set(i) {
                continue;
            }
            if let Some(meta) = read_metadata_full(path) {
                cache.set(i, meta);
            }
        }
    })
}
