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
use std::sync::atomic::{AtomicU32, Ordering};

use image::ImageEncoder;
use std::time::Duration;

use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::formats::probe::Hint;

/// Cell footprint of the banner cover slot (Classic). Each terminal column is
/// 1 pixel wide and each row covers 2 pixels tall (upper/lower half-block), so
/// a slot of C×R cells decodes to C×2R pixels — see `CoverSize`.
pub const COVER_COLS: u32 = 20;
pub const COVER_ROWS: u32 = 10;
/// Square target for Kitty-protocol transmissions. Chosen large enough for
/// good quality on high-DPI terminals but small enough to keep PNG/base64
/// transmission cost trivial.
const KITTY_SIZE: u32 = 320;
/// Kitty image ID we reserve. Re-transmitting with the same ID replaces.
const KITTY_IMAGE_ID: u32 = 1;
/// Separate Kitty image id for the viz (cover uses id 1; avoid clobbering it).
const VIZ_IMAGE_ID: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GraphicsProtocol {
    Kitty,
    Iterm2,
    Sixel,
    HalfBlock,
}

/// Detect once per process. Cheap env var reads, cached for later.
pub fn detect_protocol() -> GraphicsProtocol {
    static CACHED: OnceLock<GraphicsProtocol> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let term_program = std::env::var("TERM_PROGRAM").ok();
        let term = std::env::var("TERM").ok();
        let lc_terminal = std::env::var("LC_TERMINAL").ok();
        protocol_from_env(
            term_program.as_deref(),
            term.as_deref(),
            std::env::var("KITTY_WINDOW_ID").is_ok(),
            lc_terminal.as_deref(),
            std::env::var("WT_SESSION").is_ok(),
            cfg!(windows),
        )
    })
}

/// Pure decision core of `detect_protocol`, parameterized for testability.
fn protocol_from_env(
    term_program: Option<&str>,
    term: Option<&str>,
    has_kitty_window_id: bool,
    lc_terminal: Option<&str>,
    has_wt_session: bool,
    is_windows: bool,
) -> GraphicsProtocol {
    if let Some(tp) = term_program {
        let lower = tp.to_ascii_lowercase();
        // On Windows, locally spawned processes talk to the terminal through
        // ConPTY, which drops the APC sequences the Kitty protocol rides on —
        // WezTerm itself supports Kitty graphics but never sees the bytes.
        // Half-block is plain SGR and survives any ConPTY version.
        if lower == "wezterm" && is_windows {
            return GraphicsProtocol::HalfBlock;
        }
        // WezTerm speaks Kitty too, prefer it for image-id-based replacement.
        if lower == "ghostty" || lower == "wezterm" {
            return GraphicsProtocol::Kitty;
        }
        if lower == "iterm.app" {
            return GraphicsProtocol::Iterm2;
        }
    }
    if let Some(term) = term {
        if term.contains("kitty") {
            return GraphicsProtocol::Kitty;
        }
        if term == "foot" || term == "foot-extra" || term == "mlterm" {
            return GraphicsProtocol::Sixel;
        }
    }
    if has_kitty_window_id {
        return GraphicsProtocol::Kitty;
    }
    if let Some(lc) = lc_terminal {
        if lc.eq_ignore_ascii_case("iterm2") {
            return GraphicsProtocol::Iterm2;
        }
    }
    // Windows Terminal sets WT_SESSION; v1.22+ supports sixel natively.
    if has_wt_session {
        return GraphicsProtocol::Sixel;
    }
    GraphicsProtocol::HalfBlock
}

/// Cell footprint of a cover slot.
///
/// Half-block and Sixel bake the target size in at DECODE time (raw pixels /
/// pre-encoded escape data), while Kitty and iTerm2 carry a PNG and state the
/// footprint at placement time. Carrying one value from decode through render
/// is what stops those two ever disagreeing — a mismatch paints the image
/// outside its reserved rows and the frame below it gets overwritten.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CoverSize {
    pub cols: u32,
    pub rows: u32,
}

impl CoverSize {
    /// Classic's banner slot.
    pub const CLASSIC: Self = Self { cols: COVER_COLS, rows: COVER_ROWS };
    /// Minimal's slot, vertically aligned with the SIGNAL column (label row
    /// through `mem` = 8 rows).
    pub const MINIMAL: Self = Self { cols: 18, rows: 8 };

    /// The slot a theme reserves. Minimal keeps its cover beside the identity
    /// block; Classic and Hi-Fi use the banner slot.
    pub fn for_theme(kind: crate::theme::ThemeKind) -> Self {
        match kind {
            crate::theme::ThemeKind::Minimal => Self::MINIMAL,
            _ => Self::CLASSIC,
        }
    }

