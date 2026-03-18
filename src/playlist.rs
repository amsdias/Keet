use std::fs::{self, File};
use std::path::{Path, PathBuf};

use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Value};
use symphonia::core::probe::Hint;

use crate::state::SUPPORTED_EXTENSIONS;

/// Extract artist and title from audio file metadata.
/// Returns "Artist - Title" if both found, just title if only title, or None.
pub fn read_metadata(path: &Path) -> Option<String> {
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

    // Check metadata from the container format
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
        }
    }

    // Also check metadata from the probe result (e.g. ID3 tags before the container)
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
                }
            }
        }
    }

    match (artist, title) {
        (Some(a), Some(t)) => Some(format!("{} - {}", a, t)),
        (None, Some(t)) => Some(t),
        _ => None,
    }
}

pub fn shuffle_list(list: &mut [PathBuf]) {
    let mut rng = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(12345);
    for i in (1..list.len()).rev() {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        list.swap(i, rng as usize % (i + 1));
    }
}

pub fn build_playlist(path: &Path, shuffle: bool) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut list = Vec::new();

    if path.is_file() {
        list.push(path.to_path_buf());
    } else if path.is_dir() {
        fn scan_dir(dir: &Path, list: &mut Vec<PathBuf>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        scan_dir(&p, list);
                    } else if p.is_file() {
                        if let Some(ext) = p.extension() {
                            if SUPPORTED_EXTENSIONS.contains(&ext.to_string_lossy().to_lowercase().as_str()) {
                                list.push(p);
                            }
                        }
                    }
                }
            }
        }
        scan_dir(path, &mut list);
        list.sort();

        if shuffle {
            shuffle_list(&mut list);
        }
    }

    if list.is_empty() {
        return Err("No audio files found".into());
    }
    Ok(list)
}
