use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};

use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::TrackType;
use symphonia::core::common::Limit;
use symphonia::core::formats::FormatReader;
use symphonia::core::meta::{MetadataOptions, MetadataRevision, RawValue, StandardTag};

#[derive(Clone)]
struct CachedMeta {
    display: String,
    search_text: String,
    artist: Option<String>,
    title: Option<String>,
    album: Option<String>,
    // RG values are read per-track by the decoder, not from this cache (yet).
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
    track_number: Option<u32>,
    disc_number: Option<u32>,
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
            crate::ansi::sanitize_display(&path.file_name().unwrap_or_default().to_string_lossy())
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

    pub fn track_number(&self, index: usize) -> Option<u32> {
        let entries = self.entries.read().unwrap();
        entries.get(index).and_then(|e| e.as_ref()).and_then(|m| m.track_number)
    }

    pub fn disc_number(&self, index: usize) -> Option<u32> {
        let entries = self.entries.read().unwrap();
        entries.get(index).and_then(|e| e.as_ref()).and_then(|m| m.disc_number)
    }

    pub fn title(&self, index: usize) -> Option<String> {
        let entries = self.entries.read().unwrap();
        entries.get(index).and_then(|e| e.as_ref()).and_then(|m| m.title.clone())
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

    // NOTE: there are deliberately NO positional mutators (remove_at/move_entry)
    // here. Scan workers write by the index of the playlist snapshot they were
    // spawned with, so any reshape of the entries Vec must go through
    // `ui::reindex_and_restart_scan` (cancel → join → remap by path → respawn).

    /// Sum of every known track duration, under ONE read lock — HiFi's
    /// library header totals the playlist each frame, and summing through
    /// `duration(i)` took a lock per track (10k locks a frame on a big list).
    pub fn total_duration(&self) -> f64 {
        let entries = self.entries.read().unwrap();
        entries.iter().flatten().filter_map(|m| m.duration_secs).sum()
    }

    /// The longest known track duration (one read lock).
    pub fn max_duration(&self) -> f64 {
        let entries = self.entries.read().unwrap();
        entries.iter().flatten().filter_map(|m| m.duration_secs).fold(0.0, f64::max)
    }

    pub fn duration(&self, index: usize) -> Option<f64> {
        let entries = self.entries.read().unwrap();
        entries.get(index).and_then(|e| e.as_ref()).and_then(|m| m.duration_secs)
    }

    pub fn is_set(&self, index: usize) -> bool {
        let entries = self.entries.read().unwrap();
        entries.get(index).map(|e| e.is_some()).unwrap_or(false)
    }
}

/// The default audio track's sample rate.
fn rate_of(format: &dyn FormatReader) -> Option<u32> {
    format
        .default_track(TrackType::Audio)?
        .codec_params
        .as_ref()?
        .audio()?
        .sample_rate
}

/// Every metadata block the reader holds, NEWEST first. Symphonia logs them
/// oldest first, and the probe reads trailing tags (ID3v1, APE) before the
/// leading ID3v2, which the container's own tags then follow. Its `current()`
/// is the oldest block — so reading only that gave an MP3 tagged both ways a
/// 30-character ID3v1 title, no lyrics, no ReplayGain and no cover. Callers
/// fill each field from the first block that has it, so newest-first makes the
/// richer, more specific block win and older ones only fill its gaps.
///
/// ID3v1 always goes last, whatever its place in the log: its fields are cut
/// to 30 characters, so it is only ever a fallback. Read before APE (which
/// sits just ahead of it at the end of the file) it gave truncated titles to
/// files carrying a full APE tag.
pub(crate) fn revisions_newest_first(format: &mut dyn FormatReader) -> Vec<MetadataRevision> {
    let mut md = format.metadata();
    let mut older = Vec::new();
    while let Some(r) = md.pop() {
        older.push(r);
    }
    let mut out: Vec<MetadataRevision> = md.current().cloned().into_iter().collect();
    out.extend(older.into_iter().rev());
    id3v1_last(out, |r| r.info.short_name)
}

/// `revs` with every ID3v1 block moved to the end, the rest in order.
fn id3v1_last<T>(revs: Vec<T>, name: impl Fn(&T) -> &str) -> Vec<T> {
    let (v1, mut rest): (Vec<T>, Vec<T>) = revs.into_iter().partition(|r| name(r) == "id3v1");
    rest.extend(v1);
    rest
}

/// A tag's number, read leniently: surrounding space and a decimal comma
/// ("-6,50", as some European-locale taggers write it) are accepted. Never a
/// non-finite value: "nan" and "inf" parse as f32, and a NaN gain made the
/// limiter zero every sample — a silent track.
fn parse_tag_number(s: &str) -> Option<f32> {
    let v = s.trim().replace(',', ".").parse::<f32>().ok()?;
    v.is_finite().then_some(v)
}

/// Parse a ReplayGain gain string like "-7.2 dB" or "-7.2" into an f32 dB
/// value. The unit is matched case-insensitively ("DB" occurs).
pub fn parse_rg_gain_value(s: &str) -> Option<f32> {
    let s = s.trim();
    // `get`, not slicing: a tag ending in a multi-byte character ("-6 €")
    // put the cut inside it, and the slice panicked — in the metadata scan
    // that killed the scan thread.
    let num = match s.len().checked_sub(2).and_then(|i| s.get(i..)) {
        Some(unit) if unit.eq_ignore_ascii_case("db") => &s[..s.len() - 2],
        _ => s,
    };
    parse_tag_number(num)
}

/// Parse a ReplayGain peak (linear sample peak, 1.0 = full scale). Only a
/// positive value is a usable peak.
pub(crate) fn parse_rg_peak_value(s: &str) -> Option<f32> {
    parse_tag_number(s).filter(|&p| p > 0.0)
}

/// Tag values accumulated across one or more metadata sources. Each field is
/// filled only once (first non-empty wins), so the primary container metadata
/// takes precedence over probe-side tags.
#[derive(Default)]
struct TagFields {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    lyrics: Option<String>,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    rg_track_gain: Option<f32>,
    rg_track_peak: Option<f32>,
    rg_album_gain: Option<f32>,
    rg_album_peak: Option<f32>,
}

/// Merge a metadata revision's tags into `fields`, filling only gaps. Shared by
/// the container-metadata and probe-side passes so the extraction logic lives once.
fn merge_metadata_tags(fields: &mut TagFields, tags: &[symphonia::core::meta::Tag]) {
    for tag in tags {
        // Symphonia 0.6 carries the parsed value inside the StandardTag variant
        // itself, replacing 0.5's (std_key, value) pair — and it now maps
        // ReplayGain to standard tags, which used to need raw-key matching.
        match &tag.std {
            Some(StandardTag::TrackTitle(s)) if fields.title.is_none() => {
                fields.title = Some(s.to_string());
            }
            Some(StandardTag::Artist(s)) if fields.artist.is_none() => {
                fields.artist = Some(s.to_string());
            }
            Some(StandardTag::Album(s)) if fields.album.is_none() => {
                fields.album = Some(s.to_string());
            }
            Some(StandardTag::Lyrics(s)) if fields.lyrics.is_none() => {
                fields.lyrics = Some(s.to_string());
            }
            Some(StandardTag::TrackNumber(n)) if fields.track_number.is_none() => {
                fields.track_number = (*n).try_into().ok();
            }
            Some(StandardTag::DiscNumber(n)) if fields.disc_number.is_none() => {
                fields.disc_number = (*n).try_into().ok();
            }
            Some(StandardTag::ReplayGainTrackGain(s)) if fields.rg_track_gain.is_none() => {
                fields.rg_track_gain = parse_rg_gain_value(s);
            }
            Some(StandardTag::ReplayGainTrackPeak(s)) if fields.rg_track_peak.is_none() => {
                fields.rg_track_peak = parse_rg_peak_value(s);
            }
            Some(StandardTag::ReplayGainAlbumGain(s)) if fields.rg_album_gain.is_none() => {
                fields.rg_album_gain = parse_rg_gain_value(s);
            }
            Some(StandardTag::ReplayGainAlbumPeak(s)) if fields.rg_album_peak.is_none() => {
                fields.rg_album_peak = parse_rg_peak_value(s);
            }
            _ => {}
        }
        // Raw fallback: tags a reader didn't map to a standard one, plus the
        // "5/12" track-number form that only appears in the raw value.
        if fields.track_number.is_none() && tag.raw.key.eq_ignore_ascii_case("tracknumber") {
            fields.track_number = parse_leading_u32(&tag.raw.value);
        }
        if fields.disc_number.is_none() && tag.raw.key.eq_ignore_ascii_case("discnumber") {
            fields.disc_number = parse_leading_u32(&tag.raw.value);
        }
        let key_lower = tag.raw.key.to_lowercase();
        if let RawValue::String(ref s) = tag.raw.value {
            match key_lower.as_str() {
                "replaygain_track_gain" if fields.rg_track_gain.is_none() => {
                    fields.rg_track_gain = parse_rg_gain_value(s);
                }
                "replaygain_track_peak" if fields.rg_track_peak.is_none() => {
                    fields.rg_track_peak = parse_rg_peak_value(s);
                }
                "replaygain_album_gain" if fields.rg_album_gain.is_none() => {
                    fields.rg_album_gain = parse_rg_gain_value(s);
                }
                "replaygain_album_peak" if fields.rg_album_peak.is_none() => {
                    fields.rg_album_peak = parse_rg_peak_value(s);
                }
                "lyrics" | "unsyncedlyrics" if fields.lyrics.is_none() => {
                    fields.lyrics = Some(s.to_string());
                }
                _ => {}
            }
        }
    }
}

fn read_metadata_full(path: &Path) -> Option<CachedMeta> {
    let file = File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension() {
        hint.with_extension(ext.to_str().unwrap_or(""));
    }
    // Skip embedded pictures: this scan only wants tags, and decoding every
    // FLAC PICTURE block (often 0.5–2 MB each) across a whole library was pure
    // allocator churn — covers are loaded separately by cover.rs, which does
    // its own probe with visuals enabled. Same trick as the decode thread.
    // 0.6's MetadataOptions is non-exhaustive, so it's built with the setters
    // rather than a struct literal (limit_metadata_bytes is now limit_tag_bytes,
    // left at its default).
    let meta_opts = MetadataOptions::default().limit_visual_bytes(Limit::Maximum(0));
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), meta_opts)
        .ok()?;

    // 0.6 moved track timing out of codec_params onto the Track itself, which
    // is exactly why it's now reliably populated.
    let duration_secs: Option<f64> = format.default_track(TrackType::Audio).and_then(|t| {
        let rate = t.codec_params.as_ref().and_then(|c| c.audio()).and_then(|a| a.sample_rate);
        if let (Some(n_frames), Some(rate)) = (t.num_frames, rate) {
            if rate > 0 {
                return Some(n_frames as f64 / rate as f64);
            }
        }
        if let (Some(tb), Some(n_frames)) = (t.time_base, t.num_frames) {
            return Some(n_frames as f64 * tb.numer.get() as f64 / tb.denom.get() as f64);
        }
        None
    });

    // AAC in MP4: the container's length includes the encoder's priming and
    // padding; the real length is in iTunSMPB or the edit list.
    let revisions = revisions_newest_first(format.as_mut());
    let duration_secs = match rate_of(format.as_ref()).filter(|&r| r > 0) {
        Some(rate) => crate::gapless::for_mp4(path, &revisions, rate)
            .and_then(|g| g.length)
            .map_or(duration_secs, |len| Some(len as f64 / rate as f64)),
        None => duration_secs,
    };

    let mut fields = TagFields::default();

    // 0.6 folds probe-side metadata (leading ID3v2, trailing ID3v1/APE) into
    // the format reader's log, alongside the container's own tags.
    for rev in &revisions {
        merge_metadata_tags(&mut fields, &rev.media.tags);
    }

    let TagFields {
        title, artist, album, lyrics, track_number, disc_number,
        rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak,
    } = fields;
    // Everything here ends up in frame lines: no control characters (a tag
    // can carry a newline or an ESC).
    let clean = |t: Option<String>| t.map(|s| crate::ansi::sanitize_display(&s));
    let (title, artist, album) = (clean(title), clean(artist), clean(album));

    let filename = crate::ansi::sanitize_display(
        &path.file_name().unwrap_or_default().to_string_lossy(),
    );
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
        track_number,
        disc_number,
    })
}

