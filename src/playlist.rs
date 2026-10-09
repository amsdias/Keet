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
/// then the line read as Windows-1252 (what Windows tools write in "ANSI").
///
/// Only 1252 among the ANSI code pages, deliberately. "ANSI" is whatever the
/// writing PC's locale was (1250 Central European, 1251 Cyrillic, 932
/// Japanese …), and the file does not say which. 1252 is a 32-entry table
/// here; the others need full code-page tables (932 alone is thousands of
/// entries — in practice the `encoding_rs` crate, a sizeable share of a
/// binary kept small on purpose, see native-tls in CLAUDE.md) for playlists
/// that are rarer every year: Windows tools write `.m3u8`/UTF-8 now. 1252
/// covers the Western-European playlists that were the common case.
fn m3u_entry_candidates(line: &[u8]) -> Vec<PathBuf> {
    if let Ok(s) = std::str::from_utf8(line) {
        return vec![PathBuf::from(s)];
    }
    let ansi = PathBuf::from(line.iter().map(|&b| windows_1252(b)).collect::<String>());
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        vec![PathBuf::from(std::ffi::OsStr::from_bytes(line)), ansi]
    }
    #[cfg(not(unix))]
    vec![ansi]
}

/// One Windows-1252 byte as a char. It is Latin-1 except for 0x80–0x9F,
/// where it keeps its punctuation (’ “ ” – — … €): read as Latin-1 those
/// became control characters and the entry silently missed its file. The five
/// unassigned bytes keep their Latin-1 meaning.
fn windows_1252(b: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}',
        '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
    ];
    match b {
        0x80..=0x9F => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

/// Write `bytes` to `path` whole or not at all: into a temporary file beside
/// it, then renamed over it (atomic on the same filesystem, POSIX and
/// Windows alike). Writing in place left a truncated file behind a failed or
/// interrupted write. The temporary name is unique per call, so two workers
/// saving the same file cannot trample each other's half-written copy.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(
        ".{name}.{}.{}.keet-tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, bytes)?;
    // On Windows a rename over a file that another process has open (a
    // virus scanner or the search indexer reading the file just written)
    // fails with "access denied" for a moment; a few short retries ride
    // that out. Elsewhere a rename replaces an open file, so one try.
    let tries = if cfg!(windows) { 5 } else { 1 };
    let mut result = fs::rename(&tmp, path);
    for _ in 1..tries {
        match &result {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                std::thread::sleep(std::time::Duration::from_millis(20));
                result = fs::rename(&tmp, path);
            }
            _ => break,
        }
    }
    // A temp file is left behind only if the process dies between the write
    // and the rename (a crash or power loss mid-save). It is a dot-file next
    // to the target, a few KB, and the next save of that file does not need
    // it; scanning for and deleting such leftovers at startup would be more
    // code (and more deleting of files) than the rare leftover is worth.
    result.inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Save a playlist as an M3U file.
/// If `name` contains a path separator, treat it as a full path.
/// Otherwise, save to ~/.config/keet/playlists/<name>.m3u.
pub fn save_m3u(playlist: &[PathBuf], name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    // ".M3U" is an M3U too: a case-sensitive check made "Mix.M3U" into
    // "Mix.m3u" beside it (on a case-sensitive file system) or "Mix.M3U.m3u".
    let is_m3u = |n: &str| {
        let lower = n.to_ascii_lowercase();
        lower.ends_with(".m3u") || lower.ends_with(".m3u8")
    };
    let path = if name.contains('/') || name.contains('\\') {
        let p = PathBuf::from(name);
        if !is_m3u(&p.to_string_lossy()) {
            p.with_extension("m3u")
        } else {
            p
        }
    } else {
        let dir = keet_config_dir()
            .ok_or("Could not determine config directory")?
            .join("playlists");
        fs::create_dir_all(&dir)?;
        let filename = if is_m3u(name) {
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
    // Whole or not at all: writing in place left a truncated playlist if the
    // write failed half-way (a full disk, a network share dropping out).
    write_atomic(&path, &content)?;
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
    // two sources or a symlink), keeping the first — except for the playing
    // file, whose PLAYING entry stays: keeping an earlier copy instead moved
    // playback to that position, replaying or skipping a stretch.
    let current_key = current_track_path.map(|c| key(&c.to_path_buf()));
    let mut seen = HashSet::new();
    let mut kept_current = false;
    playlist.retain(|p| {
        let k = key(p);
        if current_key.as_ref() == Some(&k) {
            let keep = !kept_current && current_track_path == Some(p.as_path());
            kept_current |= keep;
            keep
        } else {
            seen.insert(k)
        }
    });
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
    fn a_rescan_keeps_the_playing_copy_of_a_duplicated_file() {
        // The same file under two names (a symlink, two sources): dedup kept
        // the FIRST, dropping the entry actually playing.
        let first = PathBuf::from("/lib/link/song.flac");
        let playing = PathBuf::from("/lib/real/song.flac");
        let other = PathBuf::from("/lib/real/other.flac");
        let mut playlist = vec![first.clone(), other.clone(), playing.clone()];
        let canon: std::collections::HashMap<PathBuf, PathBuf> =
            [(first.clone(), playing.clone()), (playing.clone(), playing.clone()), (other.clone(), other.clone())].into();
        let r = RescanResult { fresh: vec![playing.clone(), other.clone()], canon, had_error: false };
        apply_rescan(&mut playlist, &r, Some(&playing), &Default::default());
        assert_eq!(playlist, [other, playing], "the playing entry stays, the other copy goes");
    }

    #[test]
    fn a_windows_ansi_playlist_keeps_its_quotes_and_dashes() {
        assert_eq!(windows_1252(0x92), '’');
        assert_eq!(windows_1252(0x96), '–');
        assert_eq!(windows_1252(0x80), '€');
        assert_eq!(windows_1252(0xE9), 'é');
        let dir = std::env::temp_dir().join(format!("keet_m3u_1252_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let track = dir.join("Don’t Stop – Live.mp3");
        fs::write(&track, b"").unwrap();
        let m3u = dir.join("ansi.m3u");
        fs::write(&m3u, b"Don\x92t Stop \x96 Live.mp3\r\n").unwrap();
        let parsed = parse_m3u(&m3u).expect("the entry was skipped: its quote and dash read as controls");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(parsed, vec![track]);
    }

    #[test]
    fn write_atomic_replaces_whole_files_and_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("keet_atomic_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("offsets.json");
        fs::write(&f, b"a much longer old content").unwrap();
        write_atomic(&f, b"new").unwrap();
        let (text, n) = (fs::read(&f).unwrap(), fs::read_dir(&dir).unwrap().count());
        let _ = fs::remove_dir_all(&dir);
        assert_eq!((text.as_slice(), n), (&b"new"[..], 1));
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
