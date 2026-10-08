use std::fs;
use std::path::{Path, PathBuf};

use crate::state::SUPPORTED_EXTENSIONS;

pub fn shuffle_list(list: &mut [PathBuf]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    // A monotonic counter ensures two shuffles within the same clock tick still
    // get distinct seeds (wall-clock nanos alone isn't enough when called back-to-back).
    static SEED_COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15);
    let counter = SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
    // SplitMix64 finalizer — scrambles correlated inputs into uncorrelated 64-bit states.
    let mut seed = nanos ^ counter.wrapping_mul(0x9E3779B97F4A7C15);
    seed = (seed ^ (seed >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    seed = (seed ^ (seed >> 27)).wrapping_mul(0x94D049BB133111EB);
    seed ^= seed >> 31;

    let mut rng = seed;
    for i in (1..list.len()).rev() {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        list.swap(i, (rng >> 32) as usize % (i + 1));
    }
}

pub fn build_playlist(path: &Path, shuffle: bool) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    // Check for M3U playlist file
    if let Some(ext) = path.extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        if ext_lower == "m3u" || ext_lower == "m3u8" {
            let mut list = parse_m3u(path)?;
            if shuffle {
                shuffle_list(&mut list);
            }
            return Ok(list);
        }
    }

    let mut list = Vec::new();

    if path.is_file() {
        list.push(path.to_path_buf());
    } else if path.is_dir() {
        // Track visited directories by canonical path so a symlink pointing back up
        // the tree can't drive scan_dir into unbounded (stack-overflowing) recursion.
        fn scan_dir(dir: &Path, list: &mut Vec<PathBuf>, visited: &mut std::collections::HashSet<PathBuf>) {
            let canonical = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            if !visited.insert(canonical) {
                return;
            }
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        scan_dir(&p, list, visited);
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
        let mut visited = std::collections::HashSet::new();
        scan_dir(path, &mut list, &mut visited);
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

/// Returns the platform-aware Keet config directory.
pub fn keet_config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        std::env::var("APPDATA").ok().map(|p| PathBuf::from(p).join("keet"))
    } else {
        std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config").join("keet"))
    }
}

/// Parse an M3U/M3U8 playlist file into a list of audio file paths.
pub fn parse_m3u(path: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    // Bytes, not a String: plain .m3u files are often Windows-1252/Latin-1,
    // and reading them as UTF-8 failed the whole playlist.
    let bytes = fs::read(path)?;
    // Windows editors commonly prepend a UTF-8 BOM, which would otherwise glue
    // itself to the first entry's path and make it unresolvable.
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut list = Vec::new();

    for line in bytes.split(|&b| b == b'\n') {
        let line = line.trim_ascii();
        if line.is_empty() || line.starts_with(b"#") {
            continue;
        }
        let resolve = |entry: PathBuf| if entry.is_absolute() { entry } else { parent.join(entry) };
        let Some(track_path) = m3u_entry_candidates(line).into_iter().map(resolve).find(|p| p.is_file()) else {
            continue;
        };
        if let Some(ext) = track_path.extension() {
            if SUPPORTED_EXTENSIONS.contains(&ext.to_string_lossy().to_lowercase().as_str()) {
                list.push(track_path);
            }
        }
    }

    if list.is_empty() {
        return Err("No audio files found in playlist".into());
    }
    Ok(list)
}

/// The paths an M3U line may name, best guess first: the line as UTF-8; or,
/// when it is not valid UTF-8, its raw bytes as the path (Unix stores names
/// as bytes, so this is exact for a playlist written on the same system) and
/// then the line read as Latin-1 (what older Windows tools write).
fn m3u_entry_candidates(line: &[u8]) -> Vec<PathBuf> {
    if let Ok(s) = std::str::from_utf8(line) {
        return vec![PathBuf::from(s)];
    }
    let latin1 = PathBuf::from(line.iter().map(|&b| b as char).collect::<String>());
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        vec![PathBuf::from(std::ffi::OsStr::from_bytes(line)), latin1]
    }
    #[cfg(not(unix))]
    vec![latin1]
}

