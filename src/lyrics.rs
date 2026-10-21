//! LRC lyrics parser and synced lyrics state.
//!
//! Supports:
//!
//! - Plain (unsynced) lyrics — just text lines
//! - LRC (synced) lyrics — `[MM:SS.xx]Line text` with auto-scroll by playback position

/// A single synced lyrics line: timestamp in seconds + text.
#[derive(Clone, PartialEq)]
pub struct LrcLine {
    pub time: f64,
    pub text: String,
}

/// Parsed lyrics: either synced (with timestamps) or plain text lines.
#[derive(PartialEq)]
pub enum Lyrics {
    Synced(Vec<LrcLine>),
    Plain(Vec<String>),
}

impl Lyrics {
    pub fn line_count(&self) -> usize {
        match self {
            Lyrics::Synced(lines) => lines.len(),
            Lyrics::Plain(lines) => lines.len(),
        }
    }

    pub fn line_text(&self, index: usize) -> &str {
        match self {
            Lyrics::Synced(lines) => lines.get(index).map(|l| l.text.as_str()).unwrap_or(""),
            Lyrics::Plain(lines) => lines.get(index).map(|s| s.as_str()).unwrap_or(""),
        }
    }

    /// For synced lyrics, find the index of the current line based on playback position.
    pub fn current_line(&self, position_secs: f64) -> Option<usize> {
        match self {
            Lyrics::Synced(lines) => {
                if lines.is_empty() { return None; }
                // Find the last line whose timestamp <= position
                let mut idx = None;
                for (i, line) in lines.iter().enumerate() {
                    if line.time <= position_secs {
                        idx = Some(i);
                    } else {
                        break;
                    }
                }
                idx
            }
            Lyrics::Plain(_) => None,
        }
    }

    pub fn is_synced(&self) -> bool {
        matches!(self, Lyrics::Synced(_))
    }

    /// A synced line's timestamp (None for plain lyrics).
    pub fn line_time(&self, index: usize) -> Option<f64> {
        match self {
            Lyrics::Synced(lines) => lines.get(index).map(|l| l.time),
            Lyrics::Plain(_) => None,
        }
    }
}

/// Parse raw lyrics text into a Lyrics struct.
/// Detects LRC format by looking for `[MM:SS` patterns.
pub fn parse_lyrics(raw: &str) -> Lyrics {
    // Check if this looks like LRC (at least one timestamp line)
    let has_timestamps = raw.lines().any(|line| !parse_lrc_line(line).is_empty());

    if has_timestamps {
        // [offset:±N]: a whole-file adjustment in milliseconds; positive shows
        // the lyrics earlier. It was ignored, so files that rely on it ran
        // out of sync by exactly that much.
        let offset_secs = raw.lines().find_map(parse_lrc_offset).unwrap_or(0.0);
        let mut lines: Vec<LrcLine> = Vec::new();
        for line in raw.lines() {
            // A line may carry several timestamps sharing the same text.
            // Non-timestamped lines (metadata like [ar:Artist]) yield nothing.
            for (time, text) in parse_lrc_line(line) {
                let time = (time - offset_secs).max(0.0);
                lines.push(LrcLine { time, text: clean_line(&strip_word_tags(&text)) });
            }
        }
        lines.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
        Lyrics::Synced(lines)
    } else {
        let lines: Vec<String> = raw.lines().map(clean_line).collect();
        Lyrics::Plain(lines)
    }
}

/// A lyric line as it is shown. Lyrics are drawn straight into frame lines,
/// and LRCLIB text is user-submitted: control characters (ESC, CR, ...) are
/// stripped. Spaces at either end are dropped — most LRC files put one after
/// the timestamp (`[00:17.12] I feel it`), which shifted those lines a
/// column right of the rest and off Minimal's centre.
fn clean_line(text: &str) -> String {
    crate::ansi::sanitize_display(text).trim().to_string()
}

/// Remove enhanced-LRC word timing (`<00:12.34>` before each word): Keet
/// highlights whole lines, and the tags were shown raw in the text.
fn strip_word_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut removed = false;
    while let Some(open) = rest.find('<') {
        let tag_end = rest[open..].find('>').map(|e| open + e);
        match tag_end {
            Some(end) if is_timestamp(&rest[open + 1..end]) => {
                out.push_str(&rest[..open]);
                rest = &rest[end + 1..];
                removed = true;
            }
            _ => {
                out.push_str(&rest[..open + 1]);
                rest = &rest[open + 1..];
            }
        }
    }
    out.push_str(rest);
    if !removed {
        return out;
    }
    // A tag between words leaves its spaces behind: collapse them (only
    // then — a line's own spacing is left as written).
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `mm:ss`, `mm:ss.xx` or `mm:ss:xx` — the inside of a word timing tag.
fn is_timestamp(s: &str) -> bool {
    !s.is_empty()
        && s.contains(':')
        && s.chars().all(|c| c.is_ascii_digit() || c == ':' || c == '.')
}

/// Parse an LRC line into `(seconds, text)` for each leading timestamp tag.
/// Supports `[MM:SS]`, `[MM:SS.xx]` and `[HH:MM:SS.xx]`, and multiple timestamps
/// sharing one line (e.g. `[00:12.00][00:48.00]Chorus`), which LRCLIB occasionally
/// returns. Returns empty for metadata lines like `[ar:Artist]` and untimed text.
fn parse_lrc_line(line: &str) -> Vec<(f64, String)> {
    let mut rest = line.trim();
    let mut times: Vec<f64> = Vec::new();
    while let Some(stripped) = rest.strip_prefix('[') {
        let close = match stripped.find(']') {
            Some(c) => c,
            None => break,
        };
        match parse_lrc_time(&stripped[..close]) {
            Some(t) => {
                times.push(t);
                rest = &stripped[close + 1..];
            }
            None => break, // not a timestamp (e.g. [ar:...]) — stop scanning tags
        }
    }
    if times.is_empty() {
        return Vec::new();
    }
    let text = rest.to_string();
    times.into_iter().map(|t| (t, text.clone())).collect()
}

