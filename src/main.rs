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

use state::{PlayerState, UiState, VizMode, RING_BUFFER_SIZE, VIZ_BUFFER_SIZE};
use viz::{StatsMonitor, VizAnalyser};
use audio::{build_stream, set_output_sample_rate, probe_sample_rate, fix_bluetooth_sample_rate};
use decode::decode_track;
use playlist::{build_playlist, shuffle_list, read_metadata};
use ui::{print_status, poll_input, format_time};
use resume::{ResumeState, save_state, load_state};

fn build_resume_state(
    ui: &state::UiState,
    playlist: &[std::path::PathBuf],
    player_state: &state::PlayerState,
    shuffle: bool,
    repeat: bool,
    eq_presets: &[eq::EqPreset],
    fx_presets: &[effects::EffectsPreset],
) -> ResumeState {
    ResumeState {
        source_paths: ui.source_paths.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
        track_path: playlist.get(ui.current)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        position_secs: player_state.time_secs(),
        shuffle,
        repeat,
        volume: player_state.volume.load(std::sync::atomic::Ordering::Relaxed),
        eq_preset: eq_presets[player_state.eq_index()].name.clone(),
        effects_preset: fx_presets[player_state.effects_index()].name.clone(),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Ensure terminal is in normal mode (cleanup from previous crashed runs)
    let _ = terminal::disable_raw_mode();
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

    let flags = ["--shuffle", "-s", "--repeat", "-r", "--quality", "-q", "--eq", "-e", "--fx", "--crossfade", "-x"];
    let (source_paths, shuffle, repeat) = if args.len() < 2 {
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
                (paths, rs.shuffle, rs.repeat)
            }
            None => {
                eprintln!("Usage: {} <file-or-folder>... [--shuffle] [--repeat] [--quality] [--eq <name>] [--fx <name>] [--crossfade <secs>]", args[0]);
                eprintln!("Controls: Space=Pause ↑↓=Tracks ←→=Seek V=Viz E=EQ X=FX L=List R=Rescan +/-=Vol Q=Quit");
                std::process::exit(1);
            }
        }
    } else {
        let s = args.iter().any(|a| a == "--shuffle" || a == "-s");
        let r = args.iter().any(|a| a == "--repeat" || a == "-r");
        // Collect positional args (not flags, not values after flag options)
        let mut positional = Vec::new();
        let value_flags = ["--eq", "-e", "--fx", "--crossfade", "-x"];
        let mut skip_next = false;
        for arg in &args[1..] {
            if skip_next { skip_next = false; continue; }
            if value_flags.contains(&arg.as_str()) { skip_next = true; continue; }
            if flags.contains(&arg.as_str()) { continue; }
            positional.push(PathBuf::from(arg));
        }
        if positional.is_empty() {
            eprintln!("No input files or folders specified");
            std::process::exit(1);
        }
        (positional, s, r)
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
    }

    let eq_presets = Arc::new(eq_presets);
    let fx_presets = Arc::new(fx_presets);

    let inner_w = 57;
    let title = "Keet";
    let pad_left = (inner_w - title.len()) / 2;
    let pad_right = inner_w - title.len() - pad_left;
    let eq_name = &eq_presets[state.eq_index()].name;
    let fx_name = &fx_presets[state.effects_index()].name;
    let eq_info = if eq_name != "Flat" { format!(" | EQ: {}", eq_name) } else { String::new() };
    let fx_info = if fx_name != "None" { format!(" | FX: {}", fx_name) } else { String::new() };
    let xfade_info = if crossfade_secs > 0 { format!(" | xfade: {}s", crossfade_secs) } else { String::new() };
    let info = format!("{} tracks{}{}{}{}{}{}",
        playlist.len(),
        if shuffle { " | shuffled" } else { "" },
        if repeat { " | repeat" } else { "" },
        if hq_resampler { " | HQ resampler" } else { "" },
        eq_info, fx_info, xfade_info);
    let info_display_len = info.len();
    let info_pad = inner_w.saturating_sub(info_display_len + 2);
    println!("╔{}╗", "═".repeat(inner_w));
    println!("║{}{}{}║", " ".repeat(pad_left), title, " ".repeat(pad_right));
    println!("╠{}╣", "═".repeat(inner_w));
    println!("║  {}{}║", info, " ".repeat(info_pad));
    println!("╚{}╝", "═".repeat(inner_w));

    // Audio setup
    let host = cpal::default_host();
    let mut current_output_rate = {
        let device = host.default_output_device().ok_or("No output device")?;
        let device_name = device.description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "Unknown device".to_string());
        println!("\nDevice: {}", device_name);

        // Fix stale sample rate on Bluetooth devices (CoreAudio can get stuck at wrong rate)
        let bt_rate = fix_bluetooth_sample_rate();
        if let Some(rate) = bt_rate {
            println!("Bluetooth device detected, using native {}Hz", rate);
        }

        let default_config = device.default_output_config()?;
        let rate = bt_rate.unwrap_or_else(|| default_config.sample_rate());
        let default_channels = default_config.channels();
        println!("Initial output: {}Hz (device default: {}ch)", rate, default_channels);
        rate
    };

    // Stats monitor
    let mut stats = StatsMonitor::new();

    // OS media transport controls (media keys, AirPods, Bluetooth headphones)
    let mut media_controls = media_keys::setup(Arc::clone(&state));

    println!("\n[Space] Pause  [↑↓] Track  [←→] Seek  [V/B] Viz  [E] EQ  [X] FX  [F] Fader  [L] List  [+/-] Vol  [Q] Quit\n");

    terminal::enable_raw_mode()?;

    // Hide cursor to prevent flickering
    print!("\x1B[?25l");
    io::stdout().flush().ok();

    let mut ui = UiState::new(source_paths);

    // Set starting track for resume
    if let Some(ref rs) = resume_state_loaded {
        if let Some(idx) = playlist.iter().position(|p| p.to_string_lossy() == rs.track_path.as_str()) {
            ui.current = idx;
        }
    }

    let mut prev_viz_lines: usize = usize::MAX;
    let mut crossfade_tail: Option<Vec<f32>> = None;

    'playlist: loop {
        if state.should_quit() { break; }

        if ui.current >= playlist.len() {
            if repeat {
                // Rescan sources to pick up new files and drop deleted ones
                let has_dir = ui.source_paths.iter().any(|p| p.is_dir());
                if has_dir {
                    let mut combined = Vec::new();
                    for src in &ui.source_paths {
                        if let Ok(tracks) = build_playlist(src, false) {
                            combined.extend(tracks);
                        }
                    }
                    if !combined.is_empty() {
                        let mut seen = std::collections::HashSet::new();
                        combined.retain(|p| {
                            let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                            seen.insert(key)
                        });
                        if shuffle { shuffle_list(&mut combined); }
                        playlist = combined;
                        state.total_tracks.store(playlist.len(), Ordering::Relaxed);
                    }
                } else if shuffle {
                    shuffle_list(&mut playlist);
                }
                ui.current = 0;
            } else {
                break;
            }
        }

        // Reset for new track
        state.current_track.store(ui.current, Ordering::Relaxed);
        state.producer_done.store(false, Ordering::Relaxed);
        state.track_info_ready.store(false, Ordering::Relaxed);
        state.skip_next.store(false, Ordering::Relaxed);
        state.skip_prev.store(false, Ordering::Relaxed);
        state.buffer_level.store(0, Ordering::Relaxed);
        if let Ok(mut err) = state.decode_error.lock() { *err = None; }

        let track_path = &playlist[ui.current];
        let filename = read_metadata(track_path)
            .unwrap_or_else(|| track_path.file_name().unwrap_or_default().to_string_lossy().into_owned());
        let track_ext = track_path.extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        // Re-acquire default device each track (handles device changes like AirPods disconnect)
        let device = match host.default_output_device() {
            Some(d) => d,
            None => {
                // No output device available — wait and retry
                thread::sleep(Duration::from_secs(1));
                ui.current += 1;
                continue 'playlist;
            }
        };

        // Probe source sample rate and try to switch output to match
        let source_rate = probe_sample_rate(track_path).unwrap_or(44100);
        let output_rate = set_output_sample_rate(source_rate, current_output_rate, &device);

        // Always verify the actual device rate from cpal's config.
        let actual_device_rate = match device.default_output_config() {
            Ok(config) => config.sample_rate(),
            Err(_) => output_rate,
        };

        #[cfg(debug_assertions)]
        eprintln!("DEBUG: source_rate={}, output_rate={}, actual_device_rate={}",
                  source_rate, output_rate, actual_device_rate);

        // If rate changed, update our tracking
        if actual_device_rate != current_output_rate {
            current_output_rate = actual_device_rate;
        }

        // Store output rate for time calculations
        state.output_rate.store(actual_device_rate as u64, Ordering::Relaxed);

        // Try the desired rate first; if the device rejects it, fall back
        let mut stream_rate = actual_device_rate;

        // Determine channel count for current device
        let channels = {
            let default_ch = device.default_output_config()
                .map(|c| c.channels())
                .unwrap_or(2);
            if let Ok(configs) = device.supported_output_configs() {
                let has_stereo = configs.into_iter().any(|c| {
                    c.channels() == 2
                        && c.min_sample_rate() <= stream_rate
                        && stream_rate <= c.max_sample_rate()
                });
                if has_stereo { 2 } else { default_ch }
            } else {
                default_ch
            }
        };

        // Test if the device accepts this rate by checking supported configs
        let rate_supported = device.supported_output_configs()
            .map(|configs| {
                configs.into_iter().any(|c| {
                    c.channels() == channels
                        && c.min_sample_rate() <= stream_rate
                        && stream_rate <= c.max_sample_rate()
                })
            })
            .unwrap_or(false);

        if !rate_supported {
            let fallback = device.default_output_config()
                .map(|c| c.sample_rate())
                .unwrap_or(48000);
            stream_rate = fallback;
            current_output_rate = fallback;
            state.output_rate.store(fallback as u64, Ordering::Relaxed);
        }

        // Larger buffer on Windows and WSL to reduce crackling.
        // WSL detection: /proc/version contains "microsoft" or "WSL".
        let is_wsl = cfg!(target_os = "linux") && std::fs::read_to_string("/proc/version")
            .map(|v| v.contains("microsoft") || v.contains("WSL"))
            .unwrap_or(false);
        let buffer_size = if cfg!(target_os = "windows") || is_wsl {
            cpal::BufferSize::Fixed(2048)
        } else {
            cpal::BufferSize::Default
        };

        let stream_config = StreamConfig {
            channels,
            sample_rate: stream_rate,
            buffer_size,
        };

        // Create ring buffers: main audio + viz tap
        let (prod, cons) = RingBuffer::<f32>::new(RING_BUFFER_SIZE);
        let (viz_prod, mut viz_cons) = RingBuffer::<f32>::new(VIZ_BUFFER_SIZE);

        // Start audio stream (retry with fresh device if it fails)
        let stream = match build_stream(&device, &stream_config, cons, viz_prod, Arc::clone(&state)) {
            Ok(s) => s,
            Err(_) => {
                thread::sleep(Duration::from_millis(500));
                ui.current += 1;
                continue 'playlist;
            }
        };
        if stream.play().is_err() {
            ui.current += 1;
            continue 'playlist;
        }

        // Load current EQ and effects presets for this track
        let mut eq_chain = eq::EqChain::new();
        eq_chain.load_preset(&eq_presets[state.eq_index()], stream_rate as f32);
        let mut fx_chain = effects::EffectsChain::new(stream_rate as f32);
        fx_chain.load_preset(&fx_presets[state.effects_index()], stream_rate as f32);

        // Crossfade setup
        let xfade_in = crossfade_tail.take();
        let xfade_samples = crossfade_secs as usize * stream_rate as usize * 2; // stereo samples

        // Start producer thread
        let path_clone = track_path.clone();
        let state_clone = Arc::clone(&state);
        let eq_presets_clone = Arc::clone(&eq_presets);
        let fx_presets_clone = Arc::clone(&fx_presets);
        let mut prod = prod;
        let producer_handle = thread::spawn(move || -> Option<Vec<f32>> {
            match decode_track(
                &path_clone, &mut prod, &state_clone, stream_rate, hq_resampler,
                &mut eq_chain, &eq_presets_clone,
                &mut fx_chain, &fx_presets_clone,
                xfade_in.as_deref(), xfade_samples,
            ) {
                Ok(tail) => {
                    state_clone.producer_done.store(true, Ordering::Relaxed);
                    tail
                }
                Err(e) => {
                    if let Ok(mut err) = state_clone.decode_error.lock() {
                        *err = Some(e);
                    }
                    state_clone.producer_done.store(true, Ordering::Relaxed);
                    None
                }
            }
        });

        // Wait for track info and initial buffer fill (also process input for fast skipping)
        while (!state.track_info_ready.load(Ordering::Relaxed)
               || state.buffer_level.load(Ordering::Relaxed) < RING_BUFFER_SIZE / 4)
              && !state.producer_done.load(Ordering::Relaxed)
              && !state.should_quit()
              && !state.is_skip_requested()
        {
            poll_input(&state, &mut ui, &mut playlist);
            thread::sleep(Duration::from_millis(20));
        }

        // If skip requested during wait, advance without printing
        if state.is_skip_requested() {
            producer_handle.join().ok();
            if let Some(target) = state.take_jump() {
                ui.current = target;
            } else if state.take_skip_next() {
                ui.current += 1;
            } else if state.take_skip_prev() {
                ui.current = ui.current.saturating_sub(1);
            }
            continue 'playlist;
        }

        // If decode failed before track info was set, skip this track
        if state.producer_done.load(Ordering::Relaxed)
           && !state.track_info_ready.load(Ordering::Relaxed)
        {
            producer_handle.join().ok();
            ui.current += 1;
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
        let track_info = format!("{} • {}bit {} • {}", format_time(state.total_secs()), bits, ch_str, rate_str);

        // Update OS media transport with track metadata
        if let Some(ref mut mc) = media_controls {
            media_keys::update_metadata(mc, &filename, state.total_secs());
            media_keys::update_playback(mc, state.is_paused(), 0.0);
        }

        // Visualization analyzer (runs on main thread, fed by audio callback)
        let mut viz_analyser = VizAnalyser::new(stream_rate);
        let mut viz_scratch = Vec::with_capacity(VIZ_BUFFER_SIZE);

        // Playback loop
        let mut last_ui = Instant::now();

        loop {
            // Input (non-blocking)
            if poll_input(&state, &mut ui, &mut playlist) {
                print!("\x1B[?25h");
                if prev_viz_lines != usize::MAX {
                    let up = 2 + prev_viz_lines;
                    print!("\x1B[{}F", up);
                }
                print!("\x1B[J"); // Clear from cursor to end of screen
                io::stdout().flush().ok();
                save_state(&build_resume_state(&ui, &playlist, &state, shuffle, repeat, &eq_presets, &fx_presets));
                producer_handle.join().ok();
                break 'playlist;
            }

            // Skip handling
            if state.is_skip_requested() {
                break;
            }

            // UI update: 20fps when visualizing, 4fps when idle
            let ui_interval = if state.viz_mode() == VizMode::None { 250 } else { 50 };
            if last_ui.elapsed() >= Duration::from_millis(ui_interval) {
                // Process viz samples from audio callback (synced to playback)
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
                    // Drain viz buffer when not visualizing
                    let viz_available = viz_cons.slots();
                    if viz_available > 0 {
                        if let Ok(chunk) = viz_cons.read_chunk(viz_available) {
                            chunk.commit_all();
                        }
                    }
                }

                stats.update();
                let current_eq = &eq_presets[state.eq_index()];
                let current_fx = &fx_presets[state.effects_index()].name;
                prev_viz_lines = print_status(&state, &mut ui, &filename, &track_info, &track_ext, current_eq, current_fx, &mut stats, prev_viz_lines, &playlist);

                // Update OS media transport with playback position
                if let Some(ref mut mc) = media_controls {
                    media_keys::update_playback(mc, state.is_paused(), state.time_secs());
                }

                last_ui = Instant::now();
            }

            // Track finished?
            if state.producer_done.load(Ordering::Relaxed)
               && state.buffer_level.load(Ordering::Relaxed) == 0
            {
                thread::sleep(Duration::from_millis(200));
                break;
            }

            // Pump OS event loop for media key dispatch
            media_keys::poll();

            // Sleep - main thread does very little
            thread::sleep(Duration::from_millis(50));
        }

        if let Ok(tail) = producer_handle.join() {
            crossfade_tail = tail;
        }

        // Save resume state on track change
        save_state(&build_resume_state(&ui, &playlist, &state, shuffle, repeat, &eq_presets, &fx_presets));

        // Handle track transition
        if let Some(target) = state.take_jump() {
            ui.current = target;
        } else if state.take_skip_next() {
            ui.current += 1;
        } else if state.take_skip_prev() {
            ui.current = ui.current.saturating_sub(1);
        } else {
            ui.current += 1;
        }
    }

    terminal::disable_raw_mode()?;

    print!("\x1B[?25h");

    if prev_viz_lines != usize::MAX {
        let up = 2 + prev_viz_lines;
        print!("\x1B[{}F", up);
    }
    print!("\x1B[J"); // Clear from cursor to end of screen
    println!("✓ Done");

    Ok(())
}