/// Save a playlist as an M3U file.
/// If `name` contains a path separator, treat it as a full path.
/// Otherwise, save to ~/.config/keet/playlists/<name>.m3u.
pub fn save_m3u(playlist: &[PathBuf], name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = if name.contains('/') || name.contains('\\') {
        let p = PathBuf::from(name);
        if !p.to_string_lossy().ends_with(".m3u") && !p.to_string_lossy().ends_with(".m3u8") {
            p.with_extension("m3u")
        } else {
            p
        }
    } else {
        let dir = keet_config_dir()
            .ok_or("Could not determine config directory")?
            .join("playlists");
        fs::create_dir_all(&dir)?;
        let filename = if name.ends_with(".m3u") || name.ends_with(".m3u8") {
            name.to_string()
        } else {
            format!("{}.m3u", name)
        };
        dir.join(&filename)
    };

    // Ensure parent directory exists for arbitrary paths
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut content: Vec<u8> = b"#EXTM3U\n".to_vec();
    for track in playlist {
        // Unix paths are bytes: written as they are, a name that is not valid
        // UTF-8 still resolves when the playlist is read back.
        #[cfg(unix)]
        content.extend_from_slice(std::os::unix::ffi::OsStrExt::as_bytes(track.as_os_str()));
        #[cfg(not(unix))]
        content.extend_from_slice(track.to_string_lossy().as_bytes());
        content.push(b'\n');
    }
    // A temporary file beside the target, renamed over it: writing in place
    // left a truncated playlist if the write failed half-way (a full disk, a
    // network share dropping out).
    let tmp = path.with_extension("m3u.keet-tmp");
    fs::write(&tmp, &content)?;
    if let Err(e) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(path)
}

/// The playlist for the next repeat-all cycle (before any shuffle). With a
/// folder among the sources, every source is read again so new files join the
/// cycle; otherwise (M3U or single files) the current list is kept. Either way
/// tracks the user removed stay out (`removed` holds canonical paths) and
/// duplicates collapse. All of it is I/O — directory walks and one path lookup
/// per track — so it runs off the UI thread; on a big or network library it
/// froze the UI at every wrap.
pub fn next_cycle(
    sources: &[PathBuf],
    current: &[PathBuf],
    removed: &std::collections::HashSet<PathBuf>,
) -> Vec<PathBuf> {
    let mut list: Vec<PathBuf> = if sources.iter().any(|p| p.is_dir()) {
        let combined: Vec<PathBuf> = sources
            .iter()
            .filter_map(|src| build_playlist(src, false).ok())
            .flatten()
            .collect();
        // Nothing readable this time (a share gone offline): keep playing
        // what there was rather than ending.
        if combined.is_empty() { current.to_vec() } else { combined }
    } else {
        current.to_vec()
    };
    let mut seen = std::collections::HashSet::new();
    list.retain(|p| {
        let key = fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        !removed.contains(&key) && seen.insert(key)
    });
    list
}

/// What a rescan found on disk. Built off the UI thread by [`scan_sources`]
/// (directory walks and path canonicalisation can take seconds on a large or
/// network library) and applied on it by [`apply_rescan`].
pub struct RescanResult {
    /// Every track the sources hold now, in source order.
    pub fresh: Vec<PathBuf>,
    /// Canonical form of each path seen (fresh tracks and the playlist
    /// snapshot), so applying needs no filesystem access.
    pub canon: std::collections::HashMap<PathBuf, PathBuf>,
    /// A source could not be read: its tracks are then unknown, not gone.
    pub had_error: bool,
}