/// The `[offset:±N]` tag (milliseconds) as seconds, if `line` is one.
fn parse_lrc_offset(line: &str) -> Option<f64> {
    let inner = line.trim().strip_prefix('[')?.strip_suffix(']')?;
    let (key, value) = inner.split_once(':')?;
    if !key.trim().eq_ignore_ascii_case("offset") {
        return None;
    }
    let ms: f64 = value.trim().trim_start_matches('+').parse().ok()?;
    ms.is_finite().then_some(ms / 1000.0)
}

/// Parse the inside of an LRC time tag to seconds: `MM:SS`, `MM:SS.xx`,
/// `MM:SS:xx` (hundredths after a colon, as some editors write them) or
/// `HH:MM:SS.xx`. A three-part tag is hours only when its seconds carry a
/// decimal point (`[01:02:50]` is 1 min 2.5 s, not an hour).
fn parse_lrc_time(inside: &str) -> Option<f64> {
    let parts: Vec<&str> = inside.split(':').collect();
    let num = |s: &str| -> Option<f64> {
        let v: f64 = s.trim().parse().ok()?;
        (v.is_finite() && v >= 0.0).then_some(v)
    };
    match parts.as_slice() {
        [m, s] => Some(num(m)? * 60.0 + num(s)?),
        [h, m, s] if s.contains('.') => Some(num(h)? * 3600.0 + num(m)? * 60.0 + num(s)?),
        [m, s, frac] => {
            let digits = frac.trim();
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let frac = num(digits)? / 10f64.powi(digits.len() as i32);
            Some(num(m)? * 60.0 + num(s)? + frac)
        }
        _ => None,
    }
}

/// Process-wide HTTP agent shared by every fetch (LRCLIB lyrics, iTunes
/// covers). One native-TLS context for the process instead of a fresh one per
/// request (one per fetch leaves lingering Security.framework allocations
/// on macOS).
///
/// Split timeouts, NOT `timeout_global`: in ureq 3.3 the global timer trips
/// during TCP/TLS setup, failing every HTTPS call before the handshake even
/// completes. LRCLIB can take >7 s to first byte on a slow day; fetches run on
/// worker threads (generation-counter aborted on skip), so be generous.
pub(crate) fn http_agent() -> ureq::Agent {
    use std::sync::OnceLock;
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT
        .get_or_init(|| {
            // RootCerts::PlatformVerifier is REQUIRED, not a preference.
            //
            // ureq's default is `RootCerts::WebPki` (bundled Mozilla roots),
            // which with native-tls calls `disable_built_in_roots(true)` and
            // hands the platform a root set it must then find in the chain it
            // built. On Windows/schannel that check always fails —
            // "unable to find any user-specified roots in the final cert chain"
            // — so every HTTPS call dies in ~40 ms while macOS's
            // Security.framework happily accepts the same config. Using the
            // platform's own trust store is the whole reason we're on
            // native-tls instead of rustls.
            let tls = ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build();
            ureq::Agent::config_builder()
                .tls_config(tls)
                .timeout_connect(Some(std::time::Duration::from_secs(5)))
                .timeout_recv_response(Some(std::time::Duration::from_secs(15)))
                .timeout_recv_body(Some(std::time::Duration::from_secs(15)))
                .user_agent("Keet Audio Player (https://github.com/amsdias/Keet)")
                .build()
                .new_agent()
        })
        .clone() // Agent is an Arc handle — cloning shares the pool/TLS context
}

/// Fetch lyrics from LRCLIB (free, no API key, ~3M entries).
/// Prefers synced (LRC) lyrics over plain.
/// Returns raw lyrics text or None on failure/not found.
/// The outcome of a network lookup. A definite answer (found or not found)
/// can be remembered; a failure (offline, timeout, server error) says nothing
/// about the next attempt.
pub enum Lookup<T> {
    Found(T),
    NotFound,
    Failed,
    /// Asked too often (HTTP 429), with the Retry-After seconds when given.
    Busy(Option<u64>),
}

/// Session cache of network lookups, keyed by query. Shared with the worker
/// threads that do the fetching, so a track played again does not ask LRCLIB
/// (or iTunes) again — the same answer, and up to 15 s of waiting.
#[derive(Clone, Default)]
pub struct LookupCache<T: Clone = String> {
    map: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Option<T>>>>,
}

impl<T: Clone> LookupCache<T> {
    /// The remembered answer, if there is one (`Some(None)` = known missing).
    pub fn known(&self, key: &str) -> Option<Option<T>> {
        self.map.lock().ok().and_then(|m| m.get(key).cloned())
    }

    pub fn remember(&self, key: String, answer: Option<T>) {
        if let Ok(mut m) = self.map.lock() {
            m.insert(key, answer);
        }
    }
}

