// Keet - Low-CPU audio player with producer/consumer architecture
// - Lock-free ring buffer (no mutex in audio callback)
// - SincFixedIn resampler (high quality)
// - Batched atomic updates with Relaxed ordering
// - Separate decode thread
//
// Usage: cargo run --release -- <file-or-folder> [--shuffle] [--repeat] [--quality]
// Controls: Space=Pause, ↑↓=Tracks, ←→=Seek ±10s, V=Viz, +/-=Vol, Q=Quit

mod ansi;
mod cli;
mod state;
mod theme;
mod player;
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
mod ui_classic;
mod eq;
mod effects;
mod media_keys;
mod resume;
mod crossfeed;
mod metadata;
mod lyrics;
mod cover;
mod gapless;
mod fade;
mod signal;
mod term;
#[cfg(target_os = "windows")]
mod wasapi_out;
mod wasapi_logic;

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::StreamConfig;
use crossterm::terminal;
use rtrb::RingBuffer;

use state::{PlayerState, UiState, RgMode, ring_capacity_for, VIZ_BUFFER_SIZE};
use audio::{build_stream, set_output_sample_rate, probe_sample_rate, fix_bluetooth_sample_rate};
use playlist::{build_playlist, shuffle_list};
use ui::arm_auto_sort;
use resume::{ResumeState, save_state, load_state};

/// Exclusive mode on a device whose driver offers a single rate (Focusrite and
/// NVIDIA HDMI under WASAPI lock their clock to the control panel's setting):
/// say so, or every other rate is resampled with no hint why.
fn locked_rate_note(state: &PlayerState, device: &cpal::Device) -> Option<String> {
    let rate = state.exclusive_caps.lock().ok()?.as_ref()?.locked_rate()?;
    let name = device
        .description()
        .map(|d| d.name().trim().to_string())
        .unwrap_or_else(|_| "the device".into());
    let khz = format!("{:.1}", rate as f64 / 1000.0);
    let khz = khz.trim_end_matches(".0");
    Some(format!(
        "{name}: rate locked at {khz} kHz by its driver — other rates are resampled (set it in the device's control panel)"
    ))
}

/// Exclusive mode: put the DAC at its most precise physical format for `rate`
/// (Keet otherwise set only the rate, so a DAC left at 16-bit in Audio MIDI
/// Setup truncated 24-bit files) and record the result for display. Must run
/// with no stream open, like the rate change it follows.
fn apply_exclusive_bit_depth(state: &PlayerState, device: &cpal::Device, rate: u32) {
    if state.exclusive.load(Ordering::Relaxed) {
        let bits = audio::set_max_bit_depth(device, rate).unwrap_or(0);
        state.output_bits.store(bits, Ordering::Relaxed);
    }
}

/// The output device chosen by `--device` (an exact id, then an exact name,
/// then a substring — see audio::find_device_by_name), or the default. A name
/// that matches nothing falls back to the default with a warning, returned
/// for the status line: printed to stderr it was wiped by the UI's first
/// frame before anyone could read it. It used to be looked up twice at
/// startup, warning twice.
fn select_device(
    host: &cpal::Host,
    wanted: Option<&str>,
) -> Result<(cpal::Device, Option<String>), Box<dyn std::error::Error>> {
    let mut warning = None;
    if let Some(name) = wanted {
        if let Some(d) = audio::find_device_by_name(host, name) {
            return Ok((d, None));
        }
        warning = Some(format!("device \"{name}\" not found — playing on the default output"));
    }
    Ok((host.default_output_device().ok_or("No output device")?, warning))
}

/// The output as startup opened it.
struct StartupOutput {
    device: cpal::Device,
    /// A notice to show once the UI is up (exclusive mode off, a locked rate).
    note: Option<String>,
    out_channels: u16,
    stream_rate: u32,
    buffer_size: cpal::BufferSize,
    prod: rtrb::Producer<f32>,
    viz_cons: rtrb::Consumer<f32>,
    stream: audio::Output,
}