/// Parse a numeric prefix from a tag value: "5" → 5, "5/12" → 5, U32(7) → 7.
fn parse_leading_u32(value: &RawValue) -> Option<u32> {
    match value {
        RawValue::UnsignedInt(n) => (*n).try_into().ok(),
        RawValue::SignedInt(n) => (*n).try_into().ok(),
        RawValue::String(s) => {
            let digits: String = s.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        }
        _ => None,
    }
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

#[cfg(test)]
mod real_file_tests {
    use super::*;

    #[test]
    fn id3v1_is_only_ever_the_fallback() {
        let order = |names: &[&'static str]| id3v1_last(names.to_vec(), |n| n);
        assert_eq!(order(&["id3v2", "id3v1", "apev2"]), ["id3v2", "apev2", "id3v1"]);
        assert_eq!(order(&["id3v1", "apev2"]), ["apev2", "id3v1"]);
        assert_eq!(order(&["id3v1"]), ["id3v1"]);
        assert_eq!(order(&["vorbis", "flac"]), ["vorbis", "flac"], "others keep their order");
    }

    #[test]
    fn a_gain_tag_ending_in_a_multibyte_character_is_refused_not_a_panic() {
        assert_eq!(parse_rg_gain_value("-6 €"), None);
        assert_eq!(parse_rg_gain_value("€"), None);
        assert_eq!(parse_rg_gain_value("ü"), None);
        assert_eq!(parse_rg_gain_value("-6.5 dB"), Some(-6.5));
        assert_eq!(parse_rg_gain_value("-6,5DB"), Some(-6.5));
    }

    #[test]
    fn replaygain_values_parse_leniently_but_never_to_a_non_finite_gain() {
        assert_eq!(parse_rg_gain_value("-7.2 dB"), Some(-7.2));
        assert_eq!(parse_rg_gain_value("+3.50 DB"), Some(3.5));
        assert_eq!(parse_rg_gain_value("-6,50 dB"), Some(-6.5), "decimal comma");
        assert_eq!(parse_rg_gain_value(" -1.25db "), Some(-1.25));
        // "nan"/"inf" parse as f32 and made the gain NaN -> the limiter turned
        // every sample into 0 -> a silent track.
        for bad in ["nan", "NaN dB", "inf", "-inf dB", "", "dB", "loud"] {
            assert_eq!(parse_rg_gain_value(bad), None, "{bad:?}");
        }
        assert_eq!(parse_rg_peak_value("0,988"), Some(0.988));
        for bad in ["nan", "inf", "-0.5", "0"] {
            assert_eq!(parse_rg_peak_value(bad), None, "peak {bad:?}");
        }
    }

    #[test]
    fn aac_durations_leave_out_priming_and_padding() {
        for f in ["sine_aac_itunsmpb.m4a", "sine_aac_editlist.m4a"] {
            let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(f);
            let d = read_metadata_full(&p).and_then(|m| m.duration_secs).unwrap();
            assert!((d - 1.0).abs() < 1e-6, "{f}: {d}");
        }
    }

    #[test]
    fn mp3_tags_come_from_id3v2_when_an_id3v1_block_is_also_present() {
        // Symphonia reads the TRAILING ID3v1 block before the leading ID3v2
        // and its `current()` is the oldest block, so reading only that gave
        // a title cut to 30 characters, no artist beyond ID3v1's and no
        // ReplayGain at all. The newest block must win.
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/sine_lr_id3v1v2.mp3");
        let m = read_metadata_full(&p).expect("fixture reads");
        assert_eq!(m.title.as_deref(), Some("A Title Much Longer Than Thirty Characters"));
        assert_eq!(m.artist.as_deref(), Some("Fixture Artist"));
        assert_eq!(m.rg_track_gain, Some(-6.02));
        assert_eq!(m.rg_track_peak, Some(0.5));
    }

    /// Reads real tagged files from the user's library. Ignored by default;
    /// run with `cargo test -- --ignored --nocapture`.
    ///
    /// Tag extraction is the part of the symphonia 0.6 migration that no
    /// synthesized fixture covers — 0.6 replaced the (std_key, value) pair with
    /// value-carrying StandardTag variants, so a mistake here silently yields
    /// blank artists/titles rather than failing to compile.
    #[test]
    #[ignore = "requires a local music library"]
    fn reads_tags_from_real_files() {
        let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join("Music/local");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .expect("music dir")
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(p.extension().and_then(|e| e.to_str()), Some("flac" | "mp3" | "m4a"))
            })
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no audio files found in {}", dir.display());

        let mut tagged = 0;
        for p in files.iter().take(12) {
            let m = read_metadata_full(p);
            match &m {
                Some(m) => {
                    println!(
                        "{:<52} artist={:?} title={:?} dur={:?} rg={:?}",
                        p.file_name().unwrap().to_string_lossy(),
                        m.artist, m.title, m.duration_secs.map(|d| d.round()), m.rg_track_gain
                    );
                    if m.artist.is_some() || m.title.is_some() {
                        tagged += 1;
                    }
                }
                None => println!("{:<52} FAILED TO READ", p.file_name().unwrap().to_string_lossy()),
            }
        }
        assert!(tagged > 0, "no file yielded artist/title — tag extraction is broken");

        // ReplayGain: symphonia 0.6 maps these to StandardTag variants, a path
        // the library files above don't exercise. Uses a fixture written by
        // ffmpeg if present (see the migration notes).
        let rg_file = std::path::Path::new("/tmp/rgtagged.flac");
        if rg_file.exists() {
            let m = read_metadata_full(rg_file).expect("rg fixture readable");
            println!("RG fixture: track_gain={:?} track_peak={:?} album_gain={:?}",
                     m.rg_track_gain, m.rg_track_peak, m.rg_album_gain);
            assert_eq!(m.rg_track_gain, Some(-7.25), "track gain");
            assert_eq!(m.rg_album_gain, Some(-6.50), "album gain");
            assert!((m.rg_track_peak.unwrap() - 0.988525).abs() < 1e-6, "track peak");
        }
    }
}
