//! Album cover resolution and rendering.
//!
//! Priority: embedded tag → sidecar file → cached remote → remote fetch.
//! Remote fetches are cached to the track's folder as
//! `{artist} - {album}.cover.jpg` so the mix of albums in a single folder
//! doesn't collide with standard `cover.jpg` sidecar conventions.
//!
//! Rendering picks between the Kitty graphics protocol (native pixel
//! resolution on Ghostty/Kitty/WezTerm) and half-block truecolor Unicode
//! (20×20 pixel fallback for everything else).

use std::path::{Path, PathBuf};
use std::fs::File;
use std::fmt::Write as _;
use std::io::Cursor;
use std::sync::OnceLock;
use std::time::Duration;

use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Pixel dimensions used by the half-block renderer. Each terminal column is
/// 1 pixel wide and each row covers 2 pixels tall (upper/lower half-block).
pub const COVER_COLS: u32 = 20;
pub const COVER_ROWS: u32 = 10;
const HALF_BLOCK_W: u32 = COVER_COLS;
const HALF_BLOCK_H: u32 = COVER_ROWS * 2;
/// Square target for Kitty-protocol transmissions. Chosen large enough for
/// good quality on high-DPI terminals but small enough to keep PNG/base64
/// transmission cost trivial.
const KITTY_SIZE: u32 = 320;
/// Kitty image ID we reserve. Re-transmitting with the same ID replaces.
const KITTY_IMAGE_ID: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphicsProtocol {
    Kitty,
    HalfBlock,
}

/// Detect once per process. Cheap env var reads, cached for later.
pub fn detect_protocol() -> GraphicsProtocol {
    static CACHED: OnceLock<GraphicsProtocol> = OnceLock::new();
    *CACHED.get_or_init(|| {
        if let Ok(tp) = std::env::var("TERM_PROGRAM") {
            let lower = tp.to_ascii_lowercase();
            if lower == "ghostty" || lower == "wezterm" {
                return GraphicsProtocol::Kitty;
            }
        }
        if let Ok(term) = std::env::var("TERM") {
            if term.contains("kitty") {
                return GraphicsProtocol::Kitty;
            }
        }
        if std::env::var("KITTY_WINDOW_ID").is_ok() {
            return GraphicsProtocol::Kitty;
        }
        GraphicsProtocol::HalfBlock
    })
}

/// Decoded cover, shape depending on the detected rendering protocol.
pub enum CoverImage {
    /// Raw RGB pixels at exactly HALF_BLOCK_W × HALF_BLOCK_H.
    HalfBlock { width: u32, height: u32, pixels: Vec<u8> },
    /// PNG bytes ready for Kitty-protocol transmission (base64-encoded at render time).
    Kitty { png: Vec<u8> },
}

/// Escape sequence that removes any placement of our reserved image ID.
/// Safe to emit even when no image is currently on screen.
pub fn kitty_clear_escape() -> String {
    format!("\x1B_Ga=d,d=i,i={},q=2\x1B\\", KITTY_IMAGE_ID)
}

/// Try local sources only: embedded tag, sidecar file, on-disk cache.
/// Returns None if no local cover exists (callers can then fall back to
/// `resolve_remote`, which is gated by track-change generation counters).
pub fn resolve_local(
    track_path: &Path,
    artist: Option<&str>,
    album: Option<&str>,
) -> Option<CoverImage> {
    if let Some(bytes) = read_embedded(track_path) {
        return decode_and_resize(&bytes);
    }
    if let Some(bytes) = read_sidecar(track_path) {
        return decode_and_resize(&bytes);
    }
    let cache_path = cache_path_for(track_path, artist, album);
    if let Some(ref p) = cache_path {
        if let Ok(bytes) = std::fs::read(p) {
            return decode_and_resize(&bytes);
        }
    }
    None
}

/// Fetch a cover from iTunes Search and persist it to the on-disk cache for
/// next time. Requires both artist and album — returns None otherwise.
pub fn resolve_remote(
    track_path: &Path,
    artist: &str,
    album: &str,
) -> Option<CoverImage> {
    let bytes = fetch_itunes(artist, album)?;
    if let Some(p) = cache_path_for(track_path, Some(artist), Some(album)) {
        let _ = std::fs::write(&p, &bytes);
    }
    decode_and_resize(&bytes)
}