pub fn fetch_lrclib(artist: &str, title: &str, duration_secs: Option<u32>) -> Lookup<String> {
    let mut url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        crate::cover::urlencoded(artist),
        crate::cover::urlencoded(title),
    );
    if let Some(dur) = duration_secs {
        url.push_str(&format!("&duration={}", dur));
    }

    // Statuses read here, not as errors: a 429's Retry-After is a header.
    let response = match http_agent().get(&url).config().http_status_as_error(false).build().call() {
        Ok(r) => r,
        Err(_) => return Lookup::Failed,
    };
    match response.status().as_u16() {
        200 => {}
        // LRCLIB answers 404 for a track it has no lyrics for.
        404 => return Lookup::NotFound,
        // Asked too often: every launch asked again (see `LyricsDisk`).
        429 => {
            let after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok());
            return Lookup::Busy(after);
        }
        _ => return Lookup::Failed,
    }
    let Some(body) = response
        .into_body()
        .read_to_string()
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
    else {
        return Lookup::Failed;
    };

    // Prefer syncedLyrics (LRC format) over plainLyrics
    for field in ["syncedLyrics", "plainLyrics"] {
        if let Some(text) = body.get(field).and_then(|v| v.as_str()) {
            if !text.is_empty() {
                return Lookup::Found(text.to_string());
            }
        }
    }
    Lookup::NotFound
}


/// The wait after a 429 that gave no Retry-After: LRCLIB sends none, and
/// its limit lasts several seconds (5 s often asked again inside it).
const BUSY_WAIT_SECS: u64 = 10;

/// `fetch` (LRCLIB), asked once more after a wait when it was busy (429:
/// its Retry-After, capped at 30 s, else `BUSY_WAIT_SECS`) or failed (5 s).
/// `wait` sleeps up to the given time and says whether to go on (false: the
/// track changed). Found and not found are answers: no retry.
pub fn fetch_with_retry(
    mut fetch: impl FnMut() -> Lookup<String>,
    wait: impl FnOnce(std::time::Duration) -> bool,
) -> Lookup<String> {
    let first = fetch();
    let secs = match first {
        Lookup::Busy(after) => after.unwrap_or(BUSY_WAIT_SECS).clamp(1, 30),
        Lookup::Failed => 5,
        answered => return answered,
    };
    if !wait(std::time::Duration::from_secs(secs)) {
        return first;
    }
    fetch()
}

/// LRCLIB's answers kept on disk (`<config>/lyrics`), so a restart does not
/// ask again: it rate-limits (429), and every launch asking for the same
/// track hit it — the lyrics then failed on most runs. Found lyrics are kept
/// for good; "LRCLIB has none" for `MISSING_KEEP`, a day (lyrics get added
/// there, and a week hid them for a week).
/// Keyed by artist, title and duration (another duration, another version).
pub struct LyricsDisk {
    dir: std::path::PathBuf,
}

/// How long a "not on LRCLIB" answer is trusted.
const MISSING_KEEP: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

impl LyricsDisk {
    pub fn in_config() -> Option<Self> {
        Some(Self::at(crate::playlist::keet_config_dir()?.join("lyrics")))
    }

    pub fn at(dir: std::path::PathBuf) -> Self {
        Self { dir }
    }

    /// `<artist> - <title> - <duration> <hash>`: the names cut to a byte
    /// budget (a long classical title ran past the file system's 255 bytes
    /// and the save failed, silently, so every launch asked LRCLIB again),
    /// and a hash of the exact artist, title and duration, so two artists
    /// that clean up to the same text (AC/DC, AC:DC) get two files.
    fn base(&self, artist: &str, title: &str, dur: Option<u32>) -> Option<std::path::PathBuf> {
        let cut = |s: String, max: usize| -> String {
            let mut end = 0;
            for (i, c) in s.char_indices() {
                if i + c.len_utf8() > max {
                    break;
                }
                end = i + c.len_utf8();
            }
            s[..end].trim_end().to_string()
        };
        let (a, t) = (cut(crate::cover::sanitize_fs(artist), 60), cut(crate::cover::sanitize_fs(title), 100));
        if a.is_empty() || t.is_empty() {
            return None;
        }
        // FNV-1a: stable across runs and builds (std's hasher is not).
        let key = format!("{artist}\0{title}\0{}", dur.unwrap_or(0));
        let hash = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
        Some(self.dir.join(format!("{a} - {t} - {} {:08x}", dur.unwrap_or(0), hash as u32)))
    }

    /// `base` with `.ext` APPENDED: `with_extension` replaces whatever
    /// follows the last dot, and "Mr. Brightside" became "… - Mr.lrc",
    /// one file with "Mr. Jones".
    fn file(base: &std::path::Path, ext: &str) -> std::path::PathBuf {
        let mut name = base.as_os_str().to_os_string();
        name.push(".");
        name.push(ext);
        name.into()
    }

    /// `Some(Some(lyrics))` found, `Some(None)` known missing (today),
    /// `None` unknown.
    pub fn load(&self, artist: &str, title: &str, dur: Option<u32>) -> Option<Option<String>> {
        let base = self.base(artist, title, dur)?;
        if let Ok(text) = std::fs::read_to_string(Self::file(&base, "lrc")) {
            return Some(Some(text));
        }
        let age = std::fs::metadata(Self::file(&base, "none")).ok()?.modified().ok()?.elapsed().ok()?;
        (age < MISSING_KEEP).then_some(None)
    }

    pub fn store(&self, artist: &str, title: &str, dur: Option<u32>, lyrics: Option<&str>) {
        let Some(base) = self.base(artist, title, dur) else { return };
        let _ = std::fs::create_dir_all(&self.dir);
        match lyrics {
            Some(text) => {
                let _ = crate::playlist::write_atomic(&Self::file(&base, "lrc"), text.as_bytes());
                let _ = std::fs::remove_file(Self::file(&base, "none"));
            }
            None => {
                let _ = crate::playlist::write_atomic(&Self::file(&base, "none"), b"");
            }
        }
    }
}

