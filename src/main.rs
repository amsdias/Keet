// Keet - Low-CPU audio player with producer/consumer architecture
// - Lock-free ring buffer (no mutex in audio callback)
// - SincFixedIn resampler (high quality)
// - Batched atomic updates with Relaxed ordering
// - Separate decode thread
//
// Usage: cargo run --release -- <file-or-folder> [--shuffle] [--repeat] [--quality]
// Controls: Space=Pause, ↑↓=Tracks, ←→=Seek ±10s, V=Viz, +/-=Vol, Q=Quit

mod state;
mod viz;
mod audio;
mod decode;
mod playlist;
mod ui;
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

use state::{PlayerState, UiState, RgMode, VizMode, RING_BUFFER_SIZE, VIZ_BUFFER_SIZE};
use viz::{StatsMonitor, VizAnalyser};
use audio::{build_stream, set_output_sample_rate, probe_sample_rate, fix_bluetooth_sample_rate};
use decode::decode_playlist;
use playlist::{build_playlist, shuffle_list};
use ui::{print_status, poll_input, format_time};
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
            (Some(a), Some(al)) => cover::resolve_remote(&path, &a, &al),
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
    }
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
    // Full terminal reset in case previous run crashed mid-draw
    // \x1Bc = RIS (Reset to Initial State) - clears screen, resets charset, tab stops, modes
    print!("\x1Bc");
    io::stdout().flush().ok();

    // Restore terminal on panic so it doesn't stay in raw mode
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        print!("\x1B[?25h"); // Show cursor
        let _ = io::stdout().flush();

        // Write crash log to ~/.config/keet/crash.log
        if let Some(config_dir) = playlist::keet_config_dir() {
            let _ = std::fs::create_dir_all(&config_dir);
            let log_path = config_dir.join("crash.log");
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let entry = format!("[{}] {}\n", timestamp, info);
            // Append to log file
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
                let _ = f.write_all(entry.as_bytes());
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
        println!("  V            Cycle visualization (off → VU → spectrum H → spectrum V)");
        println!("  B            Toggle viz style (dots / bars)");
        println!("  F            Toggle pre/post-fader metering");
        println!("  E            Cycle EQ presets");
        println!("  X            Cycle effects presets");
        println!("  C            Cycle crossfeed (Off → Light → Medium → Strong)");
        println!("  [ / ]        Balance left / right (5% steps)");
        println!("  L            Toggle playlist view");
        println!("  Y            Toggle lyrics view (synced LRC auto-scrolls)");
        println!("  S            Save playlist as M3U");
        println!("  R            Rescan folders for new files");
        println!("  Z            Toggle shuffle");
        println!("  Shift+R      Toggle repeat");
        println!("  O            Open a new source (type a path)");
        println!("  P            Pick a new source (native folder dialog)");
        println!("  Q / Esc      Quit");
        println!();
        println!("\x1B[1mPLAYLIST VIEW\x1B[0m  (press L)");
        println!("  Up / Down    Scroll track list");
        println!("  Enter        Jump to selected track");
        println!("  /            Search / filter by filename");
        println!("  D            Remove selected track");
        println!("  Esc / L      Close playlist view");
        println!();
        println!("\x1B[1mCUSTOM PRESETS\x1B[0m");
        println!("  EQ:      ~/.config/keet/eq/*.json");
        println!("  Effects: ~/.config/keet/effects/*.json");
        return Ok(());
    }

    // Handle --list-devices (print and exit)
    if args.iter().any(|a| a == "--list-devices") {
        let host = cpal::default_host();
        audio::list_output_devices(&host);
        return Ok(());
    }

    let flags = ["--shuffle", "-s", "--repeat", "-r", "--quality", "-q", "--eq", "-e", "--fx", "--crossfade", "-x", "--rg-mode", "--list-devices", "--device", "--exclusive", "--no-cover", "--help", "-h"];
    let (source_paths, shuffle, repeat_mode) = if args.len() < 2 {
        // Try resume from saved state
        match load_state() {
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
        let value_flags = ["--eq", "-e", "--fx", "--crossfade", "-x", "--rg-mode", "--device"];
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

    // Load crossfeed presets (built-in only)
    let cf_presets = crossfeed::builtin_presets();
    state.crossfeed_preset_count.store(cf_presets.len(), Ordering::Relaxed);
    let cf_presets = Arc::new(cf_presets);

    // Restore resume state if resuming
    let resume_state_loaded = if args.len() < 2 { load_state() } else { None };
    let mut resume_position: i64 = 0;

    if let Some(ref rs) = resume_state_loaded {
        state.volume.store(rs.volume, Ordering::Relaxed);
        resume_position = rs.position_secs.round() as i64;

        // Restore EQ preset by name
        if let Some(idx) = eq_presets.iter().position(|p| p.name == rs.eq_preset) {
            state.eq_preset_index.store(idx, Ordering::Relaxed);
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
        let pad_left = (inner_w - title.len()) / 2;
        let pad_right = inner_w - title.len() - pad_left;
        let eq_name = &eq_presets[state.eq_index()].name;
        let fx_name = &fx_presets[state.effects_index()].name;
        let eq_info = if eq_name != "Flat" { format!(" | EQ: {}", eq_name) } else { String::new() };
        let fx_info = if fx_name != "None" { format!(" | FX: {}", fx_name) } else { String::new() };
        let xfade_info = if crossfade_secs > 0 { format!(" | xfade: {}s", crossfade_secs) } else { String::new() };
        let cf_name = &cf_presets[state.crossfeed_index()].name;
        let cf_info = if cf_name != "Off" { format!(" | crossfeed: {}", cf_name) } else { String::new() };
        let bal_val = state.balance_value();
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
    };

    let banner_box = build_banner_box(shuffle, repeat_mode, &state);
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
        writeln!(banner_tail, "\nDevice: {}", device_name).ok();

        // Fix stale sample rate on Bluetooth devices (CoreAudio can get stuck at wrong rate)
        let bt_rate = fix_bluetooth_sample_rate();
        if let Some(rate) = bt_rate {
            writeln!(banner_tail, "Bluetooth device detected, using native {}Hz", rate).ok();
        }

        let default_config = device.default_output_config()?;
        let rate = bt_rate.unwrap_or_else(|| default_config.sample_rate());
        let default_channels = default_config.channels();
        writeln!(banner_tail, "Initial output: {}Hz (device default: {}ch)", rate, default_channels).ok();
        rate
    };

    // Stats monitor
    let mut stats = StatsMonitor::new();

    // OS media transport controls (media keys, AirPods, Bluetooth headphones)
    let mut media_controls = media_keys::setup(Arc::clone(&state));

    writeln!(banner_tail, "\n{0}{{Space}}{1} Pause  {0}{{↑/↓}}{1} Track  {0}{{←/→}}{1} Seek  {0}{{+/-}}{1} Vol  {0}{{[/]}}{1} Bal  {0}{{Q}}{1} Quit",
        "\x1B[2m", "\x1B[0m").ok();
    writeln!(banner_tail, "{0}{{E}}{1} EQ  {0}{{X}}{1} FX  {0}{{C}}{1} Crossfeed  {0}{{F}}{1} Fader  {0}{{V/B}}{1} Viz  {0}{{I}}{1} Info  {0}{{Y}}{1} Lyrics",
        "\x1B[2m", "\x1B[0m").ok();
    writeln!(banner_tail, "{0}{{L}}{1} List  {0}{{R}}{1} Rescan  {0}{{Shift+R}}{1} Repeat  {0}{{Z}}{1} Shuffle  {0}{{O}}{1} Open  {0}{{P}}{1} Pick\n",
        "\x1B[2m", "\x1B[0m").ok();

    // Print banner and count its lines
    let banner = format!("{}{}", banner_box, banner_tail);
    print!("{}", banner);
    let banner_lines = banner.lines().count();

    terminal::enable_raw_mode()?;

    // Hide cursor to prevent flickering
    print!("\x1B[?25l");
    io::stdout().flush().ok();

    let metadata_cache = metadata::MetadataCache::new(playlist.len());
    let mut ui = UiState::new(source_paths, std::sync::Arc::clone(&metadata_cache));
    ui.shuffle = shuffle;
    ui.repeat_mode = repeat_mode;
    state.repeat_mode.store(repeat_mode as u8, Ordering::Relaxed);
    ui.banner_lines = banner_lines;
    ui.banner_text = banner;
    ui.cover_enabled = cover_enabled;
    ui.banner_tail = banner_tail;
    ui.scan_handle = Some(metadata::spawn_metadata_scan(
        playlist.clone(),
        std::sync::Arc::clone(&metadata_cache),
    ));

    // Set starting track for resume
    if let Some(ref rs) = resume_state_loaded {
        if let Some(idx) = playlist.iter().position(|p| p.to_string_lossy() == rs.track_path.as_str()) {
            ui.current = idx;
        }
    }

    let mut prev_viz_lines: usize = usize::MAX;

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
    let mut stream_rate = {
        let channels = 2u16;
        let rate_supported = device.supported_output_configs()
            .map(|configs| {
                configs.into_iter().any(|c| {
                    c.channels() == channels
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
    let stream_config = StreamConfig {
        channels: 2,
        sample_rate: stream_rate,
        buffer_size,
    };

    let (mut prod, cons) = RingBuffer::<f32>::new(RING_BUFFER_SIZE);
    let (viz_prod, mut viz_cons) = RingBuffer::<f32>::new(VIZ_BUFFER_SIZE);

    let mut stream = build_stream(&device, &stream_config, cons, viz_prod, Arc::clone(&state))?;
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

                // Reindex metadata cache
                ui.metadata_cache.cancel.store(true, Ordering::Relaxed);
                if let Some(h) = ui.scan_handle.take() {
                    h.join().ok();
                }
                ui.metadata_cache.reindex(&playlist, &old_playlist);
                ui.metadata_cache.cancel.store(false, Ordering::Relaxed);
                ui.scan_handle = Some(metadata::spawn_metadata_scan(
                    playlist.clone(),
                    std::sync::Arc::clone(&ui.metadata_cache),
                ));

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
            eq_chain.load_preset(&eq_presets_clone[state_clone.eq_index()], sr as f32);
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
            prev_viz_lines = usize::MAX;
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
            // Wait for ~1 second of audio in the buffer before starting the callback.
            // Using stream_rate (rather than a fraction of the raw ring size) keeps
            // the startup latency consistent across output rates.
            let startup_threshold = stream_rate as usize * 2;
            while state.buffer_level.load(Ordering::Relaxed) < startup_threshold
                  && !state.producer_done.load(Ordering::Relaxed)
                  && !state.should_quit()
            {
                poll_input(&state, &mut ui, &mut playlist);
                prev_viz_lines = print_status(&state, &mut ui, &filename, &track_info, &track_ext, current_eq, current_fx, current_cf, &mut stats, prev_viz_lines, &playlist, &viz_analyser);
                thread::sleep(Duration::from_millis(20));
            }
        }

        // Update OS media transport
        if let Some(ref mut mc) = media_controls {
            media_keys::update_metadata(mc, &filename, state.total_secs());
            media_keys::update_playback(mc, state.is_paused(), 0.0);
        }

        // Playback loop (stays here across natural track transitions)
        let mut last_ui = Instant::now();

        loop {
            // Input
            if poll_input(&state, &mut ui, &mut playlist) {
                print!("\x1B[?25h");
                if prev_viz_lines != usize::MAX {
                    let up = 2 + prev_viz_lines;
                    print!("\x1B[{}F", up);
                }
                print!("\x1B[J");
                io::stdout().flush().ok();
                save_state(&build_resume_state(&ui, &playlist, &state, &eq_presets, &fx_presets, &cf_presets, &device_arg));
                if let Some(id) = hog_device_id {
                    audio::release_exclusive_mode(id);
                }
                // Producer will exit when state.should_quit() is true
                let _ = producer_handle.join();
                break 'playlist;
            }

            // Check for track transitions from the producer
            let current_count = state.track_transition_count.load(Ordering::Acquire);
            if current_count != last_transition_count {
                let new_index = state.producer_track_index.load(Ordering::Relaxed);
                last_transition_count = current_count;

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
                        media_keys::update_metadata(mc, &filename, state.total_secs());
                        media_keys::update_playback(mc, state.is_paused(), 0.0);
                    }

                    save_state(&build_resume_state(&ui, &playlist, &state, &eq_presets, &fx_presets, &cf_presets, &device_arg));
                }
            }

            // Skip-prev or jump: join producer, respawn
            if state.skip_prev.load(Ordering::Relaxed) || state.jump_to_track.load(Ordering::Relaxed) >= 0 {
                match producer_handle.join() {
                    Ok(p) => prod = p,
                    Err(_) => break 'playlist,
                }
                // Flush ring buffer
                if RING_BUFFER_SIZE - prod.slots() > 0 {
                    state.reset_consumer_counter.store(true, Ordering::Relaxed);
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
                // Wait for buffer to drain so current track finishes
                while state.buffer_level.load(Ordering::Relaxed) > 0 && !state.should_quit() && !state.is_paused() {
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

                // Drop old stream before creating new ring buffer
                drop(stream);

                // Rebuild ring buffer and stream
                let (new_prod, new_cons) = RingBuffer::<f32>::new(RING_BUFFER_SIZE);
                let (new_viz_prod, new_viz_cons) = RingBuffer::<f32>::new(VIZ_BUFFER_SIZE);
                prod = new_prod;
                viz_cons = new_viz_cons;

                let new_config = StreamConfig {
                    channels: 2,
                    sample_rate: stream_rate,
                    buffer_size: saved_buffer_size,
                };
                stream = build_stream(&device, &new_config, new_cons, new_viz_prod, Arc::clone(&state))?;
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

                    let (new_prod, new_cons) = RingBuffer::<f32>::new(RING_BUFFER_SIZE);
                    let (new_viz_prod, new_viz_cons) = RingBuffer::<f32>::new(VIZ_BUFFER_SIZE);
                    prod = new_prod;
                    viz_cons = new_viz_cons;

                    let new_config = StreamConfig {
                        channels: 2,
                        sample_rate: stream_rate,
                        buffer_size: saved_buffer_size,
                    };
                    match build_stream(&device, &new_config, new_cons, new_viz_prod, Arc::clone(&state)) {
                        Ok(s) => {
                            stream = s;
                            if stream.play().is_err() {
                                break 'playlist;
                            }
                        }
                        Err(_) => break 'playlist,
                    }
                    // Resume from current track
                    continue 'playlist;
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

                save_state(&build_resume_state(&ui, &playlist, &state, &eq_presets, &fx_presets, &cf_presets, &device_arg));
                ui.current = playlist.len(); // Will trigger repeat-cycle or exit
                continue 'playlist;
            }

            // UI update
            let ui_interval: u64 = 50;
            if last_ui.elapsed() >= Duration::from_millis(ui_interval) {
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
                        ui.banner_dirty = true; // redraw to show new cover or clear stale one
                    }
                }

                if ui.banner_dirty {
                    ui.banner_dirty = false;
                    let new_box = build_banner_box(ui.shuffle, ui.repeat_mode, &state);
                    ui.banner_text = format!("{}{}", new_box, ui.banner_tail);
                    ui.terminal_resized = true;
                }

                if ui.terminal_resized {
                    ui.terminal_resized = false;
                    // Clear entire screen and reprint banner (old lines may
                    // have wrapped at the previous terminal width).
                    // In raw mode \n doesn't imply \r, so use \r\n.
                    let term_w = terminal::size().map(|(w, _)| w as usize).unwrap_or(120);
                    let (composed, lines) = compose_banner(&ui.banner_text, ui.cover.as_ref(), term_w);
                    ui.banner_lines = lines;
                    // Remove any previously-placed kitty graphic before redrawing.
                    // No-op on terminals that don't speak the protocol.
                    let kitty_clear = if matches!(cover::detect_protocol(), cover::GraphicsProtocol::Kitty) {
                        cover::kitty_clear_escape()
                    } else {
                        String::new()
                    };
                    print!("{}\x1B[0m\x1B[2J\x1B[H{}", kitty_clear, composed.replace('\n', "\r\n"));
                    prev_viz_lines = usize::MAX;
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
                prev_viz_lines = print_status(&state, &mut ui, &filename, &track_info, &track_ext, current_eq, current_fx, current_cf, &mut stats, prev_viz_lines, &playlist, &viz_analyser);

                if let Some(ref mut mc) = media_controls {
                    media_keys::update_playback(mc, state.is_paused(), state.time_secs());
                }

                last_ui = Instant::now();
            }

            media_keys::poll();
            thread::sleep(Duration::from_millis(50));
        }
    }

    terminal::disable_raw_mode()?;

    print!("\x1B[?25h");

    let _ = prev_viz_lines; // no longer needed: full screen clear below covers everything
    // Wipe the whole header (banner + status + viz + playlist/lyrics) and any
    // kitty graphic, leaving only the goodbye line.
    if matches!(cover::detect_protocol(), cover::GraphicsProtocol::Kitty) {
        print!("{}", cover::kitty_clear_escape());
    }
    print!("\x1B[H\x1B[2J");
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