fn read_embedded(track_path: &Path) -> Option<Vec<u8>> {
    let file = File::open(track_path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = track_path.extension() {
        hint.with_extension(ext.to_str().unwrap_or(""));
    }
    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?;

    if let Some(rev) = probed.format.metadata().current() {
        if let Some(v) = rev.visuals().first() {
            return Some(v.data.to_vec());
        }
    }
    if let Some(meta) = probed.metadata.get() {
        if let Some(rev) = meta.current() {
            if let Some(v) = rev.visuals().first() {
                return Some(v.data.to_vec());
            }
        }
    }
    None
}

fn read_sidecar(track_path: &Path) -> Option<Vec<u8>> {
    let parent = track_path.parent()?;
    const NAMES: &[&str] = &[
        "cover.jpg", "cover.jpeg", "cover.png", "cover.webp",
        "folder.jpg", "folder.jpeg", "folder.png",
        "front.jpg", "front.jpeg", "front.png",
        "album.jpg", "album.jpeg", "album.png",
        "Cover.jpg", "Folder.jpg", "Front.jpg",
    ];
    for name in NAMES {
        let candidate = parent.join(name);
        if let Ok(bytes) = std::fs::read(&candidate) {
            return Some(bytes);
        }
    }
    None
}

fn cache_path_for(track_path: &Path, artist: Option<&str>, album: Option<&str>) -> Option<PathBuf> {
    let parent = track_path.parent()?;
    let a = sanitize_fs(artist?);
    let al = sanitize_fs(album?);
    if a.is_empty() || al.is_empty() { return None; }
    Some(parent.join(format!("{} - {}.cover.jpg", a, al)))
}

fn sanitize_fs(s: &str) -> String {
    let mut out: String = s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            _ => c,
        })
        .collect();
    // Windows trims trailing dots/spaces from filenames; mirror that here.
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    out.trim().to_string()
}

fn fetch_itunes(artist: &str, album: &str) -> Option<Vec<u8>> {
    let query = format!("{} {}", artist, album);
    let url = format!(
        "https://itunes.apple.com/search?term={}&media=music&entity=album&limit=1",
        urlencoded(&query),
    );

    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .build();
    let agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_global(Some(Duration::from_secs(8)))
        .user_agent("Keet Audio Player (https://github.com)")
        .build()
        .new_agent();

    let response = agent.get(&url).call().ok()?;
    if response.status() != 200 {
        return None;
    }
    let text = response.into_body().read_to_string().ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;

    let art_url = json.get("results")?.as_array()?
        .first()?.get("artworkUrl100")?.as_str()?
        .to_string();

    // Upgrade thumbnail URL to the largest standard size iTunes serves.
    let big_url = art_url.replacen("100x100", "600x600", 1);
    let img_resp = agent.get(&big_url).call().ok()?;
    if img_resp.status() != 200 {
        return None;
    }
    img_resp.into_body().with_config().limit(8 * 1024 * 1024).read_to_vec().ok()
}

fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => {
                out.push('%');
                let _ = write!(out, "{:02X}", b);
            }
        }
    }
    out
}

fn decode_and_resize(bytes: &[u8]) -> Option<CoverImage> {
    let img = image::load_from_memory(bytes).ok()?;
    match detect_protocol() {
        GraphicsProtocol::Kitty => {
            let resized = img.resize_exact(KITTY_SIZE, KITTY_SIZE, image::imageops::FilterType::Lanczos3);
            let mut png: Vec<u8> = Vec::new();
            resized.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
            Some(CoverImage::Kitty { png })
        }
        GraphicsProtocol::HalfBlock => {
            let resized = img.resize_exact(HALF_BLOCK_W, HALF_BLOCK_H, image::imageops::FilterType::Lanczos3);
            let rgb = resized.to_rgb8();
            Some(CoverImage::HalfBlock {
                width: HALF_BLOCK_W,
                height: HALF_BLOCK_H,
                pixels: rgb.into_raw(),
            })
        }
    }
}

