// Keet - Low-CPU audio player with producer/consumer architecture
// - Lock-free ring buffer (no mutex in audio callback)
// - SincFixedIn resampler (high quality)
// - Batched atomic updates with Relaxed ordering
// - Separate decode thread
//
// Usage: cargo run --release -- <file-or-folder> [--shuffle] [--repeat] [--quality]
// Controls: Space=Pause, ↑↓=Tracks, ←→=Seek ±10s, V=Viz, +/-=Vol, Q=Quit

mod ansi;
mod state;
mod theme;
mod config;
mod library;
mod eq_ui;
mod viz;
mod audio;
mod decode;
mod playlist;
mod ui;
mod ui_hifi;
mod ui_minimal;
mod eq;
mod effects;
mod media_keys;
mod resume;
mod crossfeed;
mod metadata;
mod lyrics;
mod cover;

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::StreamConfig;
use crossterm::terminal;
use rtrb::RingBuffer;

use state::{PlayerState, UiState, RgMode, VizMode, ring_capacity_for, VIZ_BUFFER_SIZE};
use viz::{StatsMonitor, VizAnalyser};
use audio::{build_stream, set_output_sample_rate, probe_sample_rate, fix_bluetooth_sample_rate};
use decode::{decode_playlist, await_consumer_drain};
use playlist::{build_playlist, shuffle_list};
use ui::{print_status, poll_input, poll_auto_sort, poll_library_tree, arm_auto_sort, format_time};
use resume::{ResumeState, save_state, load_state};

/// Kick off the lyrics loader on a background thread and install its receiver on `ui`.
/// Reads embedded tags from the file if not already cached, then falls back to LRCLIB.
/// The main thread never blocks on disk or HTTP.
fn spawn_lyrics_worker(ui: &mut state::UiState, path: std::path::PathBuf, dur: Option<u32>) {
    if let Some(l) = ui.metadata_cache.lyrics(ui.current) {
        ui.lyrics = Some(lyrics::parse_lyrics(&l));
        ui.lyrics_receiver = None;
        return;
    }
    let (cached_artist, cached_title) = ui.metadata_cache.artist_title(ui.current);
    ui.lyrics = None;
    let (tx, rx) = std::sync::mpsc::channel();
    ui.lyrics_receiver = Some(rx);
    // Bump the generation; each worker snapshots this and bails out of the slow
    // LRCLIB fetch if the user has skipped to another track in the meantime.
    // This prevents a backlog of blocked HTTP threads during rapid skipping.
    let gen_snap = ui.lyrics_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let gen_ref = std::sync::Arc::clone(&ui.lyrics_gen);
    std::thread::spawn(move || {
        let (artist, title, embedded) = if cached_artist.is_some() || cached_title.is_some() {
            (cached_artist, cached_title, metadata::read_lyrics(&path))
        } else {
            metadata::read_artist_title_lyrics(&path)
        };
        let res = if let Some(l) = embedded {
            Some(lyrics::parse_lyrics(&l))
        } else if let (Some(a), Some(t)) = (artist, title) {
            // Skip the network round-trip if a newer request has already been issued.
            if gen_ref.load(std::sync::atomic::Ordering::Relaxed) != gen_snap {
                None
            } else {
                lyrics::fetch_lrclib(&a, &t, dur).map(|s| lyrics::parse_lyrics(&s))
            }
        } else {
            None
        };
        let _ = tx.send(res);
    });
}

/// Kick off the album-cover loader on a background thread. Tries embedded,
/// sidecar, on-disk cache, then iTunes Search (saving result back to cache).
/// Exits early (before HTTP) if a newer track has been selected.
fn spawn_cover_worker(ui: &mut state::UiState, path: std::path::PathBuf) {
    if !ui.cover_enabled {
        ui.cover = None;
        ui.cover_receiver = None;
        return;
    }
    let (cached_artist, cached_album) = ui.metadata_cache.artist_album(ui.current);
    ui.cover = None;
    let (tx, rx) = std::sync::mpsc::channel();
    ui.cover_receiver = Some(rx);
    let gen_snap = ui.cover_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let gen_ref = std::sync::Arc::clone(&ui.cover_gen);
    std::thread::spawn(move || {
        // Local sources are cheap — always try them regardless of generation.
        let local = cover::resolve_local(
            &path,
            cached_artist.as_deref(),
            cached_album.as_deref(),
        );
        if let Some(img) = local {
            let _ = tx.send(Some(img));
            return;
        }
        // Remote fetch is slow (HTTP) — skip if user has already skipped past this track.
        if gen_ref.load(std::sync::atomic::Ordering::Relaxed) != gen_snap {
            let _ = tx.send(None);
            return;
        }
        let remote = match (cached_artist, cached_album) {
            (Some(a), Some(al)) => cover::resolve_remote(&a, &al),
            _ => None,
        };
        let _ = tx.send(remote);
    });
}

/// Compose the banner text with the album-cover slot on its left. When no
/// cover is loaded, fills the slot with a solid black box so the layout
/// doesn't shift between tracks. Falls back to the plain banner only when
/// the terminal is too narrow to fit both side-by-side.
fn compose_banner(banner_text: &str, cover: Option<&cover::CoverImage>, term_w: usize) -> (String, usize) {
    let cover_cols = cover::COVER_COLS as usize;
    // Banner box is ~59 cols; need room for cover + 2-space gap + banner.
    if term_w < cover_cols + 2 + 59 {
        return (banner_text.to_string(), banner_text.lines().count());
    }
    let cover_lines = match cover {
        Some(img) => cover::render(img),
        None => cover::placeholder_lines(),
    };
    let has_trailing_nl = banner_text.ends_with('\n');
    let banner_content = if has_trailing_nl {
        &banner_text[..banner_text.len() - 1]
    } else {
        banner_text
    };
    let banner_lines: Vec<&str> = banner_content.split('\n').collect();
    let total = banner_lines.len().max(cover_lines.len());
    let pad = " ".repeat(cover_cols);
    let mut out = String::new();
    for i in 0..total {
        let left = cover_lines.get(i).map(|s| s.as_str()).unwrap_or(&pad);
        let right = banner_lines.get(i).copied().unwrap_or("");
        out.push_str(left);
        out.push_str("  ");
        out.push_str(right);
        if i + 1 < total {
            out.push('\n');
        }
    }
    if has_trailing_nl {
        out.push('\n');
    }
    let line_count = out.lines().count();
    (out, line_count)
}

/// Show the terminal cursor again, ignoring write errors.
///
/// **Must not panic.** The panic hook calls this, and `print!` panics when the
/// underlying write fails. With stdout closed (`keet --help | head`) the
/// original panic would trigger a second panic here — and panicking while
/// panicking aborts the process instead of exiting cleanly. Writing through a
/// handle and discarding the `Result` keeps this total.
fn restore_cursor(w: &mut impl Write) {
    let _ = w.write_all(b"\x1B[?25h");
    let _ = w.flush();
}

/// Whether a panic deserves a `crash.log` entry.
///
/// `println!` panics when stdout goes away, so `keet --help | head` panics with
/// "failed printing to stdout: Broken pipe". That's the shell hanging up on us,
/// not a crash — logging it would fill the file with noise from ordinary piping.
fn should_log_crash(info: &str) -> bool {
    const PIPE_CLOSED: [&str; 3] = [
        "Broken pipe",              // Unix
        "The pipe has been ended",  // Windows
        "The pipe is being closed", // Windows
    ];
    !PIPE_CLOSED.iter().any(|m| info.contains(m))
}

fn build_resume_state(
    ui: &state::UiState,
    playlist: &[std::path::PathBuf],
    player_state: &state::PlayerState,
    eq_presets: &[eq::EqPreset],
    fx_presets: &[effects::EffectsPreset],
    cf_presets: &[crossfeed::CrossfeedPreset],
    device_name: &Option<String>,
) -> ResumeState {
    let repeat_mode_str = match ui.repeat_mode {
        state::RepeatMode::Off => "off",
        state::RepeatMode::All => "all",
        state::RepeatMode::One => "one",
    };
    let eq_bands = player_state.eq_bands_array();
    ResumeState {
        source_paths: ui.source_paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        track_path: playlist.get(ui.current)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        position_secs: player_state.time_secs(),
        shuffle: ui.shuffle,
        repeat: false, // skipped during serialization; see resume.rs
        repeat_mode: Some(repeat_mode_str.to_string()),
        volume: player_state.volume.load(std::sync::atomic::Ordering::Relaxed),
        eq_preset: eq_presets[player_state.eq_index()].name.clone(),
        effects_preset: fx_presets[player_state.effects_index()].name.clone(),
        rg_mode: Some(player_state.rg_mode().name().to_lowercase()),
        device: device_name.clone(),
        exclusive: Some(player_state.exclusive.load(std::sync::atomic::Ordering::Relaxed)),
        crossfeed_preset: Some(cf_presets[player_state.crossfeed_index()].name.clone()),
        balance: Some(player_state.balance_value()),
        theme: Some(player_state.theme_kind().name().to_string()),
        eq_gains: Some(player_state.eq_gains_array().to_vec()),
        eq_custom: Some(player_state.is_eq_custom()),
        eq_types: Some(eq_bands.iter().map(|b| b.kind.name().to_string()).collect()),
        eq_freqs: Some(eq_bands.iter().map(|b| b.freq).collect()),
        eq_qs: Some(eq_bands.iter().map(|b| b.q).collect()),
        eq_preamp: Some(player_state.eq_preamp_db()),
    }
}

