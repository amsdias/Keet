/// LRC lyrics parser and synced lyrics state.
///
/// Supports:
/// - Plain (unsynced) lyrics — just text lines
/// - LRC (synced) lyrics — `[MM:SS.xx]Line text` with auto-scroll by playback position

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
    let has_timestamps = raw.lines().any(|line| parse_lrc_timestamp(line).is_some());

    if has_timestamps {
        let mut lines: Vec<LrcLine> = Vec::new();
        for line in raw.lines() {
            if let Some((time, text)) = parse_lrc_timestamp(line) {
                lines.push(LrcLine { time, text });
            }
            // Skip non-timestamped lines (metadata like [ar:Artist], [ti:Title], etc.)
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

/// Parse a single LRC line like `[01:23.45]Some text` or `[01:23]Text`.
/// Returns (seconds, text) if valid.
fn parse_lrc_timestamp(line: &str) -> Option<(f64, String)> {
    let line = line.trim();
    if !line.starts_with('[') { return None; }
    let close = line.find(']')?;
    let inside = &line[1..close];
    let text = line[close + 1..].to_string();

    // Parse MM:SS.xx or MM:SS
    let parts: Vec<&str> = inside.split(':').collect();
    if parts.len() != 2 { return None; }

    let minutes: f64 = parts[0].parse().ok()?;
    let seconds: f64 = parts[1].parse().ok()?;

    Some((minutes * 60.0 + seconds, text))
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

    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .build();
    let agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(std::time::Duration::from_secs(8)))
        .user_agent("Keet Audio Player (https://github.com)")
        .build()
        .new_agent();

    let response = agent.get(&url).call().ok()?;

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