/// Render as half-block truecolor ANSI. Returns one String per terminal row,
/// each spanning `COVER_COLS` cells. Caller is responsible for horizontal
/// placement (prefix/suffix).
fn render_half_block(width: u32, height: u32, pixels: &[u8]) -> Vec<String> {
    let w = width as usize;
    let h = height as usize;
    let n_rows = h.div_ceil(2);
    let mut lines = Vec::with_capacity(n_rows);

    let mut y = 0;
    while y < h {
        let mut line = String::with_capacity(w * 32);
        let mut last_top: Option<(u8, u8, u8)> = None;
        let mut last_bot: Option<(u8, u8, u8)> = None;
        for x in 0..w {
            let top_idx = (y * w + x) * 3;
            let top = (pixels[top_idx], pixels[top_idx + 1], pixels[top_idx + 2]);
            let bot = if y + 1 < h {
                let bi = ((y + 1) * w + x) * 3;
                Some((pixels[bi], pixels[bi + 1], pixels[bi + 2]))
            } else {
                None
            };

            if last_top != Some(top) {
                let _ = write!(line, "\x1B[38;2;{};{};{}m", top.0, top.1, top.2);
                last_top = Some(top);
            }
            match bot {
                Some(b) if last_bot != Some(b) => {
                    let _ = write!(line, "\x1B[48;2;{};{};{}m", b.0, b.1, b.2);
                    last_bot = Some(b);
                }
                None => {
                    // Clear any background from previous cell on an odd last row.
                    if last_bot.is_some() {
                        line.push_str("\x1B[49m");
                        last_bot = None;
                    }
                }
                _ => {}
            }
            line.push('▀');
        }
        line.push_str("\x1B[0m");
        lines.push(line);
        y += 2;
    }
    lines
}

/// Solid black cells filling the cover slot. Used as a placeholder when no
/// cover is available so the banner layout doesn't shift while one loads or
/// for tracks without artwork.
pub fn placeholder_lines() -> Vec<String> {
    let cells = " ".repeat(COVER_COLS as usize);
    let line = format!("\x1B[48;2;0;0;0m{}\x1B[0m", cells);
    (0..COVER_ROWS).map(|_| line.clone()).collect()
}

/// Render the cover to a Vec of COVER_ROWS lines, each COVER_COLS wide.
/// For Kitty, line 0 carries the image-transmit escape plus blank spaces;
/// subsequent lines are blank spaces that the image overlays.
pub fn render(img: &CoverImage) -> Vec<String> {
    match img {
        CoverImage::HalfBlock { width, height, pixels } => {
            render_half_block(*width, *height, pixels)
        }
        CoverImage::Kitty { png } => render_kitty(png),
    }
}

fn render_kitty(png: &[u8]) -> Vec<String> {
    let cols = COVER_COLS as usize;
    let mut lines = Vec::with_capacity(COVER_ROWS as usize);
    let blank = " ".repeat(cols);
    let mut first = String::with_capacity(png.len() * 2);
    first.push_str(&kitty_transmit(png));
    first.push_str(&blank);
    lines.push(first);
    for _ in 1..COVER_ROWS {
        lines.push(blank.clone());
    }
    lines
}

fn kitty_transmit(png: &[u8]) -> String {
    let b64 = base64_encode(png);
    let chunk_size = 4096;
    let total = b64.len();
    let mut out = String::with_capacity(total + 256);
    let mut pos = 0;
    let mut first = true;
    while pos < total {
        let end = (pos + chunk_size).min(total);
        let is_last = end == total;
        out.push_str("\x1B_G");
        if first {
            // a=T transmit+display, f=100 PNG, i=<id> for replacement,
            // c=COLS,r=ROWS fit into our banner slot, C=1 don't move cursor,
            // q=2 suppress responses from the terminal.
            let _ = write!(
                out,
                "a=T,f=100,i={},c={},r={},C=1,q=2,m={}",
                KITTY_IMAGE_ID,
                COVER_COLS,
                COVER_ROWS,
                if is_last { 0 } else { 1 }
            );
            first = false;
        } else {
            let _ = write!(out, "m={}", if is_last { 0 } else { 1 });
        }
        out.push(';');
        out.push_str(&b64[pos..end]);
        out.push_str("\x1B\\");
        pos = end;
    }
    out
}

const B64: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let b0 = bytes[i];
        let b1 = bytes[i + 1];
        let b2 = bytes[i + 2];
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(B64[(((b1 & 0x0F) << 2) | (b2 >> 6)) as usize] as char);
        out.push(B64[(b2 & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let b0 = bytes[i];
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[((b0 & 0x03) << 4) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let b0 = bytes[i];
        let b1 = bytes[i + 1];
        out.push(B64[(b0 >> 2) as usize] as char);
        out.push(B64[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(B64[((b1 & 0x0F) << 2) as usize] as char);
        out.push('=');
    }
    out
}