/// Open the output for the first track: resolve the exclusive-mode device,
/// read its rates, set its rate and bit depth (exclusive mode only), open and
/// start the stream, then take hog mode. Everything changed on the device is
/// recorded in `restore` first.
#[allow(clippy::too_many_arguments)]
fn open_startup_output(
    host: &cpal::Host,
    mut device: cpal::Device,
    mut exclusive: bool,
    source_rate: u32,
    current_output_rate: u32,
    state: &Arc<PlayerState>,
    restore: &mut audio::DeviceRestore,
) -> Result<StartupOutput, Box<dyn std::error::Error>> {
    // Exclusive mode binds to ONE device: the stream, hog mode and every rate
    // switch must all be the same hardware. macOS pins the device (hogging the
    // default makes macOS move the default away); Linux resolves the card's raw
    // hw: device. If that is impossible — a sound-server route on Linux, an
    // unsupported platform — say why and continue in normal mode, with state
    // agreeing (everything downstream reads state.exclusive).
    let mut startup_note: Option<String> = None;
    if exclusive {
        match audio::prepare_exclusive_device(host, &device) {
            Ok(d) => device = d,
            Err(e) => {
                eprintln!("Note: {e}. Playing in normal mode.");
                // The stderr note is wiped when the UI takes the screen; show
                // it where it can be read (set just before playback starts).
                startup_note = Some(format!("exclusive mode off: {e}"));
                exclusive = false;
                state.exclusive.store(false, Ordering::Relaxed);
            }
        }
    }

    // Exclusive mode changes the DAC's rate and bit depth; remember how it was
    // so quitting can put it back (before the first change, below).
    if exclusive {
        restore.capture(&device);
    }
    // Only exclusive mode may change the device. Normal mode used to switch the
    // DAC's system-wide rate to the first track's here (macOS; the other
    // platforms' set_output_sample_rate never touches the device): that spared
    // one track from resampling, resampled every other app on the DAC, never
    // switched again, and was never put back. Normal mode now plays at whatever
    // rate the device is set to and resamples every track the same way.
    // Exclusive mode's rate capabilities are read first — before the startup
    // rate is chosen (it goes through the same rule as every later switch)
    // and BEFORE any stream opens: a Linux raw hw: device admits a single
    // client, and listing its rates opens it — probed while our own stream had
    // it, the query failed with EBUSY and read as "only 48 kHz", so no track
    // ever switched rate.
    if exclusive {
        if let Ok(mut caps) = state.exclusive_caps.lock() {
            *caps = audio::probe_rate_caps(&device);
        }
        if startup_note.is_none() {
            startup_note = locked_rate_note(state, &device);
        }
    }
    let persistent_output_rate = if exclusive {
        let target = state.exclusive_target_rate(source_rate, current_output_rate);
        set_output_sample_rate(target, current_output_rate, &device)
    } else {
        current_output_rate
    };
    apply_exclusive_bit_depth(state, &device, persistent_output_rate);
    // Exclusive mode opens at the rate it chose. On macOS the device's default
    // config reflects that rate (Keet set it); a Linux raw hw: device has no
    // device-wide rate, and its default config is a fixed one (48 kHz) — so
    // reading the rate back from there opened every session at 48 kHz.
    let actual_device_rate = if exclusive {
        persistent_output_rate
    } else {
        match device.default_output_config() {
            Ok(config) => config.sample_rate(),
            Err(_) => persistent_output_rate,
        }
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
    let stream_rate = {
        let rate_supported = device.supported_output_configs()
            .map(|configs| {
                configs.into_iter().any(|c| {
                    c.channels() == out_channels
                        && c.min_sample_rate() <= actual_device_rate
                        && actual_device_rate <= c.max_sample_rate()
                })
            })
            .unwrap_or(false);
        // Exclusive mode already checked the rate against the device itself.
        // cpal's list is the SHARED-mode one — on Windows just the mixer's
        // format — and would send a 44.1 kHz file back to 48 kHz here.
        if exclusive || rate_supported { actual_device_rate } else {
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

    let (prod, viz_cons, stream, built_rate) =
        rebuild_stream(&device, stream_rate, out_channels, buffer_size, state)?;
    stream.play()?;

    // Set exclusive mode if requested (macOS only: hog mode + per-track rate switching)
    if exclusive {
        match audio::set_exclusive_mode(&device) {
            Ok(Some(id)) => {
                restore.hog = Some(id);
                crate::term::out!("Exclusive mode: hog + per-track rate switching\r\n");
            }
            Ok(None) => {
                if cfg!(target_os = "windows") {
                    crate::term::out!("Exclusive mode: WASAPI exclusive + per-track rate switching\r\n");
                } else {
                    crate::term::out!("Exclusive mode: raw hardware device + per-track rate switching\r\n");
                }
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

    Ok(StartupOutput {
        device,
        note: startup_note,
        out_channels,
        stream_rate: built_rate,
        buffer_size,
        prod,
        viz_cons,
        stream,
    })
}

/// Restore the terminal on a panic (main thread only) and log it to
/// ~/.config/keet/crash.log.
fn install_panic_hook() {
    // Restore terminal on panic so it doesn't stay in raw mode
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Only a panic on the main (UI) thread ends the program, so only that
        // one restores the terminal. Any other thread's panic — the producer
        // (caught there: the bad file is skipped), a cover, lyrics or scan
        // worker — leaves the UI running, and restoring cooked mode under it
        // broke the terminal for the rest of the session. It is still logged.
        let ends_program = thread::current().name() == Some("main");
        if ends_program {
            // The frame being built when the panic hit is half a frame: drop
            // it (TerminalGuard would flush it on the way down), close a
            // synchronized update it may have opened, and start a clean line.
            term::discard();
            let _ = io::stdout().write_all(b"\x1B[?2026l\x1B[0m\r\n");
            let _ = terminal::disable_raw_mode();
            restore_cursor(&mut io::stdout());
        }

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

        // The default handler prints to stderr — over the running UI, for any
        // thread but main. Those are in the crash log.
        if ends_program {
            default_panic(info);
        }
    }));
}

/// The tracks of every source (folders, files, M3U), deduplicated by
/// canonical path, shuffled if asked. One unreadable source among several is
/// skipped with a warning; a lone one is an error.
fn load_initial_playlist(source_paths: &[PathBuf], shuffle: bool) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut combined = Vec::new();
    for src in source_paths {
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
    Ok(combined)
}

/// `--eq`/`--fx`: a preset by name (any case), or else a JSON preset file at
/// that path, which joins the list. Its index; None when neither works.
fn pick_preset<T: serde::de::DeserializeOwned>(presets: &mut Vec<T>, wanted: &str, name: fn(&mut T) -> &mut String) -> Option<usize> {
    if let Some(i) = presets.iter_mut().position(|p| name(p).eq_ignore_ascii_case(wanted)) {
        return Some(i);
    }
    let mut preset = serde_json::from_str::<T>(&std::fs::read_to_string(wanted).ok()?).ok()?;
    // Shown on screen like any preset name: cleaned the same way as the ones
    // loaded from the preset folders (an ESC in it would be executed).
    let n = name(&mut preset);
    *n = crate::ansi::sanitize_display(n);
    presets.push(preset);
    Some(presets.len() - 1)
}

/// Put a resumed session's settings back (volume, presets, a custom EQ,
/// ReplayGain mode, crossfeed, balance). Returns where to resume, in seconds.
fn apply_resume_state(
    state: &PlayerState,
    rs: &ResumeState,
    eq_presets: &[eq::EqPreset],
    fx_presets: &[effects::EffectsPreset],
    cf_presets: &[crossfeed::CrossfeedPreset],
) -> i64 {
    state.volume.store(rs.volume, Ordering::Relaxed);

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
    rs.position_secs.round() as i64
}

/// config.json defaults: viz mode, ReplayGain mode, EQ and crossfeed preset.
/// Each overrides the resumed value but yields to an explicit flag for the
/// same setting (`rg_mode_flag`, `eq_flag`).
fn apply_config(
    state: &PlayerState,
    config: &config::Config,
    rg_mode_flag: bool,
    eq_flag: bool,
    eq_presets: &[eq::EqPreset],
    cf_presets: &[crossfeed::CrossfeedPreset],
) {
    if let Some(v) = config.viz.as_deref().and_then(state::VizMode::from_str) {
        state.viz_mode.store(v as u8, Ordering::Relaxed);
    }
    if !rg_mode_flag {
        if let Some(m) = config.rg_mode.as_deref().and_then(RgMode::from_str) {
            state.rg_mode.store(m as u8, Ordering::Relaxed);
        }
    }
    if !eq_flag {
        if let Some(name) = config.eq.as_deref() {
            if let Some(idx) = eq_presets.iter().position(|p| p.name.eq_ignore_ascii_case(name)) {
                state.eq_preset_index.store(idx, Ordering::Relaxed);
            }
        }
    }
    if let Some(name) = config.crossfeed.as_deref() {
        if let Some(idx) = cf_presets.iter().position(|p| p.name.eq_ignore_ascii_case(name)) {
            state.crossfeed_preset_index.store(idx, Ordering::Relaxed);
        }
    }
}

/// Open the output at `target_rate`: in exclusive mode switch the device to it
/// and set its bit depth first, then build and start the stream. Call with NO
/// stream open — a stream alive during a rate change reports
/// StreamInvalidated. The returned rate is the one the stream really runs at
/// (a fallback can differ); spawn the producer with it. One sequence for every
/// switch: the track-start rate match, the rate-change handler and device
/// recovery each had their own copy.
fn open_output(
    device: &cpal::Device,
    current_rate: u32,
    target_rate: u32,
    channels: u16,
    buffer_size: cpal::BufferSize,
    state: &Arc<PlayerState>,
) -> Result<StreamParts, Box<dyn std::error::Error>> {
    let rate = if state.exclusive.load(Ordering::Relaxed) {
        set_output_sample_rate(target_rate, current_rate, device)
    } else {
        target_rate
    };
    apply_exclusive_bit_depth(state, device, rate);
    // Anything the old stream reported on its way out is about that stream.
    state.stream_error.store(false, Ordering::Relaxed);
    let parts = rebuild_stream(device, rate, channels, buffer_size, state)?;
    parts.2.play()?;
    state.output_rate.store(parts.3 as u64, Ordering::Relaxed);
    Ok(parts)
}

/// No output could be opened. Say so and hand over to the recovery block,
/// which retries with a backoff (input keeps working, so quit stays possible).
/// It used to quit the app — leaving the terminal in raw mode.
fn output_failed(ui: &mut state::UiState, state: &PlayerState, e: &dyn std::error::Error) {
    let wait = recovery_backoff(ui.recovery_failures);
    ui.recovery_failures = ui.recovery_failures.saturating_add(1);
    ui.set_status_for(format!("can't open the output: {e} — retrying in {} s", wait.as_secs()), Duration::from_secs(3));
    state.stream_error.store(true, Ordering::Relaxed);
    ui.recovery_retry_at = Some(Instant::now() + wait);
}

/// How long to wait before the next attempt after `failures` failed ones:
/// 1, 2, 4, then 8 s. Each attempt opens (and in exclusive mode can probe) a
/// device on the UI thread; a fixed second hammered a device that stays
/// unavailable and stalled the UI each time.
fn recovery_backoff(failures: u32) -> Duration {
    Duration::from_secs(1 << failures.min(3))
}

/// "44100Hz", "44100→48000Hz" when resampling, plus the DAC's format in
/// exclusive mode once known ("96000Hz • out 24-bit").
fn rate_label(src_rate: u32, stream_rate: u32, state: &PlayerState) -> String {
    let rate = if src_rate != stream_rate {
        format!("{}→{}Hz", src_rate, stream_rate)
    } else {
        format!("{}Hz", src_rate)
    };
    match state.output_bits.load(Ordering::Relaxed) {
        0 => rate,
        bits => format!("{rate} • out {bits}-bit"),
    }
}

/// Name of the producer (decode) thread. The panic hook uses it to tell a
/// caught decoder panic apart from one that is really taking Keet down.
const PRODUCER_THREAD: &str = "keet-producer";

/// Apply a pending full-screen repaint (resize, theme or viz-layout change).
/// Returns true when the screen was repainted from scratch, so the caller
/// resets its frame bookkeeping (`prev_frame_lines = usize::MAX`, any Kitty
/// image gone). Must run after the DEC 2026 sync-begin.
///
/// Every loop that draws frames calls this — the steady-state loop, the
/// start-of-track buffering wait and the exclusive-mode rate-change wait. The
/// buffering wait used to render without it, so a Shift+F or resize pressed in
/// the ~1 s after a track change drew the new layout over the old one.
fn repaint_if_needed(ui: &mut state::UiState) -> bool {
    if !ui.terminal_resized {
        return false;
    }
    ui.terminal_resized = false;
    // Remove any placed Kitty graphic: the frame redraws (and re-places) it.
    // No-op on terminals that don't speak the protocol.
    let kitty_clear = if matches!(cover::detect_protocol(), cover::GraphicsProtocol::Kitty) {
        format!("{}{}", cover::kitty_clear_escape(), cover::viz_image_clear_escape())
    } else {
        String::new()
    };
    // Home + erase-down (NOT \x1B[2J): ConPTY implements ED2 by scrolling the
    // viewport into scrollback, so on Windows Terminal a 2J repaint shoves the
    // whole UI out of sight instead of refreshing in place. ED0 from home
    // erases the same cells without the scroll.
    crate::term::out!("{}\x1B[0m\x1B[H\x1B[J", kitty_clear);
    ui.cover_block_intact = false;
    true
}

/// Kick off the lyrics loader on a background thread and install its receiver on `ui`.
/// Reads embedded tags from the file if not already cached, then falls back to LRCLIB.
/// The main thread never blocks on disk or HTTP.
fn spawn_lyrics_worker(ui: &mut state::UiState, path: std::path::PathBuf, dur: Option<u32>) {
    // A manual sync fix belongs to the track it was made for, and is kept.
    ui.lyrics_offset = ui.lyrics_offsets.get(&path);
    if let Some(l) = ui.metadata_cache.lyrics(ui.current) {
        ui.lyrics = Some(lyrics::parse_lyrics(&l));
        ui.lyrics_source = Some(lyrics::LyricsSource::Embedded);
        ui.lyrics_receiver = None;
        return;
    }
    let (cached_artist, cached_title) = ui.metadata_cache.artist_title(ui.current);
    ui.lyrics = None;
    ui.lyrics_source = None;
    let (tx, rx) = std::sync::mpsc::channel();
    ui.lyrics_receiver = Some(rx);
    // Bump the generation; each worker snapshots this and bails out of the slow
    // LRCLIB fetch if the user has skipped to another track in the meantime.
    // This prevents a backlog of blocked HTTP threads during rapid skipping.
    let gen_snap = ui.lyrics_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let gen_ref = std::sync::Arc::clone(&ui.lyrics_gen);
    let net_cache = ui.lyrics_lookups.clone();
    std::thread::spawn(move || {
        let (artist, title, embedded) = if cached_artist.is_some() || cached_title.is_some() {
            (cached_artist, cached_title, metadata::read_lyrics(&path))
        } else {
            metadata::read_artist_title_lyrics(&path)
        };
        let res = if let Some(l) = embedded {
            Some((lyrics::parse_lyrics(&l), lyrics::LyricsSource::Embedded))
        } else if let (Some(a), Some(t)) = (artist, title) {
            // Skip the network round-trip if a newer request has already been issued.
            if gen_ref.load(std::sync::atomic::Ordering::Relaxed) != gen_snap {
                None
            } else {
                let key = format!("{a}\0{t}\0{}", dur.unwrap_or(0));
                net_cache
                    .get_or_fetch(key, || lyrics::fetch_lrclib(&a, &t, dur))
                    .map(|s| (lyrics::parse_lyrics(&s), lyrics::LyricsSource::Lrclib))
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
fn spawn_cover_worker(ui: &mut state::UiState, path: std::path::PathBuf, size: cover::CoverSize) {
    if !ui.cover_enabled {
        ui.cover = None;
        ui.cover_receiver = None;
        ui.cover_dirty_frame = true;
        return;
    }
    let (cached_artist, cached_album) = ui.metadata_cache.artist_album(ui.current);
    ui.cover = None;
    ui.cover_dirty_frame = true;
    let (tx, rx) = std::sync::mpsc::channel();
    ui.cover_receiver = Some(rx);
    let gen_snap = ui.cover_gen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let gen_ref = std::sync::Arc::clone(&ui.cover_gen);
    let misses = ui.cover_misses.clone();
    std::thread::spawn(move || {
        // Local sources are cheap — always try them regardless of generation.
        let local = cover::resolve_local(
            &path,
            cached_artist.as_deref(),
            cached_album.as_deref(),
            size,
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
        // A found cover lands in the on-disk cache (resolve_local above);
        // what is remembered here is an album iTunes does not have, so it is
        // not searched for again on every play.
        let remote = match (cached_artist, cached_album) {
            (Some(a), Some(al)) => {
                let key = format!("{a}\0{al}");
                if misses.known(&key).is_some() {
                    None
                } else {
                    match cover::resolve_remote(&a, &al, size) {
                        lyrics::Lookup::Found(img) => Some(img),
                        lyrics::Lookup::NotFound => {
                            misses.remember(key, None);
                            None
                        }
                        lyrics::Lookup::Failed => None,
                    }
                }
            }
            _ => None,
        };
        let _ = tx.send(remote);
    });
}

/// Show the terminal cursor again, ignoring write errors.
///
/// **Must not panic.** The panic hook calls this, and `print!` panics when the
/// underlying write fails. With stdout closed (`keet --help | head`) the
/// original panic would trigger a second panic here — and panicking while
/// panicking aborts the process instead of exiting cleanly. Writing through a
/// handle and discarding the `Result` keeps this total.
/// Restores the terminal when dropped (see its use in main).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        crate::term::flush();
        let _ = terminal::disable_raw_mode();
        restore_cursor(&mut io::stdout());
    }
}

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
/// Audio producer, viz consumer, output stream, and the rate the stream
/// ACTUALLY runs at, returned by `rebuild_stream`. The rate is part of the
/// result because the fallback path can land on a different rate than the one
/// requested — a caller that kept its own copy then spawned the producer
/// resampling for the rejected rate, playing everything off-speed.
type StreamParts = (rtrb::Producer<f32>, rtrb::Consumer<f32>, audio::Output, u32);

fn rebuild_stream(
    device: &cpal::Device,
    stream_rate: u32,
    channels: u16,
    buffer_size: cpal::BufferSize,
    state: &Arc<PlayerState>,
) -> Result<StreamParts, Box<dyn std::error::Error>> {
    // `state.ring_capacity` (the UI's buffer gauge) is stored only once a
    // stream has opened: a failed open keeps the old ring, and the size has
    // to stay the old ring's. The producer reads the size from its own ring.
    let ring_cap = ring_capacity_for(stream_rate);
    let (prod, cons) = RingBuffer::<f32>::new(ring_cap);
    let (viz_prod, viz_cons) = RingBuffer::<f32>::new(VIZ_BUFFER_SIZE);

    // Windows exclusive mode: Keet's own WASAPI exclusive stream (cpal only
    // opens shared mode), in the most precise layout the device accepts at
    // this rate. A device in use comes back as DeviceBusyError.
    #[cfg(target_os = "windows")]
    if state.exclusive.load(Ordering::Relaxed) {
        let id = device.id().map(|i| i.id().to_string())?;
        // No layout found usually means another program holds the device (the
        // probe cannot tell); opening anyway reports THAT, or the device's own
        // reason if the rate really is unsupported.
        let layout = crate::wasapi_out::best_layout(&id, stream_rate, channels).unwrap_or((32, 24));
        let out = crate::wasapi_out::WasapiOutput::start(
            &id, stream_rate, channels, layout, cons, viz_prod, Arc::clone(state),
        )
        .map_err(|e| -> Box<dyn std::error::Error> {
            if audio::is_device_busy(e.as_ref()) {
                format!(
                    "the output device is busy: another program is using it exclusively. \
                     Close it (or turn off \"Allow applications to take exclusive control\" for it), \
                     then try again ({e})"
                ).into()
            } else {
                e
            }
        })?;
        state.output_bits.store(layout.1 as u32, Ordering::Relaxed);
        state.ring_capacity.store(ring_cap, Ordering::Relaxed);
        return Ok((prod, viz_cons, audio::Output::Wasapi(out), stream_rate));
    }

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
    // Float everywhere except Linux exclusive mode, where a raw hw: device is
    // opened in the integer format that carries the most bits exactly.
    let exclusive = state.exclusive.load(Ordering::Relaxed);
    let (format, bits) = audio::output_format(device, stream_rate, channels, exclusive);
    if exclusive && cfg!(target_os = "linux") {
        state.output_bits.store(bits, Ordering::Relaxed);
        if bits == 16 {
            // cpal cannot open packed 24-bit (S24_3LE), so a DAC offering only
            // that and 16-bit is left with 16: say so rather than truncate
            // 24-bit files in silence.
            if let Ok(mut err) = state.decode_error.lock() {
                *err = Some("device accepts only 16-bit here — 24-bit files are rounded to 16".to_string());
            }
        }
    }
    let stream = match build_stream(device, &config, format, cons, viz_prod, Arc::clone(state)) {
        Ok(s) => s,
        // A busy device (another program — usually PipeWire or PulseAudio —
        // holds the ALSA hw: device) cannot be fixed by a fallback config: say
        // what is wrong and what to do instead.
        Err(e) if audio::is_device_busy(e.as_ref()) => {
            return Err(format!(
                "the output device is busy: another program is using it (on Linux usually \
                 PipeWire or PulseAudio holding the card). Exclusive mode needs the card to \
                 itself — use a card no sound server is using, or free it first ({e})"
            ).into());
        }
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
            state.output_rate.store(rate as u64, Ordering::Relaxed);
            let (p, c) = RingBuffer::<f32>::new(ring_cap);
            let (vp, vc) = RingBuffer::<f32>::new(VIZ_BUFFER_SIZE);
            let cfg = StreamConfig {
                channels: fallback.channels(),
                sample_rate: rate,
                buffer_size: cpal::BufferSize::Default,
            };
            let s = build_stream(device, &cfg, cpal::SampleFormat::F32, c, vp, Arc::clone(state))?;
            state.ring_capacity.store(ring_cap, Ordering::Relaxed);
            return Ok((p, vc, audio::Output::Cpal(s), rate));
        }
    };
    state.ring_capacity.store(ring_cap, Ordering::Relaxed);
    Ok((prod, viz_cons, audio::Output::Cpal(stream), stream_rate))
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

    install_panic_hook();

    let args: Vec<String> = env::args().collect();

    let opts = match cli::parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("Run with --help for usage information");
            std::process::exit(1);
        }
    };

    if opts.help {
        cli::print_help();
        return Ok(());
    }
    if opts.list_devices {
        let host = cpal::default_host();
        audio::list_output_devices(&host, opts.verbose);
        return Ok(());
    }
    if let Some(name) = &opts.unknown_theme {
        eprintln!("Unknown theme '{}' (expected: classic, minimal, hifi)", name);
    }

    // Full terminal reset in case a previous run crashed mid-draw.
    // \x1Bc = RIS (Reset to Initial State) - clears screen, resets charset,
    // tab stops, modes. Deliberately AFTER the print-and-exit flags above so
    // it only fires on the path that actually draws the TUI.
    crate::term::out!("\x1Bc");
    crate::term::flush();

    // Loaded once and reused for the volume/EQ/device restore further down.
    let resume_state_loaded = if opts.resume { load_state() } else { None };
    let (source_paths, shuffle, repeat_mode) = if opts.resume {
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
                        eprintln!("Usage: {} <file-or-folder>... [options]  (--help for all of them)", args[0]);
                        std::process::exit(1);
                    }
                }
            }
        }
    } else {
        let repeat = if opts.repeat { state::RepeatMode::All } else { state::RepeatMode::Off };
        (opts.sources.clone(), opts.shuffle, repeat)
    };
    let hq_resampler = opts.hq_resampler;
    let eq_arg = opts.eq.clone();
    let fx_arg = opts.fx.clone();
    let crossfade_secs = opts.crossfade_secs;
    let rg_mode: RgMode = opts.rg_mode;
    let device_arg: Option<String> = opts.device.clone();
    let exclusive = opts.exclusive;
    // A half-block cover is nothing but colour: with NO_COLOR it would be a
    // slab of identical blocks. Image protocols are pictures, not text colour.
    let cover_enabled = opts.cover
        && !(term::no_color()
            && matches!(cover::detect_protocol(), cover::GraphicsProtocol::HalfBlock));
    let theme_arg: Option<theme::ThemeKind> = opts.theme;
    // Persistent user preferences (config.json) — applies on every launch.
    let app_config = config::load();

    let playlist = load_initial_playlist(&source_paths, shuffle)?;
    let state = Arc::new(PlayerState::new());
    state.total_tracks.store(playlist.len(), Ordering::Relaxed);

    // Load EQ presets (built-in + custom from ~/.config/keet/eq/)
    let mut eq_presets = eq::builtin_presets();
    eq_presets.extend(eq::load_custom_presets());
    state.eq_preset_count.store(eq_presets.len(), Ordering::Relaxed);

    // Set initial EQ preset from --eq argument
    if let Some(idx) = eq_arg.as_deref().and_then(|n| pick_preset(&mut eq_presets, n, |p| &mut p.name)) {
        state.eq_preset_count.store(eq_presets.len(), Ordering::Relaxed);
        state.eq_preset_index.store(idx, Ordering::Relaxed);
    }

    // Load effects presets (built-in + custom from ~/.config/keet/effects/)
    let mut fx_presets = effects::builtin_presets();
    fx_presets.extend(effects::load_custom_presets());
    state.effects_preset_count.store(fx_presets.len(), Ordering::Relaxed);

    if let Some(idx) = fx_arg.as_deref().and_then(|n| pick_preset(&mut fx_presets, n, |p| &mut p.name)) {
        state.effects_preset_count.store(fx_presets.len(), Ordering::Relaxed);
        state.effects_preset_index.store(idx, Ordering::Relaxed);
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
    let resume_position = resume_state_loaded
        .as_ref()
        .map_or(0, |rs| apply_resume_state(&state, rs, &eq_presets, &fx_presets, &cf_presets));
    // Resolve the launch theme: --theme flag → config.json default → resumed
    // last-session theme → Classic. The config default applies on every launch
    // (including with explicit source paths), unlike the resume theme.
    let config_theme = app_config.theme.as_deref().and_then(theme::ThemeKind::from_str);
    let resume_theme = resume_state_loaded
        .as_ref()
        .and_then(|rs| rs.theme.as_deref())
        .and_then(theme::ThemeKind::from_str);
    state.set_theme(theme::resolve_theme(theme_arg, config_theme, resume_theme));
    // Classic in truecolor, when config.json asks; set before the first frame.
    let classic_colour_problems = if app_config.classic_use_truecolor {
        let ([hl, warn, err], bad) = app_config.classic_colors.resolve();
        theme::use_classic_truecolor(hl, warn, err);
        bad
    } else {
        Vec::new()
    };

    // Apply remaining config.json defaults. Each overrides the resumed value but
    // yields to an explicit CLI flag for the same setting. (CLI flags only occur
    // with explicit paths, and resume only on a bare launch, so checking flag
    // presence gives the right priority in both modes.)
    apply_config(&state, &app_config, args.iter().any(|a| a == "--rg-mode"), eq_arg.is_some(), &eq_presets, &cf_presets);

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
    // The resumed value must reach state too: it was stored earlier from the
    // command-line flag only, so a bare `keet` resuming an exclusive session
    // took hog mode (the local) while rate switching, the bit depth and error
    // handling (all reading state) ran as non-exclusive — and the next save
    // wrote exclusive: false, dropping it from the resume for good.
    state.exclusive.store(exclusive, Ordering::Relaxed);

    let eq_presets = Arc::new(eq_presets);
    let fx_presets = Arc::new(fx_presets);

    // Name of the output device, shown in the header and kept honest by the
    // default-device poll — the OS can move playback without telling us.
    let shown_device_name: String;

    // Create UI state first so shuffle/repeat have a single home (ui.*). The
    // parsed `shuffle`/`repeat_mode` locals feed it once here and are not read
    // again — every later reader uses ui.shuffle / ui.repeat_mode.
    let metadata_cache = metadata::MetadataCache::new(playlist.len());
    let mut ui = UiState::new(source_paths, std::sync::Arc::clone(&metadata_cache));
    ui.shuffle = shuffle;
    ui.repeat_mode = repeat_mode;
    ui.hq_resampler = hq_resampler;
    // Unreadable config values: each was skipped on its own (config::parse).
    if !app_config.problems.is_empty() {
        ui.set_status_for(
            format!("config.json: ignored {}", app_config.problems.join(", ")),
            std::time::Duration::from_secs(8),
        );
    } else if !classic_colour_problems.is_empty() {
        ui.set_status_for(
            format!("config.json: classic_colors.{} is not a #RRGGBB colour — using the default", classic_colour_problems.join(", ")),
            std::time::Duration::from_secs(8),
        );
    }
    state.repeat_mode.store(repeat_mode as u8, Ordering::Relaxed);

    // Audio setup
    let host = cpal::default_host();
    let (device, device_warning) = select_device(&host, device_arg.as_deref())?;
    if let Some(w) = device_warning {
        ui.set_status_for(w, std::time::Duration::from_secs(8));
    }
    let current_output_rate = {
        let device_name = device.description()
            .map(|d| d.name().to_string())
            .unwrap_or_else(|_| "Unknown device".to_string());
        shown_device_name = device_name.clone();
        ui.device_name = device_name;

        // Fix stale sample rate on Bluetooth devices (CoreAudio can get stuck at wrong rate)
        let bt_rate = fix_bluetooth_sample_rate(&device);
        if let Some(rate) = bt_rate {
            ui.set_status_for(
                format!("Bluetooth device: using its native {rate} Hz"),
                std::time::Duration::from_secs(5),
            );
        }

        let default_config = device.default_output_config()?;
        bt_rate.unwrap_or_else(|| default_config.sample_rate())
    };

    // OS media transport controls (media keys, AirPods, Bluetooth headphones)
    let media_controls = media_keys::setup(Arc::clone(&state));

    terminal::enable_raw_mode()?;
    // From here on, ANY way out of main — an error returned through `?` (a
    // device busy at startup), a panic on this thread — must hand the terminal
    // back: raw mode left on eats Ctrl+C and echo in the user's shell.
    let _terminal_guard = TerminalGuard;

    // Hide cursor to prevent flickering
    crate::term::out!("\x1B[?25l");
    crate::term::flush();

    ui.cover_enabled = cover_enabled;
    ui.terminal_resized = false;
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

    // --- Persistent audio setup (created once, reused across all tracks) ---
    // Declared before the stream, so the stream always drops first (see
    // audio::DeviceRestore).
    let mut device_restore = audio::DeviceRestore::default();
    let source_rate = probe_sample_rate(&playlist[ui.current]).unwrap_or(44100);
    let StartupOutput {
        device, note: mut startup_note, out_channels, stream_rate,
        buffer_size, prod, viz_cons, stream: first_stream,
    } = open_startup_output(&host, device, exclusive, source_rate, current_output_rate, &state, &mut device_restore)?;

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

    if let Some(note) = startup_note.take() {
        ui.set_status_for(note, Duration::from_secs(10));
    }

    let mut player = player::Player::new(player::PlayerSetup {
        stream: first_stream,
        device_restore,
        device,
        prod,
        viz_cons,
        stream_rate,
        out_channels,
        buffer_size,
        host,
        state: Arc::clone(&state),
        ui,
        playlist,
        eq_presets,
        fx_presets,
        cf_presets,
        hq_resampler,
        crossfade_secs,
        device_arg,
        save_tx,
        media_controls,
        resume_position,
        shown_device_name,
    });
    player.run();

    // Flush any queued resume-state writes before exit.
    player.close_saves();
    let _ = saver_handle.join();

    terminal::disable_raw_mode()?;

    crate::term::out!("\x1B[?25h");

    // Wipe the whole frame (header + status + viz + playlist/lyrics) and any
    // kitty graphic, leaving only the goodbye line.
    if matches!(cover::detect_protocol(), cover::GraphicsProtocol::Kitty) {
        crate::term::out!("{}{}", cover::kitty_clear_escape(), cover::viz_image_clear_escape());
    }
    // Home + erase-down, not ED2 — see the resize repaint above (ConPTY turns
    // 2J into a scrollback push on Windows Terminal).
    crate::term::out!("\x1B[H\x1B[J✓ Done\n");
    crate::term::flush();

    // Release exclusive mode and restore the DAC's format. Normally already
    // done (and taken) by the quit key, which silences the stream first; this
    // covers the rarer exits, most of which have already dropped the stream.
    player.release_output();

    // Exit immediately — implicit drops of cpal::Stream (ALSA backend) and
    // souvlaki::MediaControls (D-Bus) can block indefinitely on Linux, hanging
    // the process after the user presses Q.
    std::process::exit(0);
}

#[cfg(test)]
mod main_tests {
    use super::*;

    #[test]
    fn output_retries_back_off_to_eight_seconds() {
        let secs: Vec<u64> = (0..6).map(|n| recovery_backoff(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 8, 8]);
    }

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
    fn eq_and_fx_flags_take_a_preset_name_or_a_json_file() {
        let mut presets = eq::builtin_presets();
        let n = presets.len();
        let bass = presets.iter().position(|p| p.name == "Bass Boost").unwrap();
        assert_eq!(pick_preset(&mut presets, "bass BOOST", |p| &mut p.name), Some(bass), "any case");
        let file = std::env::temp_dir().join(format!("keet_pick_{}.json", std::process::id()));
        std::fs::write(&file, r#"{"name":"Mine","gains":[1,2,3,4,5,6,7,8,9,10]}"#).unwrap();
        let got = pick_preset(&mut presets, file.to_str().unwrap(), |p| &mut p.name);
        let _ = std::fs::remove_file(&file);
        assert_eq!(got, Some(n), "a file joins the list");
        assert_eq!(presets[n].name, "Mine");
        assert_eq!(pick_preset(&mut presets, "no-such-preset", |p| &mut p.name), None);
        // A name from a file is shown on screen: control characters go.
        let file = std::env::temp_dir().join(format!("keet_pick_esc_{}.json", std::process::id()));
        std::fs::write(&file, "{\"name\":\"Bad\\u001b[2Jname\",\"gains\":[0]}").unwrap();
        let got = pick_preset(&mut presets, file.to_str().unwrap(), |p| &mut p.name).unwrap();
        let _ = std::fs::remove_file(&file);
        assert!(!presets[got].name.contains('\x1B'), "{:?}", presets[got].name);
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