/// Read every source (folder or M3U) and canonicalise the paths. Pure I/O;
/// `snapshot` is the playlist at the time, canonicalised here too.
pub fn scan_sources(sources: &[PathBuf], snapshot: &[PathBuf]) -> RescanResult {
    let mut fresh = Vec::new();
    let mut had_error = false;
    for src in sources {
        match build_playlist(src, false) {
            Ok(list) => fresh.extend(list),
            Err(_) => had_error = true,
        }
    }
    let canon = fresh
        .iter()
        .chain(snapshot)
        .map(|p| (p.clone(), fs::canonicalize(p).unwrap_or_else(|_| p.clone())))
        .collect();
    RescanResult { fresh, canon, had_error }
}

/// Diff the playlist against what is on disk, across ALL sources at once.
/// Tracks no longer found are dropped (except the one playing), new ones are
/// appended in source order, duplicates collapse, and tracks the user removed
/// (`removed`, canonical paths) are never brought back. Each source used to be
/// diffed on its own, so with two folders the second pass dropped everything
/// the first had kept. When a source could not be read nothing is dropped:
/// its tracks are unknown, not gone. Returns (added, removed).
pub fn apply_rescan(
    playlist: &mut Vec<PathBuf>,
    r: &RescanResult,
    current_track_path: Option<&Path>,
    removed: &std::collections::HashSet<PathBuf>,
) -> (usize, usize) {
    use std::collections::HashSet;
    let key = |p: &PathBuf| r.canon.get(p).cloned().unwrap_or_else(|| p.clone());
    let fresh_keys: HashSet<PathBuf> = r.fresh.iter().map(key).filter(|k| !removed.contains(k)).collect();

    let before = playlist.len();
    if !r.had_error {
        playlist.retain(|p| fresh_keys.contains(&key(p)) || current_track_path == Some(p.as_path()));
    }
    let dropped = before - playlist.len();

    let mut have: HashSet<PathBuf> = playlist.iter().map(key).collect();
    let mut added = 0;
    for f in &r.fresh {
        let k = key(f);
        if !removed.contains(&k) && have.insert(k) {
            playlist.push(f.clone());
            added += 1;
        }
    }
    // Collapse duplicates already in the list (the same file reached through
    // two sources or a symlink), keeping the first.
    let mut seen = HashSet::new();
    playlist.retain(|p| seen.insert(key(p)));
    (added, dropped)
}

#[cfg(test)]
mod m3u_tests {
    use super::*;

    fn tmp_lib(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keet_rescan_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for f in files {
            fs::write(dir.join(f), b"").unwrap();
        }
        dir
    }

    #[test]
    fn the_next_repeat_cycle_rereads_folders_without_removed_tracks() {
        let a = tmp_lib("cycle_a", &["a1.flac", "a2.flac"]);
        fs::write(a.join("a3.flac"), b"").unwrap(); // added since the last cycle
        let current = vec![a.join("a1.flac"), a.join("a2.flac")];
        let removed: std::collections::HashSet<PathBuf> =
            [fs::canonicalize(a.join("a2.flac")).unwrap()].into_iter().collect();
        let next = next_cycle(std::slice::from_ref(&a), &current, &removed);
        let _ = fs::remove_dir_all(&a);
        assert_eq!(next, vec![a.join("a1.flac"), a.join("a3.flac")]);
    }

    #[test]
    fn the_next_repeat_cycle_of_a_playlist_file_keeps_its_order_minus_removals() {
        // Not a folder: nothing to rescan, the list stays as it was.
        let current = vec![PathBuf::from("/m/x.flac"), PathBuf::from("/m/y.flac")];
        let removed: std::collections::HashSet<PathBuf> = [PathBuf::from("/m/x.flac")].into_iter().collect();
        let next = next_cycle(&[PathBuf::from("/m/list.m3u")], &current, &removed);
        assert_eq!(next, vec![PathBuf::from("/m/y.flac")]);
    }