    /// Sixel is pixel-exact, so its target is derived from the cell box at a
    /// conservative 10×20 px per cell (the Windows Terminal default font).
    fn sixel_px(&self) -> (u32, u32) {
        (self.cols * 10, self.rows * 20)
    }
}

/// Decoded cover, shape depending on the detected rendering protocol.
/// Every variant remembers the slot it was sized for.
pub enum CoverImage {
    /// Raw RGB pixels at exactly `size.cols` × `size.rows * 2`.
    HalfBlock { width: u32, height: u32, pixels: Vec<u8>, size: CoverSize },
    /// PNG bytes ready for Kitty-protocol transmission (base64-encoded at render time).
    Kitty { png: Vec<u8>, size: CoverSize },
    /// PNG bytes ready for iTerm2 inline image protocol (OSC 1337).
    Iterm2 { png: Vec<u8>, size: CoverSize },
    /// Pre-encoded Sixel escape data, ready to print verbatim.
    Sixel { data: String, size: CoverSize },
}

impl CoverImage {
    pub fn size(&self) -> CoverSize {
        match self {
            CoverImage::HalfBlock { size, .. }
            | CoverImage::Kitty { size, .. }
            | CoverImage::Iterm2 { size, .. }
            | CoverImage::Sixel { size, .. } => *size,
        }
    }
}

/// Whether the detected protocol places an image that PERSISTS in its cells.
///
/// Kitty, iTerm2 and Sixel all do: once transmitted, the image stays until
/// something overwrites those cells, so an unchanged frame can skip the
/// transmit entirely (and must skip erase-to-EOL over them). Half-block *is*
/// ordinary text, so it has to be repainted like any other content.
pub fn image_is_sticky() -> bool {
    !matches!(detect_protocol(), GraphicsProtocol::HalfBlock)
}

/// Rows that step past an already-placed image without drawing over it.
/// Used on frames where the cover hasn't changed — re-transmitting a PNG or
/// Sixel blob 20x/s through the terminal is exactly the cost the analysis
/// spectrogram's emit-on-change rule exists to avoid.
pub fn passive_lines(size: CoverSize) -> Vec<String> {
    (0..size.rows).map(|_| format!("\x1B[{}C", size.cols)).collect()
}

/// Escape sequence that removes any placement of our reserved image ID.
/// Safe to emit even when no image is currently on screen.
pub fn kitty_clear_escape() -> String {
    format!("\x1B_Ga=d,d=i,i={},q=2\x1B\\", KITTY_IMAGE_ID)
}

/// Remove any placement of the viz image id (analysis spectrogram). Safe to emit
/// unconditionally, even when no such image is on screen.
pub fn viz_image_clear_escape() -> String {
    format!("\x1B_Ga=d,d=i,i={},q=2\x1B\\", VIZ_IMAGE_ID)
}

/// Try local sources only: embedded tag, sidecar file, on-disk cache (the
/// config-dir cache, then any legacy cache an older Keet wrote next to the
/// music). Returns None if no local cover exists (callers can then fall back
/// to `resolve_remote`, which is gated by track-change generation counters).
pub fn resolve_local(
    track_path: &Path,
    artist: Option<&str>,
    album: Option<&str>,
    size: CoverSize,
) -> Option<CoverImage> {
    if let Some(bytes) = read_embedded(track_path) {
        return decode_and_resize(&bytes, size);
    }
    if let Some(bytes) = read_sidecar(track_path) {
        return decode_and_resize(&bytes, size);
    }
    let candidates = [
        cache_path_for(artist, album),
        legacy_cache_path_for(track_path, artist, album),
    ];
    for p in candidates.into_iter().flatten() {
        if let Ok(bytes) = std::fs::read(&p) {
            return decode_and_resize(&bytes, size);
        }
    }
    None
}

/// Fetch a cover from iTunes Search and persist it to the on-disk cache for
/// next time. Requires both artist and album — returns None otherwise.
pub fn resolve_remote(artist: &str, album: &str, size: CoverSize) -> Option<CoverImage> {
    let bytes = fetch_itunes(artist, album)?;
    if let Some(p) = cache_path_for(Some(artist), Some(album)) {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, &bytes);
    }
    decode_and_resize(&bytes, size)
}