/// LRCLIB's lyrics for (artist, title, duration): what this session or an
/// earlier run (`disk`) already knows, else `fetch` (with one retry, `wait`
/// between). Found and not-found answers are remembered, in both; a busy or
/// failed lookup is not (it says nothing about the track: the next play asks
/// again).
pub fn lookup_lrclib(
    net: &LookupCache,
    disk: Option<&LyricsDisk>,
    (artist, title, dur): (&str, &str, Option<u32>),
    fetch: impl FnMut() -> Lookup<String>,
    wait: impl FnOnce(std::time::Duration) -> bool,
) -> Lookup<String> {
    let key = format!("{artist}\0{title}\0{}", dur.unwrap_or(0));
    let known = net.known(&key).or_else(|| disk.and_then(|d| d.load(artist, title, dur)));
    if let Some(answer) = known {
        net.remember(key, answer.clone());
        return answer.map_or(Lookup::NotFound, Lookup::Found);
    }
    let answer = fetch_with_retry(fetch, wait);
    let keep = match &answer {
        Lookup::Found(text) => Some(Some(text.clone())),
        Lookup::NotFound => Some(None),
        Lookup::Busy(_) | Lookup::Failed => None,
    };
    if let Some(keep) = keep {
        if let Some(d) = disk {
            d.store(artist, title, dur, keep.as_deref());
        }
        net.remember(key, keep);
    }
    answer
}

/// What the lyrics worker found for a track.
pub enum LyricsAnswer {
    Found(Lyrics, LyricsSource),
    /// Neither the file nor LRCLIB has any.
    Missing,
    /// LRCLIB could not be asked (busy, unreachable): not "no lyrics".
    Unavailable(&'static str),
}

/// Where a track's lyrics came from, shown beside them ("synced · LRCLIB"):
/// LRCLIB is user-submitted, so its timing and text deserve less trust than
/// the file's own tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LyricsSource {
    Embedded,
    /// A `.lrc` file beside the track (`sidecar`).
    File,
    Lrclib,
}

impl LyricsSource {
    pub fn label(self) -> &'static str {
        match self {
            LyricsSource::Embedded => "embedded",
            LyricsSource::File => "file",
            LyricsSource::Lrclib => "LRCLIB",
        }
    }
}

/// The `.lrc` file beside a track (`song.flac` → `song.lrc`): what players
/// and tools such as LRCGET write, and the user's own choice, so it comes
/// before the tags and LRCLIB.
pub fn sidecar_path(audio: &std::path::Path) -> std::path::PathBuf {
    audio.with_extension("lrc")
}

/// Whether two lyrics texts read the same once parsed (header tags, spaces
/// and blank lines at the ends, line endings aside).
pub fn same_lyrics(a: &str, b: &str) -> bool {
    parse_lyrics(a.trim()) == parse_lyrics(b.trim())
}

/// The `.lrc` files beside `audio` that exist, `.LRC` too: older Windows tools
/// write it in capitals, and on a case-sensitive file system (Linux) it was
/// missed — and `save_lyrics_lrc` then wrote a second one beside it.
fn existing_sidecars(audio: &std::path::Path) -> impl Iterator<Item = std::path::PathBuf> {
    [sidecar_path(audio), audio.with_extension("LRC")].into_iter().filter(|p| p.is_file())
}

/// A `.lrc` file is lyrics, never this big.
const SIDECAR_MAX_BYTES: u64 = 1 << 20;

/// The lyrics in the `.lrc` file beside `audio`, if there is one with any
/// text: UTF-8 (a BOM dropped), UTF-16 with its BOM (Notepad's "Unicode"),
/// else Windows-1252 — older LRC files are seldom UTF-8.
/// Each spelling in turn: an empty `.lrc` hid a `.LRC` with lyrics.
pub fn read_sidecar(audio: &std::path::Path) -> Option<String> {
    existing_sidecars(audio).find_map(|path| {
        if std::fs::metadata(&path).ok()?.len() > SIDECAR_MAX_BYTES {
            return None;
        }
        let text = decode_text(&std::fs::read(&path).ok()?);
        (!text.trim().is_empty()).then_some(text)
    })
}

fn decode_text(bytes: &[u8]) -> String {
    let utf16 = |be: bool| {
        let units = bytes[2..].as_chunks::<2>().0.iter().map(|&c| if be { u16::from_be_bytes(c) } else { u16::from_le_bytes(c) });
        char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}')).collect()
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, ..] => utf16(false),
        [0xFE, 0xFF, ..] => utf16(true),
        _ => match std::str::from_utf8(bytes) {
            Ok(s) => s.to_string(),
            Err(_) => bytes.iter().map(|&b| crate::playlist::windows_1252(b)).collect(),
        },
    }
}

/// Save LRCLIB's `text` as the `.lrc` beside `audio` (config.json
/// `save_lyrics_lrc`), so other players find it too: synced lyrics only (an
/// LRC file is timed lines), and never over a file already there. A failure
/// (a read-only folder, a share) is said once a session (`take_save_note`), and
/// that folder is not tried again.
pub fn save_sidecar(audio: &std::path::Path, text: &str) {
    if !parse_lyrics(text).is_synced() || existing_sidecars(audio).next().is_some() {
        return;
    }
    // A folder that refused once (read-only, a share) is not asked again
    // this session: each try is a failed write, slow over SMB.
    static REFUSED: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());
    let folder = audio.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
    if REFUSED.lock().is_ok_and(|r| r.contains(&folder)) {
        return;
    }
    let path = sidecar_path(audio);
    if let Err(e) = crate::playlist::write_atomic(&path, text.as_bytes()) {
        if let Ok(mut r) = REFUSED.lock() {
            r.push(folder);
        }
        use std::sync::atomic::{AtomicBool, Ordering};
        static SAID: AtomicBool = AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            if let Ok(mut n) = SAVE_NOTE.lock() {
                *n = Some(format!("lyrics not saved beside the track: {e}"));
            }
        }
    }
}