    #[test]
    fn rescan_keeps_every_source_folder() {
        // Each source used to be diffed on its own: the pass for ~/A removed
        // every track not under ~/A, then the pass for ~/B removed all of A's.
        let a = tmp_lib("multi_a", &["a1.flac", "a2.flac"]);
        let b = tmp_lib("multi_b", &["b1.flac"]);
        let sources = vec![a.clone(), b.clone()];
        let mut playlist = vec![a.join("a1.flac"), a.join("a2.flac"), b.join("b1.flac")];
        fs::remove_file(a.join("a2.flac")).unwrap();
        fs::write(b.join("b2.flac"), b"").unwrap();

        let r = scan_sources(&sources, &playlist);
        let (added, removed) =
            apply_rescan(&mut playlist, &r, None, &std::collections::HashSet::new());
        let _ = (fs::remove_dir_all(&a), fs::remove_dir_all(&b));
        assert_eq!((added, removed), (1, 1));
        assert_eq!(playlist, vec![a.join("a1.flac"), b.join("b1.flac"), b.join("b2.flac")]);
    }

    #[test]
    fn rescan_does_not_bring_back_removed_tracks_or_drop_an_unreadable_source() {
        let a = tmp_lib("removed_a", &["a1.flac", "a2.flac"]);
        let gone = a.join("no-such-folder");
        let mut playlist = vec![a.join("a1.flac"), PathBuf::from("/elsewhere/x.flac")];
        let r = scan_sources(&[a.clone(), gone], &playlist);
        assert!(r.had_error);
        // a2 was removed by the user: it must stay out.
        let removed: std::collections::HashSet<PathBuf> =
            [fs::canonicalize(a.join("a2.flac")).unwrap()].into_iter().collect();
        let (added, dropped) = apply_rescan(&mut playlist, &r, None, &removed);
        let _ = fs::remove_dir_all(&a);
        // A source that failed to read says nothing about its tracks, so
        // nothing is dropped on its account (/elsewhere/x.flac stays).
        assert_eq!((added, dropped), (0, 0));
        assert_eq!(playlist, vec![a.join("a1.flac"), PathBuf::from("/elsewhere/x.flac")]);
    }

    #[test]
    fn parse_m3u_strips_utf8_bom_from_first_entry() {
        let dir = std::env::temp_dir().join(format!("keet_m3u_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let track = dir.join("song.mp3");
        fs::write(&track, b"").unwrap();
        let m3u = dir.join("list.m3u");
        // Windows editors commonly prepend a UTF-8 BOM.
        fs::write(&m3u, format!("\u{feff}{}\n", track.display())).unwrap();

        let parsed = parse_m3u(&m3u).expect("BOM-prefixed first entry should parse");
        assert_eq!(parsed, vec![track.clone()]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_latin1_playlist_still_finds_its_tracks() {
        let dir = std::env::temp_dir().join(format!("keet_m3u_latin1_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let track = dir.join("café.mp3"); // UTF-8 on disk
        fs::write(&track, b"").unwrap();
        let m3u = dir.join("old.m3u");
        // "café.mp3" in Latin-1: é is the single byte 0xE9, not valid UTF-8.
        fs::write(&m3u, b"#EXTM3U\r\ncaf\xE9.mp3\r\n").unwrap();
        let parsed = parse_m3u(&m3u).expect("a Latin-1 playlist used to fail as a whole");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(parsed, vec![track]);
    }

    #[test]
    fn saving_replaces_the_file_whole_and_leaves_no_temporary_behind() {
        let dir = std::env::temp_dir().join(format!("keet_m3u_save_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("mix.m3u");
        fs::write(&target, b"old contents that are longer than the new ones").unwrap();
        let saved = save_m3u(&[PathBuf::from("/music/a.flac")], target.to_str().unwrap()).unwrap();
        let text = fs::read_to_string(&saved).unwrap();
        let leftovers = fs::read_dir(&dir).unwrap().count();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(text, "#EXTM3U\n/music/a.flac\n");
        assert_eq!(leftovers, 1, "only the playlist itself");
    }
}
