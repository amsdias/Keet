//! LRC lyrics parser and synced lyrics state.
//!
//! Supports:
//!
//! - Plain (unsynced) lyrics — just text lines
//! - LRC (synced) lyrics — `[MM:SS.xx]Line text` with auto-scroll by playback position

/// A single synced lyrics line: timestamp in seconds + text.
#[derive(Clone)]
pub struct LrcLine {
    pub time: f64,
    pub text: String,
}

/// Parsed lyrics: either synced (with timestamps) or plain text lines.
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
                // Lyrics are drawn straight into frame lines, and LRCLIB text
                // is user-submitted: strip control characters (ESC, CR, ...).
                let time = (time - offset_secs).max(0.0);
                lines.push(LrcLine { time, text: crate::ansi::sanitize_display(&strip_word_tags(&text)) });
            }
        }
        lines.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
        Lyrics::Synced(lines)
    } else {
        let lines: Vec<String> = raw.lines()
            .map(crate::ansi::sanitize_display)
            .collect();
        Lyrics::Plain(lines)
    }
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
/// decimal point; `[01:02:50]` used to be read as one HOUR and two minutes.
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
/// request — the per-fetch construction showed up as lingering
/// Security.framework allocations on macOS.
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
}

/// Session cache of network lookups, keyed by query. Shared with the worker
/// threads that do the fetching. Each play of a track used to ask LRCLIB (and
/// iTunes) again — the same answer every time, and up to 15 s of waiting.
#[derive(Clone, Default)]
pub struct LookupCache<T: Clone = String> {
    map: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Option<T>>>>,
}

impl<T: Clone> LookupCache<T> {
    /// The remembered answer for `key`, or `fetch` it — remembering found and
    /// not-found answers, but not failures.
    pub fn get_or_fetch(&self, key: String, fetch: impl FnOnce() -> Lookup<T>) -> Option<T> {
        if let Some(hit) = self.known(&key) {
            return hit;
        }
        match fetch() {
            Lookup::Found(v) => {
                self.remember(key, Some(v.clone()));
                Some(v)
            }
            Lookup::NotFound => {
                self.remember(key, None);
                None
            }
            Lookup::Failed => None,
        }
    }

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

    let response = match http_agent().get(&url).call() {
        Ok(r) if r.status() == 200 => r,
        // LRCLIB answers 404 for a track it has no lyrics for.
        Err(ureq::Error::StatusCode(404)) => return Lookup::NotFound,
        _ => return Lookup::Failed,
    };
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


/// Where a track's lyrics came from, shown beside them ("synced · LRCLIB"):
/// LRCLIB is user-submitted, so its timing and text deserve less trust than
/// the file's own tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LyricsSource {
    Embedded,
    Lrclib,
}

impl LyricsSource {
    pub fn label(self) -> &'static str {
        match self {
            LyricsSource::Embedded => "embedded",
            LyricsSource::Lrclib => "LRCLIB",
        }
    }
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
                .and_then(|f| std::fs::read_to_string(f).ok())
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
    fn network_lookups_are_cached_except_failures() {
        // Every play of a track re-asked LRCLIB (or iTunes) — the same answer
        // each time, up to 15 s of waiting for it. Found and definitively
        // missing results are remembered for the session; a failure (offline,
        // timeout) is retried next time.
        let cache = LookupCache::default();
        let mut calls = 0;
        for _ in 0..3 {
            let got = cache.get_or_fetch("a\0b".into(), || { calls += 1; Lookup::Found("words".to_string()) });
            assert_eq!(got.as_deref(), Some("words"));
        }
        assert_eq!(calls, 1);

        let mut calls = 0;
        for _ in 0..3 {
            assert_eq!(cache.get_or_fetch("missing".into(), || { calls += 1; Lookup::NotFound }), None);
        }
        assert_eq!(calls, 1, "a definite miss is remembered too");

        let mut calls = 0;
        for _ in 0..3 {
            assert_eq!(cache.get_or_fetch("offline".into(), || { calls += 1; Lookup::Failed }), None);
        }
        assert_eq!(calls, 3, "failures are retried");
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
