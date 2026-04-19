use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};

use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Value};
use symphonia::core::probe::Hint;

#[derive(Clone)]
struct CachedMeta {
    display: String,
    search_text: String,
    #[allow(dead_code)]
    artist: Option<String>,
    #[allow(dead_code)]
    title: Option<String>,
    #[allow(dead_code)]
    album: Option<String>,
    #[allow(dead_code)]
    rg_track_gain: Option<f32>,
    #[allow(dead_code)]
    rg_track_peak: Option<f32>,
    #[allow(dead_code)]
    rg_album_gain: Option<f32>,
    #[allow(dead_code)]
    rg_album_peak: Option<f32>,
    lyrics: Option<String>,
    duration_secs: Option<f64>,
}

pub struct MetadataCache {
    // RwLock: many concurrent readers (UI playlist render) vs rare writers
    // (background scan threads). Mutex caused UI stutter on large libraries.
    entries: RwLock<Vec<Option<CachedMeta>>>,
    pub cancel: AtomicBool,
}

impl MetadataCache {
    pub fn new(len: usize) -> Arc<Self> {
        let entries: Vec<Option<CachedMeta>> = (0..len).map(|_| None).collect();
        Arc::new(Self {
            entries: RwLock::new(entries),
            cancel: AtomicBool::new(false),
        })
    }

    pub fn display_name(&self, index: usize, path: &Path) -> String {
        let entries = self.entries.read().unwrap();
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
        let entries = self.entries.read().unwrap();
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

    pub fn lyrics(&self, index: usize) -> Option<String> {
        let entries = self.entries.read().unwrap();
        entries.get(index).and_then(|e| e.as_ref()).and_then(|m| m.lyrics.clone())
    }

    pub fn artist_title(&self, index: usize) -> (Option<String>, Option<String>) {
        let entries = self.entries.read().unwrap();
        if let Some(Some(meta)) = entries.get(index) {
            (meta.artist.clone(), meta.title.clone())
        } else {
            (None, None)
        }
    }

    pub fn artist_album(&self, index: usize) -> (Option<String>, Option<String>) {
        let entries = self.entries.read().unwrap();
        if let Some(Some(meta)) = entries.get(index) {
            (meta.artist.clone(), meta.album.clone())
        } else {
            (None, None)
        }
    }

    pub fn album(&self, index: usize) -> Option<String> {
        let entries = self.entries.read().unwrap();
        entries.get(index).and_then(|e| e.as_ref()).and_then(|m| m.album.clone())
    }

    fn set(&self, index: usize, meta: CachedMeta) {
        let mut entries = self.entries.write().unwrap();
        if index < entries.len() {
            entries[index] = Some(meta);
        }
    }

    pub fn reindex(&self, new_playlist: &[PathBuf], old_playlist: &[PathBuf]) {
        let mut entries = self.entries.write().unwrap();
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
        let mut entries = self.entries.write().unwrap();
        if index < entries.len() {
            entries.remove(index);
        }
    }

    pub fn duration(&self, index: usize) -> Option<f64> {
        let entries = self.entries.read().unwrap();
        entries.get(index).and_then(|e| e.as_ref()).and_then(|m| m.duration_secs)
    }

    pub fn move_entry(&self, from: usize, to: usize) {
        let mut entries = self.entries.write().unwrap();
        if from >= entries.len() { return; }
        let meta = entries.remove(from);
        let dst = to.min(entries.len());
        entries.insert(dst, meta);
    }

    pub fn is_set(&self, index: usize) -> bool {
        let entries = self.entries.read().unwrap();
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

    let duration_secs: Option<f64> = probed.format.tracks().iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .and_then(|t| {
            if let (Some(n_frames), Some(rate)) = (t.codec_params.n_frames, t.codec_params.sample_rate) {
                if rate > 0 { return Some(n_frames as f64 / rate as f64); }
            }
            if let (Some(tb), Some(n_frames)) = (t.codec_params.time_base, t.codec_params.n_frames) {
                return Some(n_frames as f64 * tb.numer as f64 / tb.denom as f64);
            }
            None
        });

    let mut title: Option<String> = None;
    let mut artist: Option<String> = None;
    let mut album: Option<String> = None;
    let mut lyrics: Option<String> = None;
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
                Some(StandardTagKey::Album) => {
                    if let Value::String(ref s) = tag.value { album = Some(s.clone()); }
                }
                Some(StandardTagKey::Lyrics) if lyrics.is_none() => {
                    if let Value::String(ref s) = tag.value { lyrics = Some(s.clone()); }
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
                    "lyrics" | "unsyncedlyrics" if lyrics.is_none() => {
                        lyrics = Some(s.clone());
                    }
                    _ => {}
                }
            }
        }
    }

    if title.is_none() || artist.is_none() || album.is_none() || lyrics.is_none() {
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
                        Some(StandardTagKey::Album) if album.is_none() => {
                            if let Value::String(ref s) = tag.value { album = Some(s.clone()); }
                        }
                        Some(StandardTagKey::Lyrics) if lyrics.is_none() => {
                            if let Value::String(ref s) = tag.value { lyrics = Some(s.clone()); }
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
                            "lyrics" | "unsyncedlyrics" if lyrics.is_none() => {
                                lyrics = Some(s.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    let filename = path.file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let display = match (&artist, &title) {
        (Some(a), Some(t)) => format!("{} - {}", a, t),
        (None, Some(t)) => t.clone(),
        (Some(a), None) => a.clone(),
        (None, None) => filename.to_string(),
    };

    let search_text = format!("{}\0{}", display.to_lowercase(), filename.to_lowercase());

    Some(CachedMeta {
        display,
        search_text,
        artist,
        title,
        album,
        rg_track_gain,
        rg_track_peak,
        rg_album_gain,
        rg_album_peak,
        lyrics,
        duration_secs,
    })
}

/// Read only embedded lyrics from a file (for tracks not yet in the metadata cache).
pub fn read_lyrics(path: &Path) -> Option<String> {
    read_metadata_full(path).and_then(|m| m.lyrics)
}

/// Read (artist, title, embedded lyrics) from a file in a single pass — used as
/// fallback when the background metadata scan hasn't reached this track yet.
pub fn read_artist_title_lyrics(path: &Path) -> (Option<String>, Option<String>, Option<String>) {
    match read_metadata_full(path) {
        Some(m) => (m.artist, m.title, m.lyrics),
        None => (None, None, None),
    }
}

pub fn spawn_metadata_scan(
    playlist: Vec<PathBuf>,
    cache: Arc<MetadataCache>,
) -> JoinHandle<()> {
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(2);
    let shared_playlist = Arc::new(playlist);
    let next_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut handles = Vec::with_capacity(num_threads);

    for _ in 0..num_threads {
        let cache = Arc::clone(&cache);
        let playlist = Arc::clone(&shared_playlist);
        let next_index = Arc::clone(&next_index);

        let handle = thread::spawn(move || {
            loop {
                if cache.cancel.load(Ordering::Relaxed) {
                    break;
                }

                let i = next_index.fetch_add(1, Ordering::Relaxed);
                if i >= playlist.len() {
                    break;
                }

                if cache.is_set(i) {
                    continue;
                }

                if let Some(meta) = read_metadata_full(&playlist[i]) {
                    cache.set(i, meta);
                }
            }
        });
        handles.push(handle);
    }

    thread::spawn(move || {
        for handle in handles {
            let _ = handle.join();
        }
    })
}