/// `save_sidecar`'s failure, for the status line (it runs on a worker).
static SAVE_NOTE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn take_save_note() -> Option<String> {
    SAVE_NOTE.lock().ok().and_then(|mut n| n.take())
}

/// "synced · LRCLIB" — the kind and origin of the lyrics on screen.
pub fn source_line(lyrics: &Lyrics, source: Option<LyricsSource>) -> String {
    let kind = if lyrics.is_synced() { "synced" } else { "plain" };
    match source {
        Some(s) => format!("{kind} · {}", s.label()),
        None => kind.to_string(),
    }
}

/// Per-track lyrics sync offsets, remembered across sessions in
/// `lyrics_offsets.json` (path → seconds). A sync fix belongs to the track it
/// was made for: LRCLIB timings are off by a different amount for every file.
/// Loaded on first use; written on every change (a key press, so rare).
#[derive(Default)]
pub struct OffsetStore {
    map: Option<std::collections::HashMap<String, f64>>,
}

impl OffsetStore {
    fn file() -> Option<std::path::PathBuf> {
        // Tests must never read or write the user's real file.
        if cfg!(test) {
            return None;
        }
        crate::playlist::keet_config_dir().map(|d| d.join("lyrics_offsets.json"))
    }

    fn map(&mut self) -> &mut std::collections::HashMap<String, f64> {
        self.map.get_or_insert_with(|| {
            Self::file()
                .and_then(|f| crate::config::read_json_text(&f))
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default()
        })
    }

    /// The saved offset for `track` (0 when none).
    pub fn get(&mut self, track: &std::path::Path) -> f64 {
        self.map().get(&*track.to_string_lossy()).copied().unwrap_or(0.0)
    }

    /// Remember `secs` for `track` and save. Zero forgets it.
    pub fn set(&mut self, track: &std::path::Path, secs: f64) {
        set_offset(self.map(), &track.to_string_lossy(), secs);
        if let (Some(f), Some(map)) = (Self::file(), self.map.as_ref()) {
            if let Some(dir) = f.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(json) = serde_json::to_string_pretty(map) {
                // Not reported on failure: the whole map is written again at
                // the next offset change, and the offset still applies now.
                let _ = crate::playlist::write_atomic(&f, json.as_bytes());
            }
        }
    }
}