fn read_embedded(track_path: &Path) -> Option<Vec<u8>> {
    let file = File::open(track_path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = track_path.extension() {
        hint.with_extension(ext.to_str().unwrap_or(""));
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .ok()?;

    // 0.6 folds the probe's own metadata into the format reader, so the
    // separate `probed.metadata` pass this used to need is gone. The Metadata
    // guard must outlive the revision borrowed out of it, hence the binding.
    let meta = format.metadata();
    let rev = meta.current()?;
    let v = rev.media.visuals.first()?;
    Some(v.data.to_vec())
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

/// Cache path for a remote-fetched cover: `<config>/covers/{artist} - {album}
/// .cover.jpg`. Lives in the keet config dir — writing next to the music
/// polluted the user's library folders.
fn cache_path_for(artist: Option<&str>, album: Option<&str>) -> Option<PathBuf> {
    let a = sanitize_fs(artist?);
    let al = sanitize_fs(album?);
    if a.is_empty() || al.is_empty() { return None; }
    let dir = crate::playlist::keet_config_dir()?.join("covers");
    Some(dir.join(format!("{} - {}.cover.jpg", a, al)))
}

/// Where older Keet versions cached covers: next to the track. Still read
/// (saves a refetch for existing users) but never written anymore.
fn legacy_cache_path_for(track_path: &Path, artist: Option<&str>, album: Option<&str>) -> Option<PathBuf> {
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

    // Shared process-wide agent (native TLS, split timeouts — never
    // `timeout_global`, see lyrics::http_agent).
    let agent = crate::lyrics::http_agent();

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

fn decode_and_resize(bytes: &[u8], size: CoverSize) -> Option<CoverImage> {
    let img = image::load_from_memory(bytes).ok()?;
    // `thumbnail_exact` uses nearest-neighbor and allocates only the output
    // buffer. `resize_exact(_, _, Lanczos3)` allocates two intermediate f32
    // RGBA planes sized `dst × src` and `src × dst` — together ~5–8 MB for a
    // 1000×1000 source going to 320×320. At terminal cover sizes the visual
    // difference is invisible, but the peak heap drops by an order of
    // magnitude.
    match detect_protocol() {
        GraphicsProtocol::Kitty => {
            // The PNG is slot-independent — the terminal scales it into the
            // c=/r= cell box at placement time.
            let resized = img.thumbnail_exact(KITTY_SIZE, KITTY_SIZE);
            let mut png: Vec<u8> = Vec::new();
            resized.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
            Some(CoverImage::Kitty { png, size })
        }
        GraphicsProtocol::Iterm2 => {
            let resized = img.thumbnail_exact(KITTY_SIZE, KITTY_SIZE);
            let mut png: Vec<u8> = Vec::new();
            resized.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
            Some(CoverImage::Iterm2 { png, size })
        }
        GraphicsProtocol::Sixel => {
            // Pixel-exact: encode to the slot's pixel box, or the image spills
            // past its reserved rows.
            let (px_w, px_h) = size.sixel_px();
            let resized = img.thumbnail_exact(px_w, px_h);
            let rgba = resized.to_rgba8();
            let opts = icy_sixel::EncodeOptions::default();
            let data = icy_sixel::sixel_encode(
                rgba.as_raw(),
                px_w as usize,
                px_h as usize,
                &opts,
            ).ok()?;
            Some(CoverImage::Sixel { data, size })
        }
        GraphicsProtocol::HalfBlock => {
            // Two pixel rows per cell row (upper/lower half-block).
            let (w, h) = (size.cols, size.rows * 2);
            let resized = img.thumbnail_exact(w, h);
            let rgb = resized.to_rgb8();
            Some(CoverImage::HalfBlock { width: w, height: h, pixels: rgb.into_raw(), size })
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
                None
                    // Clear any background from previous cell on an odd last row.
                    if last_bot.is_some() => {
                        line.push_str("\x1B[49m");
                        last_bot = None;
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

/// Public wrapper so other modules (viz) can render an arbitrary RGB buffer
/// as half-block truecolor rows.
pub fn render_half_block_public(width: u32, height: u32, pixels: &[u8]) -> Vec<String> {
    render_half_block(width, height, pixels)
}

/// Render a raw RGB image (`w`×`h` px) into a block occupying `cols`×`rows`
/// terminal cells, using the detected graphics protocol. Returns one String per
/// row (caller adds horizontal padding). For HalfBlock, the caller should use
/// `render_half_block_public` instead — this fn covers the pixel-image protocols.
pub fn render_image_block(rgb: &[u8], w: u32, h: u32, cols: u32, rows: u32) -> Vec<String> {
    let blank = || vec![String::new(); rows as usize];
    match detect_protocol() {
        GraphicsProtocol::Kitty => match encode_png_rgb(rgb, w, h) {
            Some(png) => viz_kitty_lines(&png, cols, rows),
            None => blank(),
        },
        GraphicsProtocol::Iterm2 => match encode_png_rgb(rgb, w, h) {
            Some(png) => viz_iterm2_lines(&png, cols, rows),
            None => blank(),
        },
        GraphicsProtocol::Sixel => {
            // NOTE: the analysis spectrogram does NOT come through here — it
            // uses render_viz_sixel_indexed (fixed palette, no quantizer; a
            // per-frame quantizer shimmers on animated content). This arm only
            // serves hypothetical future RGB viz images on sixel terminals.
            // icy_sixel wants RGBA; expand the borrowed RGB without an RgbImage copy.
            let mut rgba = Vec::with_capacity(rgb.len() / 3 * 4);
            for px in rgb.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            let opts = icy_sixel::EncodeOptions {
                max_colors: 128,
                diffusion: 0.0,
                ..icy_sixel::EncodeOptions::default()
            };
            match icy_sixel::sixel_encode(&rgba, w as usize, h as usize, &opts) {
                Ok(data) => viz_sixel_lines(&data, cols, rows),
                Err(_) => blank(),
            }
        }
        GraphicsProtocol::HalfBlock => blank(),
    }
}

/// PNG-encode a raw RGB8 buffer directly — no intermediate `RgbImage` allocation
/// or per-frame copy of the borrowed pixels.
fn encode_png_rgb(rgb: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(rgb, w, h, image::ExtendedColorType::Rgb8)
        .ok()?;
    Some(png)
}

fn viz_kitty_lines(png: &[u8], cols: u32, rows: u32) -> Vec<String> {
    let b64 = base64_encode(png);
    let chunk_size = 4096;
    let total = b64.len();
    let mut transmit = String::with_capacity(total + 256);
    let mut pos = 0;
    let mut first = true;
    while pos < total {
        let end = (pos + chunk_size).min(total);
        let is_last = end == total;
        transmit.push_str("\x1B_G");
        if first {
            // p=1 pins a single placement: re-transmitting each frame with the same
            // image id + placement id REPLACES it in place rather than spawning a new
            // placement every frame (which otherwise piles up and slows the terminal).
            let _ = write!(transmit, "a=T,f=100,i={},p=1,c={},r={},C=1,q=2,m={}",
                VIZ_IMAGE_ID, cols, rows, if is_last { 0 } else { 1 });
            first = false;
        } else {
            let _ = write!(transmit, "m={}", if is_last { 0 } else { 1 });
        }
        transmit.push(';');
        transmit.push_str(&b64[pos..end]);
        transmit.push_str("\x1B\\");
        pos = end;
    }
    let blank = " ".repeat(cols as usize);
    let mut lines = Vec::with_capacity(rows as usize);
    lines.push(format!("{transmit}{blank}"));
    for _ in 1..rows { lines.push(blank.clone()); }
    lines
}

fn viz_iterm2_lines(png: &[u8], cols: u32, rows: u32) -> Vec<String> {
    let b64 = base64_encode(png);
    let mut first = String::with_capacity(b64.len() + 128);
    first.push_str("\x1B[s\x1B]1337;File=size=");
    let _ = write!(first, "{}", png.len());
    let _ = write!(first, ";width={};height={};inline=1;preserveAspectRatio=1:", cols, rows);
    first.push_str(&b64);
    first.push('\x07');
    first.push_str("\x1B[u");
    let _ = write!(first, "\x1B[{}C", cols);
    let skip = format!("\x1B[{}C", cols);
    let mut lines = Vec::with_capacity(rows as usize);
    lines.push(first);
    for _ in 1..rows { lines.push(skip.clone()); }
    lines
}

/// Probed terminal cell size in pixels, packed `(w << 8) | h`; 0 = unknown.
/// Settable (not OnceLock) because a terminal resize invalidates it — a
/// font-zoom-out with stale metrics would make the sixel image overflow its
/// block and trigger the auto-scroll storm, so Resize resets to unknown.
static CELL_METRICS: AtomicU32 = AtomicU32::new(0);

pub fn set_cell_metrics(m: Option<(u16, u16)>) {
    let packed = m.map(|(w, h)| ((w as u32) << 8) | h as u32).unwrap_or(0);
    CELL_METRICS.store(packed, Ordering::Relaxed);
}

/// Probed cell size (w, h) in pixels, if a probe succeeded since the last resize.
pub fn cell_metrics() -> Option<(u16, u16)> {
    match CELL_METRICS.load(Ordering::Relaxed) {
        0 => None,
        p => Some(((p >> 8) as u16, (p & 0xFF) as u16)),
    }
}

/// Parse an XTWINOPS reply out of harvested input chars. Accepts either
/// `CSI 6 ; h ; w t` (reply to CSI 16 t: cell size in px, used directly) or
/// `CSI 4 ; h ; w t` (reply to CSI 14 t: text-area px, divided by the cell
/// grid). Prefers the 16 t form when both are present. Values outside sane
/// bounds (w 4..=64, h 8..=128) are rejected — a wrong cell size resurrects
/// the sixel auto-scroll storm, so "no metrics" beats bad metrics.
pub(crate) fn parse_cell_metrics_reply(buf: &str, term_cols: u16, term_rows: u16) -> Option<(u16, u16)> {
    let sane = |w: u32, h: u32| -> Option<(u16, u16)> {
        if (4..=64).contains(&w) && (8..=128).contains(&h) {
            Some((w as u16, h as u16))
        } else {
            None
        }
    };
    let mut from_14t: Option<(u16, u16)> = None;
    for (start, _) in buf.match_indices("\x1B[") {
        let body = &buf[start + 2..];
        let Some(end) = body.find('t') else { continue };
        let mut parts = body[..end].split(';').map(|p| p.parse::<u32>());
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            // CSI 16 t reply: cell size in px directly — exact, wins outright.
            (Some(Ok(6)), Some(Ok(h)), Some(Ok(w)), None) => {
                if let Some(m) = sane(w, h) {
                    return Some(m);
                }
            }
            // CSI 14 t reply: text-area px / character grid.
            (Some(Ok(4)), Some(Ok(h)), Some(Ok(w)), None)
                if from_14t.is_none() && term_cols > 0 && term_rows > 0 =>
            {
                from_14t = sane(w / term_cols as u32, h / term_rows as u32);
            }
            _ => {}
        }
    }
    from_14t
}

/// One-shot startup probe for the terminal cell size, used for pixel-exact
/// Sixel sizing. Sends XTWINOPS 16 (cell px) and 14 (text-area px) and
/// harvests the reply from crossterm key events — through ConPTY the reply
/// surfaces as a burst of Esc/Char key events. MUST run after raw mode is
/// enabled and BEFORE the first poll_input: once the main input loop owns
/// stdin, a late reply's Esc would read as the quit key. Bounded ~300 ms and
/// only runs when the detected protocol is Sixel, so non-answering terminals
/// cost one short startup pause at worst. Returns true if a Resize event was
/// swallowed during the harvest (caller should force a layout repaint).
pub fn probe_cell_metrics() -> bool {
    let mut resize_seen = false;
    if !matches!(detect_protocol(), GraphicsProtocol::Sixel) {
        return resize_seen;
    }
    {
        use std::io::Write as _;
        print!("\x1B[16t\x1B[14t");
        let _ = std::io::stdout().flush();
    }
    let (tc, tr) = crossterm::terminal::size().unwrap_or((120, 30));
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    let mut buf = String::new();
    while std::time::Instant::now() < deadline {
        if !matches!(crossterm::event::poll(Duration::from_millis(25)), Ok(true)) {
            continue;
        }
        let Ok(ev) = crossterm::event::read() else { continue };
        match ev {
            crossterm::event::Event::Key(k) => match k.code {
                crossterm::event::KeyCode::Esc => buf.push('\x1B'),
                crossterm::event::KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            crossterm::event::Event::Resize(_, _) => resize_seen = true,
            _ => {}
        }
        if let Some(m) = parse_cell_metrics_reply(&buf, tc, tr) {
            set_cell_metrics(Some(m));
            return resize_seen;
        }
    }
    // Whatever arrived by the deadline (e.g. only the 14 t reply).
    if let Some(m) = parse_cell_metrics_reply(&buf, tc, tr) {
        set_cell_metrics(Some(m));
    }
    resize_seen
}

/// Encode an indexed-color image as a sixel blob with a caller-supplied FIXED
/// palette (same DCS framing icy_sixel emits, which Windows Terminal renders:
/// `\x1bP9;1;0q`, `#i;2;r;g;b` in percent, per-color RLE bands). Bypassing the
/// quantizer matters for animated content: re-quantizing a scrolling image
/// lands a slightly different palette every emission, so even unchanged
/// pixels get re-colored — visible shimmer. With a fixed palette, unchanged
/// content encodes to byte-identical output.
pub(crate) fn sixel_encode_indexed(indices: &[u8], palette: &[(u8, u8, u8)], w: usize, h: usize) -> String {
    let mut out = String::with_capacity(indices.len() / 4 + palette.len() * 16 + 32);
    out.push_str("\x1BP9;1;0q");
    for (i, &(r, g, b)) in palette.iter().enumerate() {
        let _ = write!(out, "#{};2;{};{};{}",
            i, r as u32 * 100 / 255, g as u32 * 100 / 255, b as u32 * 100 / 255);
    }
    let mut col_bits: Vec<u8> = vec![0; w];
    for y0 in (0..h).step_by(6) {
        let y_max = (y0 + 6).min(h);
        // Colors present in this band (rows y0..y_max are contiguous memory).
        let mut used = [false; 256];
        for &px in &indices[y0 * w..y_max * w] {
            used[px as usize] = true;
        }
        for (c, used_c) in used.iter().enumerate().take(palette.len()) {
            if !used_c { continue; }
            // Build this color's per-column sixel bits, then run-length encode.
            col_bits.iter_mut().for_each(|b| *b = 0);
            for (bit, y) in (y0..y_max).enumerate() {
                let row = &indices[y * w..(y + 1) * w];
                for (b, &px) in col_bits.iter_mut().zip(row) {
                    if px as usize == c {
                        *b |= 1 << bit;
                    }
                }
            }
            let _ = write!(out, "#{}", c);
            let mut x = 0;
            while x < w {
                let bits = col_bits[x];
                let mut run = 1;
                while x + run < w && col_bits[x + run] == bits {
                    run += 1;
                }
                let ch = (63 + bits) as char;
                if run > 3 {
                    let _ = write!(out, "!{}{}", run, ch);
                } else {
                    for _ in 0..run {
                        out.push(ch);
                    }
                }
                x += run;
            }
            out.push('$'); // back to band start for the next color overlay
        }
        out.push('-'); // next band
    }
    out.push_str("\x1B\\");
    out
}

/// Sixel viz block from pre-indexed pixels and a fixed palette — the
/// shimmer-free path for the analysis spectrogram (see sixel_encode_indexed).
pub(crate) fn render_viz_sixel_indexed(
    indices: &[u8],
    palette: &[(u8, u8, u8)],
    w: usize,
    h: usize,
    cols: u32,
    rows: u32,
) -> Vec<String> {
    let data = sixel_encode_indexed(indices, palette, w, h);
    viz_sixel_lines(&data, cols, rows)
}

fn viz_sixel_lines(data: &str, cols: u32, rows: u32) -> Vec<String> {
    // Sixel pixels are ordinary cell content — no Kitty-style image layer or
    // id-replacement. Two consequences: (1) the block must clear ITSELF (one
    // erase pass over all rows here, before painting) because stale cells are
    // never cleared elsewhere; (2) the caller must print the remaining rows
    // without erase-to-EOL — an EL after this line has painted them wipes that
    // row's slice of the image (seen on Windows Terminal: 16-row image reduced
    // to a 1-row strip). See viz::analysis_needs_raw_lines.
    // SCORC (ESC[u) restores without consuming the save, so it's used twice.
    let mut first = String::with_capacity(data.len() + 8 * rows as usize + 16);
    first.push_str("\x1B[s");
    for _ in 0..rows {
        first.push_str("\x1B[2K\x1B[B");
    }
    first.push_str("\x1B[u");
    first.push_str(data);
    first.push_str("\x1B[u");
    let _ = write!(first, "\x1B[{}C", cols);
    let skip = format!("\x1B[{}C", cols);
    let mut lines = Vec::with_capacity(rows as usize);
    lines.push(first);
    for _ in 1..rows { lines.push(skip.clone()); }
    lines
}

/// Solid black cells filling the cover slot. Used as a placeholder when no
/// cover is available so the banner layout doesn't shift while one loads or
/// for tracks without artwork.
pub fn placeholder_lines(size: CoverSize) -> Vec<String> {
    let cells = " ".repeat(size.cols as usize);
    let line = format!("\x1B[48;2;0;0;0m{}\x1B[0m", cells);
    (0..size.rows).map(|_| line.clone()).collect()
}

/// Render the cover to a Vec of COVER_ROWS lines, each COVER_COLS wide.
/// For Kitty, line 0 carries the image-transmit escape plus blank spaces;
/// subsequent lines are blank spaces that the image overlays.
pub fn render(img: &CoverImage) -> Vec<String> {
    let size = img.size();
    match img {
        CoverImage::HalfBlock { width, height, pixels, .. } => {
            render_half_block(*width, *height, pixels)
        }
        CoverImage::Kitty { png, .. } => render_kitty(png, size),
        CoverImage::Iterm2 { png, .. } => render_iterm2(png, size),
        CoverImage::Sixel { data, .. } => render_sixel(data, size),
    }
}

fn render_kitty(png: &[u8], size: CoverSize) -> Vec<String> {
    let cols = size.cols as usize;
    let mut lines = Vec::with_capacity(size.rows as usize);
    let blank = " ".repeat(cols);
    let mut first = String::with_capacity(png.len() * 2);
    first.push_str(&kitty_transmit(png, size));
    first.push_str(&blank);
    lines.push(first);
    for _ in 1..size.rows {
        lines.push(blank.clone());
    }
    lines
}

fn render_sixel(data: &str, size: CoverSize) -> Vec<String> {
    // Same trick as iTerm2: save cursor, blast the sixel data (which leaves
    // the cursor in implementation-defined positions), restore, then advance
    // by size.cols via cursor-right so we don't paint over the image cells.
    let mut lines = Vec::with_capacity(size.rows as usize);
    let mut first = String::with_capacity(data.len() + 16);
    first.push_str("\x1B[s");
    first.push_str(data);
    first.push_str("\x1B[u");
    let _ = write!(first, "\x1B[{}C", size.cols);
    lines.push(first);
    let skip = format!("\x1B[{}C", size.cols);
    for _ in 1..size.rows {
        lines.push(skip.clone());
    }
    lines
}

fn render_iterm2(png: &[u8], size: CoverSize) -> Vec<String> {
    let mut lines = Vec::with_capacity(size.rows as usize);
    let b64 = base64_encode(png);
    // Save cursor, emit the image (which would otherwise leave the cursor
    // in an implementation-defined position), restore cursor, then advance
    // exactly size.cols cells. The image is "attached" to the cells it
    // occupies; using \x1B[NC instead of literal spaces avoids overwriting
    // those image cells on rows 1-9.
    let mut first = String::with_capacity(b64.len() + 128);
    first.push_str("\x1B[s\x1B]1337;File=size=");
    let _ = write!(first, "{}", png.len());
    let _ = write!(
        first,
        ";width={};height={};inline=1;preserveAspectRatio=1:",
        size.cols, size.rows
    );
    first.push_str(&b64);
    first.push('\x07');
    first.push_str("\x1B[u");
    let _ = write!(first, "\x1B[{}C", size.cols);
    lines.push(first);
    let skip = format!("\x1B[{}C", size.cols);
    for _ in 1..size.rows {
        lines.push(skip.clone());
    }
    lines
}

fn kitty_transmit(png: &[u8], size: CoverSize) -> String {
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
                size.cols,
                size.rows,
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

#[cfg(test)]
mod cache_path_tests {
    use super::*;

    #[test]
    fn cover_cache_lives_in_config_dir_not_music_folder() {
        // Remote-fetch caches used to be written next to the music as
        // `{artist} - {album}.cover.jpg`, polluting the user's library folders.
        let path = cache_path_for(Some("AC/DC"), Some("Back in Black."))
            .expect("cache path resolvable when HOME is set");
        let covers = crate::playlist::keet_config_dir()
            .expect("config dir resolvable in tests")
            .join("covers");
        assert!(
            path.starts_with(&covers),
            "cache must live under {covers:?}, got {path:?}"
        );
        // Filename sanitized: '/' replaced, trailing dot trimmed.
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            "AC_DC - Back in Black.cover.jpg"
        );
        // Unusable without both tags.
        assert!(cache_path_for(None, Some("X")).is_none());
        assert!(cache_path_for(Some("X"), None).is_none());
    }

    #[test]
    fn legacy_cache_path_stays_next_to_the_track_for_reads() {
        let track = std::path::Path::new("/music/rock/song.flac");
        let path = legacy_cache_path_for(track, Some("A"), Some("B")).unwrap();
        assert_eq!(path, std::path::Path::new("/music/rock/A - B.cover.jpg"));
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn wezterm_on_unix_uses_kitty() {
        let p = protocol_from_env(Some("WezTerm"), None, false, None, false, false);
        assert_eq!(p, GraphicsProtocol::Kitty);
    }

    #[test]
    fn wezterm_on_windows_falls_back_to_half_block() {
        // ConPTY drops the APC sequences the Kitty protocol rides on, so a
        // locally spawned process must not pick Kitty under Windows-WezTerm.
        let p = protocol_from_env(Some("WezTerm"), None, false, None, false, true);
        assert_eq!(p, GraphicsProtocol::HalfBlock);
    }

    #[test]
    fn windows_terminal_uses_sixel() {
        let p = protocol_from_env(None, Some("xterm-256color"), false, None, true, true);
        assert_eq!(p, GraphicsProtocol::Sixel);
    }

    #[test]
    fn plain_terminal_uses_half_block() {
        let p = protocol_from_env(None, Some("xterm-256color"), false, None, false, false);
        assert_eq!(p, GraphicsProtocol::HalfBlock);
    }

    #[test]
    fn ghostty_uses_kitty_and_iterm_uses_osc1337() {
        let g = protocol_from_env(Some("ghostty"), None, false, None, false, false);
        assert_eq!(g, GraphicsProtocol::Kitty);
        let i = protocol_from_env(Some("iTerm.app"), None, false, None, false, false);
        assert_eq!(i, GraphicsProtocol::Iterm2);
    }
}

#[cfg(test)]
mod viz_sixel_tests {
    use super::*;

    #[test]
    fn viz_sixel_first_line_erases_whole_block_before_painting() {
        // Sixel pixels are ordinary cell content. The block must erase all of
        // its rows up front, inside the same line as the transmit — if the
        // caller erased rows 2..N after this line painted them, the image
        // would be wiped down to a 1-row strip (observed on Windows Terminal).
        let lines = viz_sixel_lines("SIXELDATA", 120, 16);
        assert_eq!(lines.len(), 16);
        let erase_pass = "\x1B[2K\x1B[B".repeat(16);
        let want_prefix = format!("\x1B[s{}\x1B[u", erase_pass);
        assert!(
            lines[0].starts_with(&want_prefix),
            "first line must erase all rows before painting, got start: {:?}",
            &lines[0][..lines[0].len().min(40)]
        );
        assert!(lines[0].contains("SIXELDATA"));
        assert!(lines[0].ends_with("\x1B[u\x1B[120C"));
    }

    #[test]
    fn cell_metrics_parses_direct_16t_reply() {
        // CSI 16 t reply: ESC [ 6 ; height ; width t — height comes first.
        assert_eq!(parse_cell_metrics_reply("\x1B[6;19;9t", 120, 30), Some((9, 19)));
    }

    #[test]
    fn cell_metrics_parses_14t_text_area_fallback() {
        // CSI 14 t reply: text area in px, divided by the character grid.
        assert_eq!(parse_cell_metrics_reply("\x1B[4;570;1080t", 120, 30), Some((9, 19)));
    }

    #[test]
    fn cell_metrics_found_amid_other_input_and_prefers_16t() {
        // Replies can arrive surrounded by unrelated chars, and both replies
        // may land in one harvest — the exact 16 t form wins.
        let buf = "x\x1B[4;600;1200t..\x1B[6;20;10t!";
        assert_eq!(parse_cell_metrics_reply(buf, 120, 30), Some((10, 20)));
    }

    #[test]
    fn cell_metrics_rejects_insane_or_missing_replies() {
        assert_eq!(parse_cell_metrics_reply("\x1B[6;0;0t", 120, 30), None);
        assert_eq!(parse_cell_metrics_reply("\x1B[6;500;3t", 120, 30), None);
        assert_eq!(parse_cell_metrics_reply("no reply here", 120, 30), None);
        assert_eq!(parse_cell_metrics_reply("", 120, 30), None);
    }

    #[test]
    fn sixel_indexed_emits_fixed_palette_and_rle() {
        // 8×6 image, all palette index 0 → one band, every column has all six
        // bits set (char 63+63='~'), run of 8 (>3) uses RLE: "!8~".
        let idx = vec![0u8; 48];
        let s = sixel_encode_indexed(&idx, &[(0, 0, 0), (255, 255, 255)], 8, 6);
        assert!(s.starts_with("\x1BP9;1;0q"), "proven-on-WT DCS header");
        assert!(s.contains("#0;2;0;0;0"), "palette entry 0 in percent scale");
        assert!(s.contains("#1;2;100;100;100"), "palette entry 1 in percent scale");
        assert!(s.contains("#0!8~"), "RLE band for color 0");
        assert!(s.ends_with("\x1B\\"), "string terminator");
    }

    #[test]
    fn sixel_indexed_partial_band_overlays_colors() {
        // 2×3 image: column 0 = color 0, column 1 = color 1. Three rows fill
        // only the low 3 sixel bits (char 63+7='F'); empty columns are '?'.
        let idx = vec![0, 1, 0, 1, 0, 1];
        let s = sixel_encode_indexed(&idx, &[(0, 0, 0), (255, 0, 0)], 2, 3);
        assert!(s.contains("#0F?"), "color 0 paints column 0 only");
        assert!(s.contains("#1?F"), "color 1 paints column 1 only");
        assert!(s.contains('$'), "carriage return between color overlays");
        assert!(s.contains('-'), "band terminator");
    }

    #[test]
    fn viz_sixel_skip_lines_write_nothing_destructive() {
        // Rows 2..N must be pure cursor-right skips: no erase, no spaces —
        // anything that touches the cells wipes that row's slice of the image.
        let lines = viz_sixel_lines("SIXELDATA", 80, 4);
        for line in &lines[1..] {
            assert_eq!(line, "\x1B[80C");
        }
    }
}