/// Build a fresh audio + viz ring-buffer pair and output stream sized for
/// `stream_rate`, updating `state.ring_capacity` to match. Used at initial setup
/// and for every stream rebuild (exclusive rate switch, stream-error device swap)
/// so those paths can't drift apart. Returns the audio producer, viz consumer, and
/// stream; the caller calls `stream.play()`.
/// Audio producer, viz consumer, and output stream returned by `rebuild_stream`.
type StreamParts = (rtrb::Producer<f32>, rtrb::Consumer<f32>, cpal::Stream);

fn rebuild_stream(
    device: &cpal::Device,
    stream_rate: u32,
    channels: u16,
    buffer_size: cpal::BufferSize,
    state: &Arc<PlayerState>,
) -> Result<StreamParts, Box<dyn std::error::Error>> {
    let ring_cap = ring_capacity_for(stream_rate);
    state.ring_capacity.store(ring_cap, Ordering::Relaxed);
    let (prod, cons) = RingBuffer::<f32>::new(ring_cap);
    let (viz_prod, viz_cons) = RingBuffer::<f32>::new(VIZ_BUFFER_SIZE);
    // `channels` is the DEVICE's channel count, not the ring's. The ring is
    // always stereo; the audio callback fans it out to however many channels
    // the device wants (mono duplicates, >2 leaves the extras silent).
    //
    // Hardcoding 2 here broke Windows: WASAPI shared mode only accepts the
    // mixer's own format, so a device whose shared format isn't stereo made
    // `IsFormatSupported` return S_FALSE → "Stream configuration is not
    // supported in shared mode". cpal 0.17 never noticed (its check was a stub
    // returning true); 0.18 actually calls IsFormatSupported and rejects.
    let config = StreamConfig {
        channels,
        sample_rate: stream_rate,
        buffer_size,
    };
    let stream = match build_stream(device, &config, cons, viz_prod, Arc::clone(state)) {
        Ok(s) => s,
        Err(e) => {
            // Last resort: take the device's default config verbatim. Rebuilds
            // the rings because the rate may differ from what we asked for.
            //
            // Surfaced in the status line rather than swallowed: if this fires,
            // the config we derived from the device was wrong, and we want to
            // hear about it instead of silently running on different settings.
            let fallback = device.default_output_config()?;
            let note = format!(
                "audio config {}ch/{}Hz rejected ({}) — fell back to {}ch/{}Hz",
                channels, stream_rate, e, fallback.channels(), fallback.sample_rate()
            );
            // Status line AND stderr: the status line is painted over within a
            // frame or two at startup, so `keet ... 2>log.txt` is the only way
            // to actually catch this after the fact.
            eprintln!("keet: {note}");
            if let Ok(mut err) = state.decode_error.lock() {
                *err = Some(note);
            }
            if fallback.channels() == channels && fallback.sample_rate() == stream_rate {
                return Err(e);
            }
            let rate = fallback.sample_rate();
            let ring_cap = ring_capacity_for(rate);
            state.ring_capacity.store(ring_cap, Ordering::Relaxed);
            state.output_rate.store(rate as u64, Ordering::Relaxed);
            let (p, c) = RingBuffer::<f32>::new(ring_cap);
            let (vp, vc) = RingBuffer::<f32>::new(VIZ_BUFFER_SIZE);
            let cfg = StreamConfig {
                channels: fallback.channels(),
                sample_rate: rate,
                buffer_size: cpal::BufferSize::Default,
            };
            let s = build_stream(device, &cfg, c, vp, Arc::clone(state))?;
            return Ok((p, vc, s));
        }
    };
    Ok((prod, viz_cons, stream))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Ensure terminal is in normal mode (cleanup from previous crashed runs)
    let _ = terminal::disable_raw_mode();
    // On Windows, legacy conhost/cmd.exe don't enable VT processing by default, which
    // would leave the entire TUI as raw escape codes. supports_ansi() has the side
    // effect of calling SetConsoleMode with ENABLE_VIRTUAL_TERMINAL_PROCESSING.
    #[cfg(target_os = "windows")]
    {
        let _ = crossterm::ansi_support::supports_ansi();
    }
    // NOTE: the startup terminal reset lives further down, after the --help /
    // --list-devices early exits — those just print to stdout and return, so
    // resetting here would wipe the screen (and emit a stray ESC c into the
    // output when piped) for commands that never draw the TUI.

    // Restore terminal on panic so it doesn't stay in raw mode
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        restore_cursor(&mut io::stdout());

        // Write crash log to ~/.config/keet/crash.log
        let info_str = info.to_string();
        if should_log_crash(&info_str) {
            if let Some(config_dir) = playlist::keet_config_dir() {
                let _ = std::fs::create_dir_all(&config_dir);
                let log_path = config_dir.join("crash.log");
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let entry = format!("[{}] {}\n", timestamp, info_str);
                // Append to log file
                use std::io::Write as _;
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                    let _ = f.write_all(entry.as_bytes());
                }
            }
        }

        default_panic(info);
    }));

    let args: Vec<String> = env::args().collect();

    // Handle --help (print and exit)
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("\x1B[1mKeet\x1B[0m — Terminal audio player with real-time visualization and parametric EQ");
        println!();
        println!("\x1B[1mUSAGE\x1B[0m");
        println!("  keet <file|folder|playlist>... [options]");
        println!("  keet                              Resume last session");
        println!();
        println!("\x1B[1mOPTIONS\x1B[0m");
        println!("  -s, --shuffle          Randomize playlist order (re-shuffles on each repeat)");
        println!("  -r, --repeat           Loop playlist (rescans sources for new files each cycle)");
        println!("  -q, --quality          HQ resampler (higher CPU, inaudible difference)");
        println!("  -e, --eq <name|path>   Start with EQ preset by name or JSON file path");
        println!("      --fx <name|path>   Start with effects preset by name or JSON file path");
        println!("  -x, --crossfade <secs> Crossfade duration between tracks (0 = disabled)");
        println!("      --rg-mode <mode>   ReplayGain: track (default), album, or off");
        println!("      --device <name>    Output device (substring match)");
        println!("      --exclusive        Exclusive mode: per-track sample rate, device lock (macOS)");
        println!("      --no-cover         Disable album cover display");
        println!("      --theme <name>     UI theme: classic (default), minimal, hifi");
        println!("      --list-devices     List available output devices and exit");
        println!("  -h, --help             Show this help");
        println!();
        println!("\x1B[1mFORMATS\x1B[0m  MP3, FLAC, WAV, OGG, AAC/M4A, ALAC, AIFF");
        println!();
        println!("\x1B[1mKEYBOARD\x1B[0m");
        println!("  Space        Pause / resume");
        println!("  Up / Down    Next / previous track");
        println!("  Right / Left Seek forward / backward 10s");
        println!("  + / -        Volume up / down (5% steps, 0–150%)");
        println!("  V            Cycle visualization (off → VU → spectrum H/V → scope → vector → spectrogram)");
        println!("  B            Toggle viz style (dots / bars)");
        println!("  F            Toggle pre/post-fader metering");
        println!("  E            Cycle EQ presets");
        println!("  X            Cycle effects presets");
        println!("  C            Cycle crossfeed (Off → Light → Medium → Strong + custom)");
        println!("  [ / ]        Balance left / right (5% steps)");
        println!("  L            Toggle playlist view");
        println!("  Y            Toggle lyrics view (synced LRC auto-scrolls)");
        println!("  S            Save playlist as M3U");
        println!("  R            Rescan folders for new files");
        println!("  Z            Toggle shuffle");
        println!("  Shift+R      Toggle repeat (Off → All → One)");
        println!("  T            Cycle UI theme (Classic → Minimal → HiFi)");
        println!("  O            Open a new source (type a path)");
        println!("  P            Pick a new source (native folder dialog)");
        println!("  I            Toggle CPU/memory stats");
        println!("  Q / Esc      Quit");
        println!();
        println!("\x1B[1mPLAYLIST VIEW\x1B[0m  (press L)");
        println!("  Up / Down       Move cursor");
        println!("  Home / End      Jump to top / bottom              (also: g / G)");
        println!("  PgUp / PgDn     Page up / down                    (also: Ctrl+U / Ctrl+D)");
        println!("  Enter           Jump to selected track");
        println!("  A               Enqueue selected track (play next)");
        println!("  Shift+S         Sort by tags (artist → album → disc → track → title)");
        println!("  /               Search / filter by filename");
        println!("  D / Delete      Remove selected track");
        println!("  Esc / L         Close playlist view");
        println!();
        println!("\x1B[1mLYRICS VIEW\x1B[0m  (press Y)");
        println!("  W / S        Scroll up / down (disables auto-scroll)");
        println!("  A / D        Adjust sync offset −/+ 0.5s");
        println!("  Esc / Y      Close lyrics view");
        println!();
        println!("\x1B[1mCUSTOM PRESETS\x1B[0m");
        println!("  EQ:      ~/.config/keet/eq/*.json");
        println!("  Effects: ~/.config/keet/effects/*.json");
        println!("  Crossfeed: ~/.config/keet/crossfeed/*.json");
        println!();
        println!("\x1B[1mCONFIG\x1B[0m");
        println!("  ~/.config/keet/config.json — persistent defaults, e.g. {{\"theme\": \"minimal\"}}");
        return Ok(());
    }

    // Handle --list-devices (print and exit)
    if args.iter().any(|a| a == "--list-devices") {
        let host = cpal::default_host();
        audio::list_output_devices(&host);
        return Ok(());
    }

    // Full terminal reset in case a previous run crashed mid-draw.
    // \x1Bc = RIS (Reset to Initial State) - clears screen, resets charset,
    // tab stops, modes. Deliberately AFTER the print-and-exit flags above so
    // it only fires on the path that actually draws the TUI.
    print!("\x1Bc");
    io::stdout().flush().ok();

    let flags = ["--shuffle", "-s", "--repeat", "-r", "--quality", "-q", "--eq", "-e", "--fx", "--crossfade", "-x", "--rg-mode", "--list-devices", "--device", "--exclusive", "--no-cover", "--theme", "--help", "-h"];
    // Loaded once and reused for the volume/EQ/device restore further down.
    let resume_state_loaded = if args.len() < 2 { load_state() } else { None };
    let (source_paths, shuffle, repeat_mode) = if args.len() < 2 {
        // Try resume from saved state
        match resume_state_loaded.as_ref() {
            Some(rs) => {
                let paths: Vec<PathBuf> = rs.source_paths.iter()
                    .filter_map(|s| {
                        let p = PathBuf::from(s);
                        if p.exists() { Some(p) } else {
                            eprintln!("Saved path not found, skipping: {}", s);
                            None
                        }
                    })
                    .collect();
                if paths.is_empty() {
                    eprintln!("No saved paths found");
                    std::process::exit(1);
                }
                let rm = match rs.repeat_mode.as_deref() {
                    Some("one") => state::RepeatMode::One,
                    Some("all") => state::RepeatMode::All,
                    Some("off") => state::RepeatMode::Off,
                    _ => if rs.repeat { state::RepeatMode::All } else { state::RepeatMode::Off },
                };
                (paths, rs.shuffle, rm)
            }
            None => {
                match ui::run_first_launch_picker() {
                    Some(p) => (vec![p], false, state::RepeatMode::Off),
                    None => {
                        eprintln!("Usage: {} <file-or-folder>... [--shuffle] [--repeat] [--quality] [--eq <name>] [--fx <name>] [--crossfade <secs>] [--rg-mode track|album|off] [--device <name>] [--exclusive] [--list-devices]", args[0]);
                        eprintln!("Controls: Space=Pause ↑↓=Tracks ←→=Seek V=Viz E=EQ X=FX L=List R=Rescan O=Open P=Pick +/-=Vol Q=Quit");
                        std::process::exit(1);
                    }
                }
            }
        }
    } else {
        let s = args.iter().any(|a| a == "--shuffle" || a == "-s");
        let r = args.iter().any(|a| a == "--repeat" || a == "-r");
        // Collect positional args (not flags, not values after flag options)
        let mut positional = Vec::new();
        let value_flags = ["--eq", "-e", "--fx", "--crossfade", "-x", "--rg-mode", "--device", "--theme"];
        let mut skip_next = false;
        for arg in &args[1..] {
            if skip_next { skip_next = false; continue; }
            if value_flags.contains(&arg.as_str()) { skip_next = true; continue; }
            if flags.contains(&arg.as_str()) { continue; }
            if arg.starts_with("--") || (arg.starts_with('-') && arg.len() == 2) {
                eprintln!("Unknown option: {}", arg);
                eprintln!("Run with --help for usage information");
                std::process::exit(1);
            }
            positional.push(PathBuf::from(arg));
        }
        if positional.is_empty() {
            eprintln!("No input files or folders specified");
            std::process::exit(1);
        }
        (positional, s, if r { state::RepeatMode::All } else { state::RepeatMode::Off })
    };
    let hq_resampler = args.iter().any(|a| a == "--quality" || a == "-q");
    let eq_arg = args.iter().position(|a| a == "--eq" || a == "-e")
        .and_then(|i| args.get(i + 1).cloned());
    let fx_arg = args.iter().position(|a| a == "--fx")
        .and_then(|i| args.get(i + 1).cloned());
    let crossfade_secs: u32 = args.iter().position(|a| a == "--crossfade" || a == "-x")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let rg_mode: RgMode = args.iter().position(|a| a == "--rg-mode")
        .and_then(|i| args.get(i + 1))
        .map(|s| match s.to_lowercase().as_str() {
            "album" => RgMode::Album,
            "off" => RgMode::Off,
            _ => RgMode::Track,
        })
        .unwrap_or(RgMode::Track);
    let device_arg: Option<String> = args.iter().position(|a| a == "--device")
        .and_then(|i| args.get(i + 1).cloned());
    let exclusive = args.iter().any(|a| a == "--exclusive");
    let cover_enabled = !args.iter().any(|a| a == "--no-cover");
    let theme_arg: Option<theme::ThemeKind> = args.iter().position(|a| a == "--theme")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| {
            theme::ThemeKind::from_str(s).or_else(|| {
                eprintln!("Unknown theme '{}' (expected: classic, minimal, hifi)", s);
                None
            })
        });
    // Persistent user preferences (config.json) — applies on every launch.
    let app_config = config::load();

    let mut playlist = {
        let mut combined = Vec::new();
        for src in &source_paths {
            match build_playlist(src, false) {
                Ok(tracks) => combined.extend(tracks),
                Err(e) => {
                    if source_paths.len() == 1 {
                        return Err(e);
                    }
                    eprintln!("Skipping {}: {}", src.display(), e);
                }
            }
        }
        if combined.is_empty() {
            return Err("No audio files found".into());
        }
        // Deduplicate by canonical path
        let mut seen = std::collections::HashSet::new();
        combined.retain(|p| {
            let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            seen.insert(key)
        });
        if shuffle { shuffle_list(&mut combined); }
        combined
    };
    let state = Arc::new(PlayerState::new());
    state.total_tracks.store(playlist.len(), Ordering::Relaxed);

    // Load EQ presets (built-in + custom from ~/.config/keet/eq/)
    let mut eq_presets = eq::builtin_presets();
    eq_presets.extend(eq::load_custom_presets());
    state.eq_preset_count.store(eq_presets.len(), Ordering::Relaxed);

    // Set initial EQ preset from --eq argument
    if let Some(ref eq_name) = eq_arg {
        if let Some(idx) = eq_presets.iter().position(|p| p.name.eq_ignore_ascii_case(eq_name)) {
            state.eq_preset_index.store(idx, Ordering::Relaxed);
        } else if let Ok(contents) = std::fs::read_to_string(eq_name) {
            if let Ok(preset) = serde_json::from_str::<eq::EqPreset>(&contents) {
                eq_presets.push(preset);
                state.eq_preset_count.store(eq_presets.len(), Ordering::Relaxed);
                state.eq_preset_index.store(eq_presets.len() - 1, Ordering::Relaxed);
            }
        }
    }

    // Load effects presets (built-in + custom from ~/.config/keet/effects/)
    let mut fx_presets = effects::builtin_presets();
    fx_presets.extend(effects::load_custom_presets());
    state.effects_preset_count.store(fx_presets.len(), Ordering::Relaxed);

    if let Some(ref fx_name) = fx_arg {
        if let Some(idx) = fx_presets.iter().position(|p| p.name.eq_ignore_ascii_case(fx_name)) {
            state.effects_preset_index.store(idx, Ordering::Relaxed);
        } else if let Ok(contents) = std::fs::read_to_string(fx_name) {
            if let Ok(preset) = serde_json::from_str::<effects::EffectsPreset>(&contents) {
                fx_presets.push(preset);
                state.effects_preset_count.store(fx_presets.len(), Ordering::Relaxed);
                state.effects_preset_index.store(fx_presets.len() - 1, Ordering::Relaxed);
            }
        }
    }

    state.crossfade_secs.store(crossfade_secs, Ordering::Relaxed);
    state.rg_mode.store(rg_mode as u8, Ordering::Relaxed);
    state.exclusive.store(exclusive, Ordering::Relaxed);

    // Load crossfeed presets: built-ins plus any custom JSON in the config dir.
    let mut cf_presets = crossfeed::builtin_presets();
    cf_presets.extend(crossfeed::load_custom_presets());
    state.crossfeed_preset_count.store(cf_presets.len(), Ordering::Relaxed);
    let cf_presets = Arc::new(cf_presets);

    // Restore resume state if resuming
    let mut resume_position: i64 = 0;

    if let Some(ref rs) = resume_state_loaded {
        state.volume.store(rs.volume, Ordering::Relaxed);
        resume_position = rs.position_secs.round() as i64;

        // Restore EQ preset by name
        if let Some(idx) = eq_presets.iter().position(|p| p.name == rs.eq_preset) {
            state.eq_preset_index.store(idx, Ordering::Relaxed);
        }
        // Restore a Custom (edited) EQ, if that's what was saved. Older
        // state.json files carry gains only — the parametric fields then fall
        // back per band to the graphic defaults (peak at the ISO centre, Q 1.41).
        if rs.eq_custom == Some(true) {
            if let Some(ref g) = rs.eq_gains {
                let bands: [eq::BandSettings; eq::EQ_BANDS] = std::array::from_fn(|i| {
                    let d = eq::BandSettings::inert(i);
                    eq::BandSettings {
                        kind: rs.eq_types.as_ref()
                            .and_then(|t| t.get(i))
                            .and_then(|n| eq::BandType::from_name(n))
                            .unwrap_or(d.kind),
                        freq: rs.eq_freqs.as_ref()
                            .and_then(|f| f.get(i).copied())
                            .unwrap_or(d.freq),
                        gain: g.get(i).copied().unwrap_or(0.0),
                        q: rs.eq_qs.as_ref()
                            .and_then(|q| q.get(i).copied())
                            .unwrap_or(d.q),
                    }
                    .clamped()
                });
                state.set_eq_bands(&bands);
                state.set_eq_preamp_db(rs.eq_preamp.unwrap_or(0.0));
                state.eq_custom.store(true, Ordering::Relaxed);
            }
        }
        // Restore FX preset by name
        if let Some(idx) = fx_presets.iter().position(|p| p.name == rs.effects_preset) {
            state.effects_preset_index.store(idx, Ordering::Relaxed);
        }
        // Restore RG mode by name
        if let Some(ref rg_str) = rs.rg_mode {
            let rg = match rg_str.as_str() {
                "album" => RgMode::Album,
                "off" => RgMode::Off,
                _ => RgMode::Track,
            };
            state.rg_mode.store(rg as u8, Ordering::Relaxed);
        }
        // Restore crossfeed preset by name
        if let Some(ref cf_name) = rs.crossfeed_preset {
            if let Some(idx) = cf_presets.iter().position(|p| p.name.eq_ignore_ascii_case(cf_name)) {
                state.crossfeed_preset_index.store(idx, Ordering::Relaxed);
            }
        }
        // Restore balance
        if let Some(bal) = rs.balance {
            state.balance.store(bal.clamp(-100, 100), Ordering::Relaxed);
        }
    }
    // Resolve the launch theme: --theme flag → config.json default → resumed
    // last-session theme → Classic. The config default applies on every launch
    // (including with explicit source paths), unlike the resume theme.
    let config_theme = app_config.theme.as_deref().and_then(theme::ThemeKind::from_str);
    let resume_theme = resume_state_loaded
        .as_ref()
        .and_then(|rs| rs.theme.as_deref())
        .and_then(theme::ThemeKind::from_str);
    state.set_theme(theme::resolve_theme(theme_arg, config_theme, resume_theme));

    // Apply remaining config.json defaults. Each overrides the resumed value but
    // yields to an explicit CLI flag for the same setting. (CLI flags only occur
    // with explicit paths, and resume only on a bare launch, so checking flag
    // presence gives the right priority in both modes.)
    if let Some(v) = app_config.viz.as_deref().and_then(state::VizMode::from_str) {
        state.viz_mode.store(v as u8, Ordering::Relaxed);
    }
    if !args.iter().any(|a| a == "--rg-mode") {
        if let Some(m) = app_config.rg_mode.as_deref().and_then(RgMode::from_str) {
            state.rg_mode.store(m as u8, Ordering::Relaxed);
        }
    }
    if eq_arg.is_none() {
        if let Some(name) = app_config.eq.as_deref() {
            if let Some(idx) = eq_presets.iter().position(|p| p.name.eq_ignore_ascii_case(name)) {
                state.eq_preset_index.store(idx, Ordering::Relaxed);
            }
        }
    }
    if let Some(name) = app_config.crossfeed.as_deref() {
        if let Some(idx) = cf_presets.iter().position(|p| p.name.eq_ignore_ascii_case(name)) {
            state.crossfeed_preset_index.store(idx, Ordering::Relaxed);
        }
    }

    // Override device/exclusive from resume state when resuming with no args
    let mut device_arg = device_arg;
    let mut exclusive = exclusive;
    if args.len() < 2 {
        if let Some(ref rs) = resume_state_loaded {
            if device_arg.is_none() {
                device_arg = rs.device.clone();
            }
            if !exclusive {
                exclusive = rs.exclusive.unwrap_or(false);
            }
        }
    }

    let eq_presets = Arc::new(eq_presets);
    let fx_presets = Arc::new(fx_presets);

    let inner_w = 57;
    let title = "Keet";
    use std::fmt::Write as FmtWrite;

    let build_banner_box = |shuffle: bool, repeat_mode: state::RepeatMode, state: &PlayerState| -> String {
        let eq_name = &eq_presets[state.eq_index()].name;
        let fx_name = &fx_presets[state.effects_index()].name;
        let cf_name = &cf_presets[state.crossfeed_index()].name;
        let bal_val = state.balance_value();
        let theme_kind = state.theme_kind();

        match theme_kind {
            theme::ThemeKind::HiFi | theme::ThemeKind::Minimal => {
                // HiFi and Minimal render their own anchor row inside the
                // rewind region (header strip / wordmark), so the static
                // banner area is zero-height.
                let _ = (shuffle, repeat_mode, eq_name, fx_name, cf_name, bal_val);
                String::new()
            }
            _ => {
                // Classic boxed banner.
                let pad_left = (inner_w - title.len()) / 2;
                let pad_right = inner_w - title.len() - pad_left;
                let eq_info = if eq_name != "Flat" { format!(" | EQ: {}", eq_name) } else { String::new() };
                let fx_info = if fx_name != "None" { format!(" | FX: {}", fx_name) } else { String::new() };
                let xfade_info = if crossfade_secs > 0 { format!(" | xfade: {}s", crossfade_secs) } else { String::new() };
                let cf_info = if cf_name != "Off" { format!(" | crossfeed: {}", cf_name) } else { String::new() };
                let bal_info = if bal_val != 0 {
                    if bal_val < 0 { format!(" | bal: L{}%", -bal_val) } else { format!(" | bal: R{}%", bal_val) }
                } else { String::new() };
                let info = format!("{}{}{}{}{}{}{}{}",
                    if shuffle { "shuffle" } else { "sequential" },
                    repeat_mode.label(),
                    if hq_resampler { " | HQ" } else { "" },
                    eq_info, fx_info, xfade_info, cf_info, bal_info);
                let info_pad = inner_w.saturating_sub(info.chars().count() + 2);
                let mut s = String::new();
                writeln!(s, "╔{}╗", "═".repeat(inner_w)).ok();
                writeln!(s, "║{}{}{}║", " ".repeat(pad_left), title, " ".repeat(pad_right)).ok();
                writeln!(s, "╠{}╣", "═".repeat(inner_w)).ok();
                writeln!(s, "║  {}{}║", info, " ".repeat(info_pad)).ok();
                writeln!(s, "╚{}╝", "═".repeat(inner_w)).ok();
                s
            }
        }
    };

    // Create UI state before the banner so shuffle/repeat have a single home
    // (ui.*). The parsed `shuffle`/`repeat_mode` locals feed it once here and
    // are not read again — every later reader (banner rebuild, main loop) uses
    // ui.shuffle / ui.repeat_mode.
    let metadata_cache = metadata::MetadataCache::new(playlist.len());
    let mut ui = UiState::new(source_paths, std::sync::Arc::clone(&metadata_cache));
    ui.shuffle = shuffle;
    ui.repeat_mode = repeat_mode;
    state.repeat_mode.store(repeat_mode as u8, Ordering::Relaxed);

    let banner_box = build_banner_box(ui.shuffle, ui.repeat_mode, &state);
    let mut banner_tail = String::new();

    // Audio setup
    let host = cpal::default_host();
    let current_output_rate = {
        let device = if let Some(ref dev_name) = device_arg {
            audio::find_device_by_name(&host, dev_name).unwrap_or_else(|| {
                eprintln!("Warning: Device '{}' not found, using default", dev_name);
                host.default_output_device().expect("No output device")
            })
        } else {
            host.default_output_device().ok_or("No output device")?
        };
        let device_name = device.description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "Unknown device".to_string());
        // Device info banner is Classic-only — Minimal/HiFi surface device
        // and rate inline (SIGNAL block / header strip), and the extra rows
        // would push content past the terminal bottom.
        let classic = state.theme_kind() == theme::ThemeKind::Classic;
        if classic {
            writeln!(banner_tail, "\nDevice: {}", device_name).ok();
        }

        // Fix stale sample rate on Bluetooth devices (CoreAudio can get stuck at wrong rate)
        let bt_rate = fix_bluetooth_sample_rate();
        if let Some(rate) = bt_rate {
            if classic {
                writeln!(banner_tail, "Bluetooth device detected, using native {}Hz", rate).ok();
            }
        }

        let default_config = device.default_output_config()?;
        let rate = bt_rate.unwrap_or_else(|| default_config.sample_rate());
        let default_channels = default_config.channels();
        if classic {
            writeln!(banner_tail, "Initial output: {}Hz (device default: {}ch)", rate, default_channels).ok();
        }
        rate
    };

    // Stats monitor
    let mut stats = StatsMonitor::new();

    // OS media transport controls (media keys, AirPods, Bluetooth headphones)
    let mut media_controls = media_keys::setup(Arc::clone(&state));

    // Verbose banner help is Classic-only. Minimal and HiFi have their own
    // footer key bars per the design handoff, and the extra rows would push
    // content past the terminal bottom and cause the kitty cover to scroll
    // out of its banner slot.
    if state.theme_kind() == theme::ThemeKind::Classic {
        writeln!(banner_tail, "\n{0}{{Space}}{1} Pause  {0}{{↑/↓}}{1} Track  {0}{{←/→}}{1} Seek  {0}{{+/-}}{1} Vol  {0}{{[/]}}{1} Bal  {0}{{Q}}{1} Quit",
            "\x1B[2m", "\x1B[0m").ok();
        writeln!(banner_tail, "{0}{{E}}{1} EQ  {0}{{X}}{1} FX  {0}{{C}}{1} Crossfeed  {0}{{F}}{1} Fader  {0}{{V/B}}{1} Viz  {0}{{I}}{1} Info  {0}{{Y}}{1} Lyrics",
            "\x1B[2m", "\x1B[0m").ok();
        writeln!(banner_tail, "{0}{{L}}{1} List  {0}{{R}}{1} Rescan  {0}{{Shift+R}}{1} Repeat  {0}{{Z}}{1} Shuffle  {0}{{O}}{1} Open  {0}{{P}}{1} Pick\n",
            "\x1B[2m", "\x1B[0m").ok();
    }

    // Print banner and count its lines
    let banner = format!("{}{}", banner_box, banner_tail);
    print!("{}", banner);
    let banner_lines = banner.lines().count();

    terminal::enable_raw_mode()?;

    // Hide cursor to prevent flickering
    print!("\x1B[?25l");
    io::stdout().flush().ok();

    // Probe the terminal cell size (Sixel-only, bounded ~300 ms) for
    // pixel-exact spectrogram sizing. Must happen after raw mode and before
    // the first poll_input — a late reply's Esc would read as the quit key.
    let resize_during_probe = cover::probe_cell_metrics();

    ui.banner_lines = banner_lines;
    ui.banner_text = banner;
    ui.cover_enabled = cover_enabled;
    ui.banner_tail = banner_tail;
    ui.terminal_resized = resize_during_probe;
    ui.scan_handle = Some(metadata::spawn_metadata_scan(
        playlist.clone(),
        std::sync::Arc::clone(&metadata_cache),
    ));
    // Arm the one-shot artist→album auto-sort for folder sources; it fires once
    // the scan above loads tags (see poll_auto_sort in the main loop).
    arm_auto_sort(&mut ui);

    // Set starting track for resume
    if let Some(ref rs) = resume_state_loaded {
        if let Some(idx) = playlist.iter().position(|p| p.to_string_lossy() == rs.track_path.as_str()) {
            ui.current = idx;
        }
    }

    let mut prev_frame_lines: usize = usize::MAX;
    // Tracks whether the Kitty analysis-spectrogram image was placed last frame,
    // so we can delete it (by id) when the user switches away from that mode.
    let mut prev_viz_image_shown = false;

    // --- Persistent audio setup (created once, reused across all tracks) ---
    let mut device = if let Some(ref dev_name) = device_arg {
        audio::find_device_by_name(&host, dev_name).unwrap_or_else(|| {
            eprintln!("Warning: Device '{}' not found, using default", dev_name);
            host.default_output_device().expect("No output device")
        })
    } else {
        host.default_output_device().ok_or("No output device")?
    };

    // Probe first track's sample rate to set output rate
    let source_rate = probe_sample_rate(&playlist[ui.current]).unwrap_or(44100);
    let persistent_output_rate = set_output_sample_rate(source_rate, current_output_rate, &device);
    let actual_device_rate = match device.default_output_config() {
        Ok(config) => config.sample_rate(),
        Err(_) => persistent_output_rate,
    };
    // Output channel count comes from the device, not an assumption. WASAPI
    // shared mode only accepts the mixer's own format, so a non-stereo device
    // rejects a hardcoded 2 outright. The ring stays stereo either way — the
    // callback fans it out.
    let out_channels: u16 = device
        .default_output_config()
        .map(|c| c.channels())
        .unwrap_or(2)
        .max(1);
    let mut stream_rate = {
        let rate_supported = device.supported_output_configs()
            .map(|configs| {
                configs.into_iter().any(|c| {
                    c.channels() == out_channels
                        && c.min_sample_rate() <= actual_device_rate
                        && actual_device_rate <= c.max_sample_rate()
                })
            })
            .unwrap_or(false);
        if rate_supported { actual_device_rate } else {
            device.default_output_config()
                .map(|c| c.sample_rate())
                .unwrap_or(48000)
        }
    };
    state.output_rate.store(stream_rate as u64, Ordering::Relaxed);

    let is_wsl = cfg!(target_os = "linux") && std::fs::read_to_string("/proc/version")
        .map(|v| v.contains("microsoft") || v.contains("WSL"))
        .unwrap_or(false);
    let buffer_size = if cfg!(target_os = "windows") || is_wsl {
        cpal::BufferSize::Fixed(2048)
    } else {
        cpal::BufferSize::Default
    };

    let saved_buffer_size = buffer_size;

    let (mut prod, mut viz_cons, mut stream) =
        rebuild_stream(&device, stream_rate, out_channels, saved_buffer_size, &state)?;
    stream.play()?;

    // Set exclusive mode if requested (macOS only: hog mode + per-track rate switching)
    let mut hog_device_id: Option<u32> = None;
    if exclusive {
        match audio::set_exclusive_mode(&device) {
            Ok(id) => {
                hog_device_id = Some(id);
                println!("Exclusive mode: hog + per-track rate switching");
            }
            Err(e) => {
                if cfg!(target_os = "macos") {
                    // macOS: hog mode failed but rate switching still works via CoreAudio
                    eprintln!("Note: Hog mode unavailable ({}). Per-track rate switching is still active.", e);
                } else {
                    // Other platforms: exclusive mode is not supported at all
                    eprintln!("Note: {}", e);
                    state.exclusive.store(false, Ordering::Relaxed);
                }
            }
        }
    }

    let mut last_transition_count: usize = 0;

    // Media-key now-playing throttle state (see the update site in the UI loop).
    let mut last_mk_push = Instant::now() - Duration::from_secs(2);
    let mut last_mk_paused = false;

    // Persist resume state off the main thread: serializing + writing JSON on a
    // slow/network $HOME could otherwise stall the UI at every track transition.
    // A single saver thread serializes the writes (so the temp-file rename can't
    // race), and the channel is drained + joined at shutdown so the final save
    // is never lost.
    let (save_tx, save_rx) = std::sync::mpsc::channel::<ResumeState>();
    let saver_handle = thread::spawn(move || {
        while let Ok(rs) = save_rx.recv() {
            save_state(&rs);
        }
    });

    'playlist: loop {
        if state.should_quit() { break; }

        // Repeat-cycle check
        if ui.current >= playlist.len() {
            if ui.repeat_mode != state::RepeatMode::Off {
                let old_playlist = playlist.clone();

                let has_dir = ui.source_paths.iter().any(|p| p.is_dir());
                if has_dir {
                    let mut combined = Vec::new();
                    for src in &ui.source_paths {
                        if let Ok(tracks) = build_playlist(src, false) {
                            combined.extend(tracks);
                        }
                    }
                    if !combined.is_empty() {
                        // Single pass: canonicalize each path once, then dedupe and
                        // filter-by-removed in one retain. Previously each retain
                        // re-ran canonicalize() on every entry.
                        let mut seen = std::collections::HashSet::new();
                        let removed = &ui.removed_paths;
                        combined.retain(|p| {
                            let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                            if removed.contains(&key) { return false; }
                            seen.insert(key)
                        });
                        if ui.shuffle { shuffle_list(&mut combined); }
                        playlist = combined;
                        state.total_tracks.store(playlist.len(), Ordering::Relaxed);
                    }
                } else {
                    // Non-directory sources: filter removed tracks from existing playlist
                    if !ui.removed_paths.is_empty() {
                        playlist.retain(|p| {
                            let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                            !ui.removed_paths.contains(&key)
                        });
                        state.total_tracks.store(playlist.len(), Ordering::Relaxed);
                    }
                    if ui.shuffle { shuffle_list(&mut playlist); }
                }

                // Everything gone (in-app removals + files deleted on disk):
                // nothing left to play. Without this guard the track fetch
                // below would index into an empty playlist and panic.
                if playlist.is_empty() {
                    break;
                }

                // Reindex metadata cache
                crate::ui::reindex_and_restart_scan(&mut ui, &playlist, &old_playlist);
                // Re-arm the artist→album auto-sort: the rebuild above is
                // path-ordered, so without this a folder played on repeat-all
                // reverts to filename order instead of staying tag-sorted.
                arm_auto_sort(&mut ui);

                ui.current = 0;
            } else {
                break;
            }
        }

        // Reset state for new producer
        state.current_track.store(ui.current, Ordering::Relaxed);
        state.producer_done.store(false, Ordering::Relaxed);
        state.track_info_ready.store(false, Ordering::Relaxed);
        state.skip_next.store(false, Ordering::Relaxed);
        state.skip_prev.store(false, Ordering::Relaxed);
        state.buffer_level.store(0, Ordering::Relaxed);
        if let Ok(mut err) = state.decode_error.lock() { *err = None; }

        let track_path = &playlist[ui.current];
        let mut filename = ui.metadata_cache.display_name(ui.current, track_path);
        let mut track_ext = track_path.extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        // Spawn producer thread (continuous — decodes multiple tracks)
        let playlist_snapshot = playlist.clone();
        let start_idx = ui.current;
        let state_clone = Arc::clone(&state);
        let eq_presets_clone = Arc::clone(&eq_presets);
        let fx_presets_clone = Arc::clone(&fx_presets);
        let cf_presets_clone = Arc::clone(&cf_presets);
        let hq = hq_resampler;
        let sr = stream_rate;
        let xfade = crossfade_secs;
        let mut prod_for_thread = prod;

        let producer_handle = thread::spawn(move || {
            let mut eq_chain = eq::EqChain::new();
            if state_clone.is_eq_custom() {
                eq_chain.load_bands(&state_clone.eq_bands_array(), state_clone.eq_preamp_db(), sr as f32);
            } else {
                eq_chain.load_preset(&eq_presets_clone[state_clone.eq_index()], sr as f32);
            }
            let mut fx_chain = effects::EffectsChain::new(sr as f32);
            fx_chain.load_preset(&fx_presets_clone[state_clone.effects_index()], sr as f32);
            let mut cf_filter = crossfeed::CrossfeedFilter::new();
            cf_filter.load_preset(&cf_presets_clone[state_clone.crossfeed_index()], sr as f32);

            decode_playlist(
                &playlist_snapshot, start_idx,
                &mut prod_for_thread, &state_clone, sr, hq,
                &mut eq_chain, &eq_presets_clone,
                &mut fx_chain, &fx_presets_clone,
                xfade,
                &mut cf_filter, &cf_presets_clone,
            );
            prod_for_thread // Return producer ownership
        });

        // Stage 1: wait for the producer to open the file and publish track info
        // (fast, usually < 50ms). Once this is set, sample rate / bits / duration
        // are available so we can build track_info and show the new status line
        // while the buffer fills underneath us.
        while !state.track_info_ready.load(Ordering::Relaxed)
              && !state.producer_done.load(Ordering::Relaxed)
              && !state.should_quit()
        {
            poll_input(&state, &mut ui, &mut playlist);
            thread::sleep(Duration::from_millis(10));
        }

        // If producer failed before track info, skip
        if state.producer_done.load(Ordering::Relaxed)
           && !state.track_info_ready.load(Ordering::Relaxed)
        {
            match producer_handle.join() {
                Ok(p) => prod = p,
                Err(_) => break 'playlist,
            }
            let err_msg = state.decode_error.lock().ok().and_then(|mut e| e.take());
            if let Some(msg) = err_msg {
                ui.set_status(format!("Skip: {}", msg));
            }
            ui.current += 1;
            // Force a full redraw so the next track's status line starts clean
            // instead of leaving orphan lines from the previous render.
            ui.terminal_resized = true;
            prev_frame_lines = usize::MAX;
            continue 'playlist;
        }

        // Resume: seek to saved position (only on first track after resume)
        if resume_position > 0 {
            state.seek(resume_position);
            resume_position = 0;
        }

        // Build track info string
        let src_rate = state.sample_rate.load(Ordering::Relaxed) as u32;
        let channels = state.channels.load(Ordering::Relaxed);
        let bits = state.bits_per_sample.load(Ordering::Relaxed);
        let ch_str = match channels {
            1 => "mono".to_string(),
            2 => "stereo".to_string(),
            n => format!("{}ch", n),
        };
        let rate_str = if src_rate != stream_rate {
            format!("{}→{}Hz", src_rate, stream_rate)
        } else {
            format!("{}Hz", src_rate)
        };
        let mut track_info = format!("{} • {}bit {} • {}", format_time(state.total_secs()), bits, ch_str, rate_str);

        // Load lyrics off the main thread so skip stays responsive.
        let dur = { let t = state.total_secs(); if t > 0.0 { Some(t as u32) } else { None } };
        ui.lyrics_scroll = 0;
        ui.lyrics_auto_scroll = true;
        let lyrics_path = playlist[ui.current].clone();
        spawn_lyrics_worker(&mut ui, lyrics_path.clone(), dur);
        spawn_cover_worker(&mut ui, lyrics_path);

        // Visualization analyzer (created before the startup wait so print_status
        // can draw the waveform/lissajous/spectrogram viz modes during buffering).
        let mut viz_analyser = VizAnalyser::new(stream_rate);
        let mut viz_scratch = Vec::with_capacity(VIZ_BUFFER_SIZE);

        // Stage 2: wait for the ring buffer to fill enough that the audio callback
        // won't underrun, while refreshing the status line so the user sees the new
        // track name immediately instead of staring at the old one.
        {
            let current_eq = &eq_presets[state.eq_index()];
            let current_fx = &fx_presets[state.effects_index()].name;
            let current_cf = &cf_presets[state.crossfeed_index()].name;
            // Wait for ~1 second of audio in the ring before entering the
            // steady-state loop, so the already-running callback can't underrun
            // right after the track starts. Using stream_rate (rather than a
            // fraction of the raw ring size) keeps the cushion consistent
            // across output rates.
            let startup_threshold = stream_rate as usize * 2;
            while state.buffer_level.load(Ordering::Relaxed) < startup_threshold
                  && !state.producer_done.load(Ordering::Relaxed)
                  && !state.should_quit()
            {
                poll_input(&state, &mut ui, &mut playlist);
                // Begin/end synchronized update (DEC mode 2026): present each frame
                // atomically so terminals (notably Windows Terminal) don't show the
                // mid-redraw erase-then-repaint as flickering black lines. Ignored by
                // terminals that don't support it.
                print!("\x1B[?2026h");
                prev_frame_lines = print_status(&state, &mut ui, &filename, &track_info, &track_ext, current_eq, current_fx, current_cf, &mut stats, prev_frame_lines, &playlist, &viz_analyser);
                print!("\x1B[?2026l");
                io::stdout().flush().ok();
                thread::sleep(Duration::from_millis(20));
            }
        }

        // Update OS media transport. Title/artist/album come from the cache
        // when the scan has reached this track; otherwise the display name
        // stands in for the title (same fallback the UI itself uses).
        if let Some(ref mut mc) = media_controls {
            let (mk_artist, mk_album) = ui.metadata_cache.artist_album(ui.current);
            let mk_title = ui.metadata_cache.title(ui.current).unwrap_or_else(|| filename.clone());
            media_keys::update_metadata(
                mc, &mk_title, mk_artist.as_deref(), mk_album.as_deref(), state.total_secs(),
            );
            media_keys::update_playback(mc, state.is_paused(), 0.0);
        }

        // Playback loop (stays here across natural track transitions)
        let mut last_ui = Instant::now();

        loop {
            // Input. Also honor a quit that was raised while this loop wasn't
            // watching (the stage-1/2 buffering waits poll input but discard
            // the quit return) — otherwise Q during buffering keeps playing
            // until the ring drains, or hangs entirely when paused.
            if poll_input(&state, &mut ui, &mut playlist) || state.should_quit() {
                print!("\x1B[?25h");
                if prev_frame_lines != usize::MAX {
                    // One row above the frame's anchor line (the gap row under
                    // the banner), then erase down to wipe the whole frame.
                    let up = 1 + prev_frame_lines;
                    print!("\x1B[{}F", up);
                }
                print!("\x1B[J");
                io::stdout().flush().ok();
                let _ = save_tx.send(build_resume_state(&ui, &playlist, &state, &eq_presets, &fx_presets, &cf_presets, &device_arg));
                if let Some(id) = hog_device_id {
                    audio::release_exclusive_mode(id);
                }
                // Producer will exit when state.should_quit() is true
                let _ = producer_handle.join();
                break 'playlist;
            }

            // Fire the one-shot artist→album auto-sort once the metadata scan
            // has loaded tags (no-op until armed + scan finished + not shuffling).
            poll_auto_sort(&state, &mut ui, &mut playlist);
            // Keep the library tree fresh while it's showing (no-op otherwise).
            poll_library_tree(&mut ui, &playlist);

            // Check for track transitions from the producer
            let current_count = state.track_transition_count.load(Ordering::Acquire);
            if current_count != last_transition_count {
                let new_index = state.producer_track_index.load(Ordering::Relaxed);
                last_transition_count = current_count;

                // Surface mid-playlist decode failures. The producer skips a
                // bad file and signals the next track; without this the error
                // text it stored was never shown anywhere.
                let skip_err = state.decode_error.lock().ok().and_then(|mut e| e.take());
                if let Some(msg) = skip_err {
                    ui.set_status(format!("Skip: {}", msg));
                }

                // Playlist was modified — producer's new_index is from the stale snapshot.
                // Schedule a jump to the right track; skip the rest of this transition so we
                // don't display/fetch-lyrics for the wrong file. The jump_to_track check on the
                // next loop iteration will respawn the producer with the fresh playlist.
                if ui.playlist_dirty {
                    ui.playlist_dirty = false;
                    let target = if ui.current_track_removed {
                        ui.current_track_removed = false;
                        ui.current
                    } else {
                        (ui.current + 1).min(playlist.len().saturating_sub(1))
                    };
                    state.jump_to(target);
                } else if new_index < playlist.len() {
                    ui.current = new_index;
                    ui.enqueue_count = 0;
                    state.current_track.store(ui.current, Ordering::Relaxed);

                    if ui.view_mode == state::ViewMode::Playlist && ui.filtered_indices.is_empty() {
                        ui.cursor = ui.current;
                    }

                    // Update display info for new track
                    let new_path = &playlist[ui.current];
                    filename = ui.metadata_cache.display_name(ui.current, new_path);
                    track_ext = new_path.extension()
                        .map(|e| e.to_string_lossy().to_lowercase())
                        .unwrap_or_default();

                    ui.lyrics_scroll = 0;
                    ui.lyrics_auto_scroll = true;
                    let dur = { let t = state.total_secs(); if t > 0.0 { Some(t as u32) } else { None } };
                    spawn_lyrics_worker(&mut ui, new_path.clone(), dur);
                    spawn_cover_worker(&mut ui, new_path.clone());

                    let src_rate = state.sample_rate.load(Ordering::Relaxed) as u32;
                    let channels = state.channels.load(Ordering::Relaxed);
                    let bits = state.bits_per_sample.load(Ordering::Relaxed);
                    let ch_str = match channels {
                        1 => "mono".to_string(),
                        2 => "stereo".to_string(),
                        n => format!("{}ch", n),
                    };
                    let rate_str = if src_rate != stream_rate {
                        format!("{}→{}Hz", src_rate, stream_rate)
                    } else {
                        format!("{}Hz", src_rate)
                    };
                    track_info = format!("{} • {}bit {} • {}", format_time(state.total_secs()), bits, ch_str, rate_str);

                    if let Some(ref mut mc) = media_controls {
                        let (mk_artist, mk_album) = ui.metadata_cache.artist_album(ui.current);
                        let mk_title = ui.metadata_cache.title(ui.current)
                            .unwrap_or_else(|| filename.clone());
                        media_keys::update_metadata(
                            mc, &mk_title, mk_artist.as_deref(), mk_album.as_deref(),
                            state.total_secs(),
                        );
                        media_keys::update_playback(mc, state.is_paused(), 0.0);
                    }

                    let _ = save_tx.send(build_resume_state(&ui, &playlist, &state, &eq_presets, &fx_presets, &cf_presets, &device_arg));
                }
            }

            // Skip-prev or jump: join producer, respawn
            if state.skip_prev.load(Ordering::Relaxed) || state.jump_to_track.load(Ordering::Relaxed) >= 0 {
                match producer_handle.join() {
                    Ok(p) => prod = p,
                    Err(_) => break 'playlist,
                }
                // Flush ring buffer, and wait for the callback to actually
                // consume the drain request before the respawned producer can
                // push — otherwise the drain may fire late and discard the new
                // track's first samples.
                if state.ring_capacity.load(Ordering::Relaxed) - prod.slots() > 0 {
                    state.reset_consumer_counter.store(true, Ordering::Relaxed);
                    await_consumer_drain(&state);
                }
                if let Some(target) = state.take_jump() {
                    ui.current = target;
                } else if state.take_skip_prev() {
                    ui.current = ui.current.saturating_sub(1);
                }
                ui.enqueue_count = 0;
                continue 'playlist;
            }

            // Exclusive mode: rate change needed (producer detected different sample rate)
            if state.rate_change_needed.swap(false, Ordering::Relaxed) {
                // Wait for the buffer to drain so the current track finishes before we
                // tear the stream down. A paused stream never drains, so wait out the
                // pause instead of bailing — bailing here would truncate the buffered
                // tail and click. The rate switch simply defers until playback resumes.
                while !state.should_quit()
                    && (state.is_paused() || state.buffer_level.load(Ordering::Relaxed) > 0)
                {
                    thread::sleep(Duration::from_millis(10));
                }

                match producer_handle.join() {
                    Ok(_) => {} // Old producer dropped; new ring buffer below
                    Err(_) => break 'playlist,
                }

                let new_rate = state.next_track_rate.load(Ordering::Relaxed);
                let max_rate = audio::max_supported_rate(&device);
                let target_rate = new_rate.min(max_rate);
                let actual_rate = set_output_sample_rate(target_rate, stream_rate, &device);
                stream_rate = actual_rate;
                state.output_rate.store(stream_rate as u64, Ordering::Relaxed);

                // Drop old stream before creating the new ring buffer.
                drop(stream);

                let (new_prod, new_viz_cons, new_stream) =
                    rebuild_stream(&device, stream_rate, out_channels, saved_buffer_size, &state)?;
                prod = new_prod;
                viz_cons = new_viz_cons;
                stream = new_stream;
                stream.play()?;

                // Continue playlist from the track that needs the new rate
                // (viz_analyser is re-created at the top of each 'playlist iteration)
                let new_idx = state.producer_track_index.load(Ordering::Relaxed);
                if new_idx < playlist.len() {
                    ui.current = new_idx;
                }
                continue 'playlist;
            }

            // Stream error recovery (device disconnected, AirPods removed, etc.)
            if state.stream_error.swap(false, Ordering::Relaxed) {
                // Try to switch to the current default output device
                if let Some(new_device) = host.default_output_device() {
                    // Signal the producer to exit — it may be stuck in the
                    // buffer-full sleep loop since the audio callback stopped
                    // draining the ring buffer.
                    state.jump_to(ui.current);
                    match producer_handle.join() {
                        Ok(_) => {}
                        Err(_) => break 'playlist,
                    }
                    // Consume the unstick signal: it was only set to break the old
                    // producer out of its buffer-full sleep. Leaving it set would make
                    // the producer we respawn below peek jump_to_track >= 0 and exit
                    // immediately, wasting a spawn/join cycle before playback resumes.
                    state.take_jump();
                    drop(stream);

                    device = new_device;

                    // Re-acquire exclusive (hog) mode on the new device if it
                    // was active before the disconnect. The old hog_device_id
                    // refers to the (likely gone) previous device — release is
                    // best-effort and harmless if the device no longer exists.
                    if state.exclusive.load(Ordering::Relaxed) {
                        if let Some(old_id) = hog_device_id.take() {
                            audio::release_exclusive_mode(old_id);
                        }
                        if let Ok(id) = audio::set_exclusive_mode(&device) {
                            hog_device_id = Some(id);
                        }
                    }

                    let new_rate = device.default_output_config()
                        .map(|c| c.sample_rate())
                        .unwrap_or(48000);
                    stream_rate = new_rate;
                    state.output_rate.store(stream_rate as u64, Ordering::Relaxed);

                    match rebuild_stream(&device, stream_rate, out_channels, saved_buffer_size, &state) {
                        Ok((new_prod, new_viz_cons, new_stream)) => {
                            prod = new_prod;
                            viz_cons = new_viz_cons;
                            stream = new_stream;
                            if stream.play().is_err() {
                                break 'playlist;
                            }
                        }
                        Err(_) => break 'playlist,
                    }
                    // Resume from current track
                    continue 'playlist;
                } else {
                    // No default device right now — Windows can report none
                    // for a while after a USB DAC is yanked. The swap above
                    // already consumed the flag; put it back so we retry on
                    // the next frame instead of abandoning recovery forever.
                    // The loop keeps polling input, so quit stays responsive.
                    state.stream_error.store(true, Ordering::Relaxed);
                    ui.set_status("audio device lost — waiting for an output device".to_string());
                }
            }

            // Producer done (playlist exhausted or error)
            if state.producer_done.load(Ordering::Relaxed)
               && state.buffer_level.load(Ordering::Relaxed) == 0
            {
                thread::sleep(Duration::from_millis(200));
                match producer_handle.join() {
                    Ok(p) => prod = p,
                    Err(_) => break 'playlist,
                }

                let _ = save_tx.send(build_resume_state(&ui, &playlist, &state, &eq_presets, &fx_presets, &cf_presets, &device_arg));
                ui.current = playlist.len(); // Will trigger repeat-cycle or exit
                continue 'playlist;
            }

            // UI update. The analysis spectrogram is the one continuously-scrolling
            // mode; match the render cadence to its (sample-rate-adaptive) column
            // rate so it advances one column per frame, evenly. Other modes stay at
            // 20fps. The loop sleep below uses the SAME value, which keeps the cadence
            // even — an unequal sleep/interval is what made it judder before.
            let analysis_viz = state.viz_mode() == VizMode::SpectrogramAnalysis;
            let frame_ms: u64 = if analysis_viz {
                crate::state::spectro_frame_ms(state.output_rate.load(Ordering::Relaxed))
            } else {
                50
            };
            if last_ui.elapsed() >= Duration::from_millis(frame_ms) {
                if state.viz_mode() != VizMode::None {
                    let viz_available = viz_cons.slots();
                    if viz_available > 0 {
                        if let Ok(chunk) = viz_cons.read_chunk(viz_available) {
                            let (first, second) = chunk.as_slices();
                            viz_scratch.clear();
                            viz_scratch.extend_from_slice(first);
                            viz_scratch.extend_from_slice(second);
                            chunk.commit_all();
                            viz_analyser.process(&viz_scratch, 2, &state);
                        }
                    }
                } else {
                    let viz_available = viz_cons.slots();
                    if viz_available > 0 {
                        if let Ok(chunk) = viz_cons.read_chunk(viz_available) {
                            chunk.commit_all();
                        }
                    }
                }

                if state.show_stats() { stats.update(); }

                // Check if background lyrics fetch has completed
                if let Some(ref rx) = ui.lyrics_receiver {
                    if let Ok(lyrics) = rx.try_recv() {
                        if let Some(parsed) = lyrics {
                            ui.lyrics = Some(parsed);
                        }
                        ui.lyrics_receiver = None;
                    }
                }

                // Check if background cover fetch has completed
                if let Some(ref rx) = ui.cover_receiver {
                    if let Ok(cover) = rx.try_recv() {
                        ui.cover = cover;
                        ui.cover_receiver = None;
                        // Only Classic shows the cover, so only Classic needs
                        // a banner repaint when it arrives. For Minimal/HiFi
                        // we skip the dirty flag — the full-screen redraw
                        // would briefly flash the banner before the per-frame
                        // UI overwrites it.
                        if state.theme_kind() == theme::ThemeKind::Classic {
                            ui.banner_dirty = true;
                        }
                    }
                }

                if ui.banner_dirty {
                    ui.banner_dirty = false;
                    let new_box = build_banner_box(ui.shuffle, ui.repeat_mode, &state);
                    // banner_tail (device info + verbose key help) is Classic-only.
                    // It was built once at startup, so if the user starts in
                    // Classic and presses T to switch themes, the cached tail
                    // would otherwise bleed into the new theme's banner.
                    ui.banner_text = if state.theme_kind() == theme::ThemeKind::Classic {
                        format!("{}{}", new_box, ui.banner_tail)
                    } else {
                        new_box
                    };
                    ui.terminal_resized = true;
                }

                // Begin synchronized update (DEC mode 2026) before any frame
                // output — including the full repaint below, which now also runs
                // on every viz mode/style key — so the erase-then-repaint is
                // presented atomically. Closed after print_status. Ignored by
                // terminals that don't support it.
                print!("\x1B[?2026h");

                if ui.terminal_resized {
                    ui.terminal_resized = false;
                    // Clear entire screen and reprint banner (old lines may
                    // have wrapped at the previous terminal width).
                    // In raw mode \n doesn't imply \r, so use \r\n.
                    let term_w = terminal::size().map(|(w, _)| w as usize).unwrap_or(120);
                    // Cover overlay is Classic-only — variant-b/c mocks are
                    // text-first and the kitty image scrolls out of its slot
                    // once content exceeds terminal height. For non-Classic
                    // themes, skip compose_banner entirely so no placeholder
                    // black box is reserved in the cover slot.
                    let (composed, lines) = if state.theme_kind() == theme::ThemeKind::Classic {
                        compose_banner(&ui.banner_text, ui.cover.as_ref(), term_w)
                    } else {
                        let count = ui.banner_text.lines().count();
                        (ui.banner_text.clone(), count)
                    };
                    ui.banner_lines = lines;
                    // Remove any previously-placed kitty graphic before redrawing.
                    // No-op on terminals that don't speak the protocol.
                    let kitty_clear = if matches!(cover::detect_protocol(), cover::GraphicsProtocol::Kitty) {
                        format!("{}{}", cover::kitty_clear_escape(), cover::viz_image_clear_escape())
                    } else {
                        String::new()
                    };
                    // Home + erase-down (NOT \x1B[2J): ConPTY implements ED2 by
                    // scrolling the viewport into scrollback, so on Windows
                    // Terminal a 2J repaint shoves the whole UI out of sight
                    // instead of refreshing in place. ED0 from home erases the
                    // same cells without the scroll.
                    print!("{}\x1B[0m\x1B[H\x1B[J{}", kitty_clear, composed.replace('\n', "\r\n"));
                    prev_frame_lines = usize::MAX;
                    prev_viz_image_shown = false; // resize already cleared any viz image
                }

                // Refresh filename from the metadata cache once the background
                // scan has caught up (replaces the raw filename fallback shown
                // right after a skip).
                if ui.current < playlist.len() {
                    let fresh = ui.metadata_cache.display_name(ui.current, &playlist[ui.current]);
                    if fresh != filename {
                        filename = fresh;
                    }
                }

                let current_eq = &eq_presets[state.eq_index()];
                let current_fx = &fx_presets[state.effects_index()].name;
                let current_cf = &cf_presets[state.crossfeed_index()].name;

                // Delete the Kitty analysis-spectrogram image (by id) when it's no
                // longer being drawn — leaving the mode OR switching away from the
                // Player view (playlist/lyrics). The image is a graphics overlay, so
                // unlike the text viz it isn't painted over by the new view.
                let viz_image_shown = ui.view_mode == state::ViewMode::Player
                    && state.viz_mode() == VizMode::SpectrogramAnalysis
                    && matches!(cover::detect_protocol(), cover::GraphicsProtocol::Kitty);
                if prev_viz_image_shown && !viz_image_shown {
                    print!("{}", cover::viz_image_clear_escape());
                }
                prev_viz_image_shown = viz_image_shown;

                prev_frame_lines = print_status(&state, &mut ui, &filename, &track_info, &track_ext, current_eq, current_fx, current_cf, &mut stats, prev_frame_lines, &playlist, &viz_analyser);
                print!("\x1B[?2026l");
                io::stdout().flush().ok();

                // OS now-playing refresh: pushing one every UI frame (~20 Hz)
                // is needless objc/D-Bus traffic. Pause-state changes go out
                // immediately; the position otherwise syncs at ~1 Hz.
                if let Some(ref mut mc) = media_controls {
                    let paused_now = state.is_paused();
                    if paused_now != last_mk_paused
                        || last_mk_push.elapsed() >= Duration::from_secs(1)
                    {
                        media_keys::update_playback(mc, paused_now, state.time_secs());
                        last_mk_paused = paused_now;
                        last_mk_push = Instant::now();
                    }
                }

                last_ui = Instant::now();
            }

            media_keys::poll();
            thread::sleep(Duration::from_millis(frame_ms));
        }
    }

    // Flush any queued resume-state writes before exit.
    drop(save_tx);
    let _ = saver_handle.join();

    terminal::disable_raw_mode()?;

    print!("\x1B[?25h");

    let _ = prev_frame_lines; // no longer needed: full screen clear below covers everything
    // Wipe the whole header (banner + status + viz + playlist/lyrics) and any
    // kitty graphic, leaving only the goodbye line.
    if matches!(cover::detect_protocol(), cover::GraphicsProtocol::Kitty) {
        print!("{}{}", cover::kitty_clear_escape(), cover::viz_image_clear_escape());
    }
    // Home + erase-down, not ED2 — see the resize repaint above (ConPTY turns
    // 2J into a scrollback push on Windows Terminal).
    print!("\x1B[H\x1B[J");
    println!("✓ Done");
    io::stdout().flush().ok();

    // Release exclusive mode
    if let Some(id) = hog_device_id {
        audio::release_exclusive_mode(id);
    }

    // Exit immediately — implicit drops of cpal::Stream (ALSA backend) and
    // souvlaki::MediaControls (D-Bus) can block indefinitely on Linux, hanging
    // the process after the user presses Q.
    std::process::exit(0);
}