/// Record an offset, rounded to the 0.1 s it is shown at; zero removes the
/// entry so the file only lists tracks that need a fix.
fn set_offset(map: &mut std::collections::HashMap<String, f64>, key: &str, secs: f64) {
    let secs = (secs * 10.0).round() / 10.0;
    if secs == 0.0 {
        map.remove(key);
    } else {
        map.insert(key.to_string(), secs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrclib_lyrics_are_saved_beside_the_track_only_when_synced_and_new() {
        let dir = std::env::temp_dir().join(format!("keet-sidecar-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("t.flac");
        save_sidecar(&audio, "just words\nno timing");
        assert!(!sidecar_path(&audio).exists(), "plain lyrics are not an LRC file");
        save_sidecar(&audio, "[00:01.00]one");
        assert_eq!(read_sidecar(&audio).as_deref(), Some("[00:01.00]one"));
        save_sidecar(&audio, "[00:01.00]other");
        assert_eq!(read_sidecar(&audio).as_deref(), Some("[00:01.00]one"), "never over a file there");
        // One in capitals (an old Windows tool's) is read, and none is
        // written beside it.
        let caps = dir.join("u.flac");
        std::fs::write(caps.with_extension("LRC"), "[00:01.00]caps").unwrap();
        assert_eq!(read_sidecar(&caps).as_deref(), Some("[00:01.00]caps"));
        save_sidecar(&caps, "[00:01.00]other");
        assert_eq!(read_sidecar(&caps).as_deref(), Some("[00:01.00]caps"));
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.starts_with("u.")).collect();
        assert_eq!(names.len(), 1, "{names:?}");
        // An empty `.lrc` (a failed download) does not hide a `.LRC` with
        // lyrics — only where the two are different files (Linux).
        let both = dir.join("v.flac");
        std::fs::write(both.with_extension("LRC"), "[00:01.00]caps").unwrap();
        std::fs::write(both.with_extension("lrc"), "").unwrap();
        if std::fs::read(both.with_extension("LRC")).unwrap().is_empty() {
            eprintln!("case-insensitive file system: one file, nothing to check");
        } else {
            assert_eq!(read_sidecar(&both).as_deref(), Some("[00:01.00]caps"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_that_refused_a_save_is_not_asked_again() {
        // A read-only share: a failed write on every track change.
        let dir = std::env::temp_dir().join(format!("keet-sidecar-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let audio = dir.join("a.flac");
        save_sidecar(&audio, "[00:01.00]one"); // the folder is not there: fails
        std::fs::create_dir_all(&dir).unwrap();
        save_sidecar(&audio, "[00:01.00]one");
        assert!(!sidecar_path(&audio).exists(), "not tried again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lyrics_are_the_same_when_they_read_the_same() {
        // A tagger writes both; the file has headers and spaces at line ends.
        let tag = "[00:01.00]One\n[00:02.50]Two";
        let file = "[ar:Someone]\r\n[ti:Song]\r\n[00:01.00] One  \r\n[00:02.50]Two \r\n";
        assert!(same_lyrics(tag, file));
        assert!(!same_lyrics(tag, "[00:01.00]One\n[00:02.60]Two"), "another timing");
        assert!(same_lyrics("One\nTwo", "One \r\n Two\r\n"));
        assert!(!same_lyrics("One\nTwo", "[00:01.00]One\n[00:02.00]Two"), "plain and synced");
        assert!(same_lyrics("One\nTwo", "\nOne\nTwo\n\n"), "blank lines at the ends");
        assert!(!same_lyrics("One\nTwo", "One\n\nTwo"), "one between verses is not");
    }

    #[test]
    fn a_lrc_beside_the_track_is_read_in_its_encoding() {
        let dir = std::env::temp_dir().join(format!("keet-sidecar-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // "01. Mr. Brightside.flac": only the extension is replaced.
        let audio = dir.join("01. Mr. Brightside.flac");
        assert_eq!(sidecar_path(&audio), dir.join("01. Mr. Brightside.lrc"));
        assert_eq!(read_sidecar(&audio), None, "no file");
        let lrc = "[00:01.00]Café “one”";
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("utf-8", lrc.as_bytes().to_vec()),
            ("utf-8 bom", [&[0xEF, 0xBB, 0xBF][..], lrc.as_bytes()].concat()),
            ("utf-16le", [vec![0xFF, 0xFE], lrc.encode_utf16().flat_map(u16::to_le_bytes).collect()].concat()),
            ("utf-16be", [vec![0xFE, 0xFF], lrc.encode_utf16().flat_map(u16::to_be_bytes).collect()].concat()),
            ("windows-1252", b"[00:01.00]Caf\xe9 \x93one\x94".to_vec()),
        ];
        for (name, bytes) in cases {
            std::fs::write(sidecar_path(&audio), bytes).unwrap();
            assert_eq!(read_sidecar(&audio).as_deref(), Some(lrc), "{name}");
        }
        // An empty one is no lyrics (it would hide the tags' and LRCLIB's).
        std::fs::write(sidecar_path(&audio), " \n").unwrap();
        assert_eq!(read_sidecar(&audio), None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(source_line(&parse_lyrics(lrc), Some(LyricsSource::File)), "synced · file");
    }

    #[test]
    fn a_busy_or_failed_lrclib_is_asked_once_more_after_a_wait() {
        use std::cell::RefCell;
        use std::time::Duration;
        // 429 with Retry-After: 2 s → waits 2 s, then asks again.
        let answers = RefCell::new(vec![Lookup::Found("[00:01.00]hi".to_string()), Lookup::Busy(Some(2))]);
        let waited = RefCell::new(Vec::new());
        let got = fetch_with_retry(|| answers.borrow_mut().pop().unwrap(), |d| { waited.borrow_mut().push(d); true });
        assert!(matches!(got, Lookup::Found(_)));
        assert_eq!(*waited.borrow(), [Duration::from_secs(2)]);
        // A failure with no hint: a few seconds; a huge Retry-After: capped.
        for (first, wait) in [(Lookup::Failed, 5), (Lookup::Busy(None), 10), (Lookup::Busy(Some(3600)), 30)] {
            let answers = RefCell::new(vec![Lookup::NotFound, first]);
            let waited = RefCell::new(Vec::new());
            let got = fetch_with_retry(|| answers.borrow_mut().pop().unwrap(), |d| { waited.borrow_mut().push(d); true });
            assert!(matches!(got, Lookup::NotFound));
            assert_eq!(*waited.borrow(), [Duration::from_secs(wait)]);
        }
        // Once only: still failing, it says so.
        let calls = RefCell::new(0);
        let got = fetch_with_retry(|| { *calls.borrow_mut() += 1; Lookup::<String>::Busy(Some(1)) }, |_| true);
        assert!(matches!(got, Lookup::Busy(_)) && *calls.borrow() == 2);
        // The track changed during the wait: no second request.
        let calls = RefCell::new(0);
        fetch_with_retry(|| { *calls.borrow_mut() += 1; Lookup::<String>::Failed }, |_| false);
        assert_eq!(*calls.borrow(), 1);
        // Found or not found: answered, no retry.
        let calls = RefCell::new(0);
        fetch_with_retry(|| { *calls.borrow_mut() += 1; Lookup::<String>::NotFound }, |_| panic!("no wait"));
        assert_eq!(*calls.borrow(), 1);
    }

    #[test]
    fn long_or_lookalike_names_each_get_their_own_lyrics_file() {
        let dir = std::env::temp_dir().join(format!("keet-lyrics-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let disk = LyricsDisk::at(dir.clone());
        // A classical title far past a file name's 255 bytes, multi-byte too.
        let title = "Symphonie Nr. 9 d-Moll op. 125 „Chorfantasie“ – IV. Presto – Allegro assai – ".repeat(4);
        disk.store("Wiener Philharmoniker, Karl Böhm", &title, Some(1500), Some("words"));
        assert_eq!(disk.load("Wiener Philharmoniker, Karl Böhm", &title, Some(1500)), Some(Some("words".into())));
        for e in std::fs::read_dir(&dir).unwrap() {
            assert!(e.unwrap().file_name().len() <= 200, "a file name the file system takes");
        }
        // Two artists that clean up to the same text: two files.
        disk.store("AC/DC", "Song", None, Some("one"));
        disk.store("AC:DC", "Song", None, Some("two"));
        assert_eq!(disk.load("AC/DC", "Song", None), Some(Some("one".into())));
        assert_eq!(disk.load("AC:DC", "Song", None), Some(Some("two".into())));
        // A dot in a title is not an extension: "Mr. Brightside" and
        // "Mr. Jones" are two files.
        disk.store("X", "Mr. Brightside", None, Some("one"));
        disk.store("X", "Mr. Jones", None, None);
        assert_eq!(disk.load("X", "Mr. Brightside", None), Some(Some("one".into())));
        assert_eq!(disk.load("X", "Mr. Jones", None), Some(None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tag_with_a_control_character_is_still_kept_on_disk() {
        // Windows refuses control characters in a file name: a title with a
        // tab in it was never stored, and asked LRCLIB on every launch.
        let dir = std::env::temp_dir().join(format!("keet-lyrics-ctl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let disk = LyricsDisk::at(dir.clone());
        disk.store("Band\u{7}", "Song\tTitle", Some(200), Some("words"));
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.chars().any(char::is_control)), "{names:?}");
        assert_eq!(disk.load("Band\u{7}", "Song\tTitle", Some(200)), Some(Some("words".to_string())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lrclib_answers_are_kept_on_disk_found_for_good_missing_for_a_day() {
        let dir = std::env::temp_dir().join(format!("keet-lyrics-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let disk = LyricsDisk::at(dir.clone());
        assert_eq!(disk.load("A/C", "Song?", Some(200)), None, "nothing known yet");
        disk.store("A/C", "Song?", Some(200), Some("[00:01.00]la"));
        assert_eq!(disk.load("A/C", "Song?", Some(200)), Some(Some("[00:01.00]la".to_string())));
        // Another duration is another version.
        assert_eq!(disk.load("A/C", "Song?", Some(201)), None);
        // Not on LRCLIB: remembered, but only for a day (lyrics get added).
        disk.store("B", "Other", None, None);
        assert_eq!(disk.load("B", "Other", None), Some(None));
        let marker = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "none")).expect("the missing marker");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3600);
        std::fs::File::options().write(true).open(&marker).unwrap().set_modified(old).unwrap();
        assert_eq!(disk.load("B", "Other", None), None, "a day on, asked again");
        // Found later: the marker goes.
        disk.store("B", "Other", None, Some("words"));
        assert_eq!(disk.load("B", "Other", None), Some(Some("words".to_string())));
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn source_line_names_kind_and_origin() {
        let synced = parse_lyrics("[00:01.00]hi");
        let plain = parse_lyrics("hi\nthere");
        assert_eq!(source_line(&synced, Some(LyricsSource::Lrclib)), "synced · LRCLIB");
        assert_eq!(source_line(&plain, Some(LyricsSource::Embedded)), "plain · embedded");
        assert_eq!(source_line(&plain, None), "plain");
    }

    #[test]
    fn offsets_are_rounded_and_zero_forgets_the_track() {
        let mut map = std::collections::HashMap::new();
        set_offset(&mut map, "a.flac", 0.30000000000000004);
        assert_eq!(map.get("a.flac"), Some(&0.3));
        set_offset(&mut map, "a.flac", 0.0000001);
        assert!(map.is_empty(), "a track back at zero leaves no entry");
    }

    #[test]
    fn the_offset_store_is_per_track() {
        let mut store = OffsetStore::default();
        let (a, b) = (std::path::Path::new("/m/a.flac"), std::path::Path::new("/m/b.flac"));
        store.set(a, 1.5);
        assert_eq!(store.get(a), 1.5);
        assert_eq!(store.get(b), 0.0);
    }

    fn times(raw: &str) -> Vec<f64> {
        match parse_lyrics(raw) {
            Lyrics::Synced(lines) => lines.iter().map(|l| l.time).collect(),
            Lyrics::Plain(_) => panic!("expected synced lyrics"),
        }
    }

    fn close(a: &[f64], b: &[f64]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
    }

    #[test]
    fn network_lookups_are_kept_except_failures() {
        // Every play of a track re-asked LRCLIB — and every launch: it
        // rate-limited them. Found and definitively missing answers are kept
        // for the session and on disk; a failure (offline, busy) is retried
        // next time.
        let dir = std::env::temp_dir().join(format!("keet-lookup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let disk = LyricsDisk::at(dir.clone());
        let track = ("A", "B", Some(100));
        let no_wait = |_| true;
        let cache = LookupCache::default();
        let mut calls = 0;
        for _ in 0..3 {
            let got = lookup_lrclib(&cache, Some(&disk), track, || { calls += 1; Lookup::Found("words".to_string()) }, no_wait);
            assert!(matches!(got, Lookup::Found(ref w) if w == "words"));
        }
        assert_eq!(calls, 1, "asked once");
        // A later run (a fresh session cache) finds it on disk.
        let mut calls = 0;
        let got = lookup_lrclib(&LookupCache::default(), Some(&disk), track, || { calls += 1; Lookup::Failed }, no_wait);
        assert!(matches!(got, Lookup::Found(_)) && calls == 0, "not asked again after a restart");
        // A definite miss is kept too.
        let mut calls = 0;
        for _ in 0..3 {
            let got = lookup_lrclib(&cache, Some(&disk), ("A", "Missing", None), || { calls += 1; Lookup::NotFound }, no_wait);
            assert!(matches!(got, Lookup::NotFound));
        }
        assert_eq!(calls, 1);
        // Failures and busy answers are not: asked again (twice each: the retry).
        let mut calls = 0;
        for _ in 0..3 {
            let got = lookup_lrclib(&cache, Some(&disk), ("A", "Offline", None), || { calls += 1; Lookup::Busy(Some(1)) }, no_wait);
            assert!(matches!(got, Lookup::Busy(_)));
        }
        assert_eq!(calls, 6, "retried every time, never remembered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lrc_timestamps_in_every_common_form() {
        assert!(close(&times("[01:02]a\n[01:02.50]b\n[01:02.5]c"), &[62.0, 62.5, 62.5]));
        // [mm:ss:xx] — centiseconds after a colon, as some editors write them.
        // It was read as HOURS:minutes:seconds: a line meant for 1:02 showed
        // after an hour.
        assert!(close(&times("[01:02:50]a"), &[62.5]));
        // A real hours form keeps its decimal point on the seconds.
        assert!(close(&times("[01:00:02.25]a"), &[3602.25]));
        // Edge cases: the tags may carry no lines worth showing.
        assert!(close(&times("[00:01.00][00:03.00]chorus"), &[1.0, 3.0]));
    }

    #[test]
    fn lrc_offset_tag_shifts_every_line() {
        // [offset:+N] is in milliseconds; positive shows lyrics EARLIER.
        assert!(close(&times("[offset:+500]\n[00:10.00]a\n[00:20.00]b"), &[9.5, 19.5]));
        assert!(close(&times("[offset:-250]\n[00:10.00]a"), &[10.25]));
        // Never before the start of the track.
        assert!(close(&times("[offset:2000]\n[00:01.00]a"), &[0.0]));
    }

    #[test]
    fn lyric_lines_are_stripped_of_control_characters() {
        let synced = parse_lyrics("[00:01.00]hi\x1B]52;c;cGF3bmVk\x07there");
        assert!(!synced.line_text(0).contains('\x1B'), "{:?}", synced.line_text(0));
        let plain = parse_lyrics("verse\x1B[2J one\nverse two");
        assert!(!plain.line_text(0).contains('\x1B'));
        assert_eq!(plain.line_text(1), "verse two");
    }

    #[test]
    fn lyric_lines_keep_no_spaces_at_their_ends() {
        // LRCLIB's usual form: a space after the timestamp.
        let synced = parse_lyrics("[00:17.12] I feel it 
[00:19.00]  so   loud");
        assert_eq!(synced.line_text(0), "I feel it");
        assert_eq!(synced.line_text(1), "so   loud", "the spacing inside is kept");
        let plain = parse_lyrics("  first 
second	");
        assert_eq!(plain.line_text(0), "first");
        assert_eq!(plain.line_text(1), "second");
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;

    #[test]
    fn enhanced_lrc_word_timing_is_not_shown() {
        let l = parse_lyrics("[00:12.00]<00:12.00> low <00:12.40> tide <00:12.90> at Ferrow");
        assert_eq!(l.line_text(0), "low tide at Ferrow");
        // Angle brackets that are not timestamps stay.
        assert_eq!(strip_word_tags("a <b> c"), "a <b> c");
        assert_eq!(strip_word_tags("x < 3"), "x < 3");
        assert_eq!(strip_word_tags("two  spaces"), "two  spaces", "untagged text is left as written");
    }

    /// Live HTTPS check against LRCLIB. Ignored by default (needs network);
    /// run with `cargo test -- --ignored --nocapture`.
    ///
    /// Worth keeping: the ureq timeout config is a known landmine here. Using
    /// `timeout_global` makes the global timer trip during TCP/TLS setup, so
    /// every HTTPS call fails *at runtime* while compiling perfectly. Run this
    /// after any bump to ureq, native-tls, or the TLS stack.
    #[test]
    #[ignore = "requires network"]
    fn lrclib_fetch_completes_over_tls() {
        // Step 1: a plain HTTPS GET with the error NOT swallowed. fetch_lrclib
        // discards the cause via `.ok()?`, so a TLS failure and a 404 both look
        // identical (None) — this separates them.
        let url = "https://lrclib.net/api/get?artist_name=Radiohead&track_name=Creep&duration=238";
        let started = std::time::Instant::now();
        match http_agent().get(url).call() {
            Ok(resp) => {
                println!("[1] raw GET ok in {:?}, status={}", started.elapsed(), resp.status());
            }
            Err(e) => {
                println!("[1] raw GET FAILED in {:?}: {e}", started.elapsed());
                println!("    (a TLS/proxy/firewall problem shows up here)");
            }
        }

        // Step 2: is it LRCLIB specifically, or all outbound TLS? If this
        // succeeds while step 1 fails, the TLS stack is fine and lrclib.net is
        // being blocked or resolved wrong.
        match http_agent().get("https://example.com").call() {
            Ok(r) => println!("[2] control host ok, status={}", r.status()),
            Err(e) => println!("[2] control host FAILED: {e}  <-- outbound TLS is broken, not LRCLIB"),
        }

        let started = std::time::Instant::now();
        let got = match fetch_lrclib("Radiohead", "Creep", Some(238)) {
            Lookup::Found(l) => Some(l),
            _ => None,
        };
        let elapsed = started.elapsed();
        println!("[3] fetch_lrclib in {:?}, some={}", elapsed, got.is_some());

        // A TLS/timeout misconfiguration shows up as an instant None (the
        // handshake never completes), so assert we actually got lyrics back.
        let lyrics = got.expect("LRCLIB returned nothing — check TLS/timeout config");
        assert!(!lyrics.trim().is_empty(), "empty lyrics body");
        let parsed = parse_lyrics(&lyrics);
        assert!(parsed.line_count() > 0, "parsed to zero lines");
    }
}
