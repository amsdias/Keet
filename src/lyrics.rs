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
}

/// Parse raw lyrics text into a Lyrics struct.
/// Detects LRC format by looking for `[MM:SS` patterns.
pub fn parse_lyrics(raw: &str) -> Lyrics {
    // Check if this looks like LRC (at least one timestamp line)
    let has_timestamps = raw.lines().any(|line| !parse_lrc_line(line).is_empty());

    if has_timestamps {
        let mut lines: Vec<LrcLine> = Vec::new();
        for line in raw.lines() {
            // A line may carry several timestamps sharing the same text.
            // Non-timestamped lines (metadata like [ar:Artist]) yield nothing.
            for (time, text) in parse_lrc_line(line) {
                lines.push(LrcLine { time, text });
            }
        }
        lines.sort_by(|a, b| a.time.partial_cmp(&b.time).unwrap_or(std::cmp::Ordering::Equal));
        Lyrics::Synced(lines)
    } else {
        let lines: Vec<String> = raw.lines()
            .map(|l| l.to_string())
            .collect();
        Lyrics::Plain(lines)
    }
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

/// Parse the inside of an LRC time tag (`MM:SS`, `MM:SS.xx`, or `HH:MM:SS.xx`) to seconds.
fn parse_lrc_time(inside: &str) -> Option<f64> {
    let parts: Vec<&str> = inside.split(':').collect();
    match parts.as_slice() {
        [m, s] => {
            let minutes: f64 = m.parse().ok()?;
            let seconds: f64 = s.parse().ok()?;
            Some(minutes * 60.0 + seconds)
        }
        [h, m, s] => {
            let hours: f64 = h.parse().ok()?;
            let minutes: f64 = m.parse().ok()?;
            let seconds: f64 = s.parse().ok()?;
            Some(hours * 3600.0 + minutes * 60.0 + seconds)
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
            let tls = ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build();
            ureq::Agent::config_builder()
                .tls_config(tls)
                .timeout_connect(Some(std::time::Duration::from_secs(5)))
                .timeout_recv_response(Some(std::time::Duration::from_secs(15)))
                .timeout_recv_body(Some(std::time::Duration::from_secs(15)))
                .user_agent("Keet Audio Player (https://github.com/amsdias/rust_music_player)")
                .build()
                .new_agent()
        })
        .clone() // Agent is an Arc handle — cloning shares the pool/TLS context
}

/// Fetch lyrics from LRCLIB (free, no API key, ~3M entries).
/// Prefers synced (LRC) lyrics over plain.
/// Returns raw lyrics text or None on failure/not found.
pub fn fetch_lrclib(artist: &str, title: &str, duration_secs: Option<u32>) -> Option<String> {
    let mut url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        urlencod(artist),
        urlencod(title),
    );
    if let Some(dur) = duration_secs {
        url.push_str(&format!("&duration={}", dur));
    }

    let response = http_agent().get(&url).call().ok()?;

    if response.status() != 200 {
        return None;
    }

    let text = response.into_body().read_to_string().ok()?;
    let body: serde_json::Value = serde_json::from_str(&text).ok()?;

    // Prefer syncedLyrics (LRC format) over plainLyrics
    if let Some(synced) = body.get("syncedLyrics").and_then(|v| v.as_str()) {
        if !synced.is_empty() {
            return Some(synced.to_string());
        }
    }
    if let Some(plain) = body.get("plainLyrics").and_then(|v| v.as_str()) {
        if !plain.is_empty() {
            return Some(plain.to_string());
        }
    }
    None
}

/// Minimal percent-encoding for URL query parameters.
fn urlencod(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}