#[cfg(test)]
mod main_tests {
    use super::*;

    /// A stdout whose every write fails, like the read end of a pipe that the
    /// other process already closed (`keet --help | head`).
    struct DeadPipe;

    impl Write for DeadPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
    }

    #[test]
    fn restore_cursor_survives_a_dead_stdout() {
        // The panic hook calls this. `print!` panics when the write fails, so
        // on a closed pipe the original panic triggered a SECOND panic here —
        // and a panic while panicking aborts the process instead of exiting.
        // This must return normally no matter what stdout does.
        restore_cursor(&mut DeadPipe);
    }

    #[test]
    fn restore_cursor_emits_the_show_cursor_sequence() {
        let mut buf: Vec<u8> = Vec::new();
        restore_cursor(&mut buf);
        assert_eq!(buf, b"\x1B[?25h");
    }

    #[test]
    fn closed_pipe_panics_are_not_logged_as_crashes() {
        // Now that the hook survives a dead stdout it actually reaches the
        // logging step, so ordinary piping must not accumulate crash.log noise.
        assert!(!should_log_crash(
            "panicked at 'failed printing to stdout: Broken pipe (os error 32)'"
        ));
        assert!(!should_log_crash("failed printing to stdout: The pipe has been ended. (os error 109)"));
        // Real crashes still get logged.
        assert!(should_log_crash("panicked at 'index out of bounds: len is 3'"));
        assert!(should_log_crash("called `Option::unwrap()` on a `None` value"));
    }
}
