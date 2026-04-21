# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build --release          # Release mode required for acceptable audio performance
cargo run --release -- ~/Music # Play a folder
cargo clippy --all-targets     # Lint (treat warnings as informational, errors must be fixed)
cargo test                     # Run tests (currently minimal)
```

Linux build deps: `sudo apt install libasound2-dev libdbus-1-dev`

Version is embedded from git tags via `build.rs` (`GIT_VERSION` env var). The release profile uses `strip = true` and `lto = true`.

## Architecture

### Three-Thread Model

1. **Main/UI thread** (`main.rs`, `ui.rs`) — polls input at ~50fps, renders ANSI UI top-to-bottom, runs visualization analysis (FFT/VU), detects track transitions via atomic `track_transition_count` (Acquire ordering)
2. **Producer/decode thread** (`decode.rs`) — decodes audio (symphonia), applies full DSP chain, writes to lock-free ring buffer (`rtrb`). Sleeps when buffer >75% full
3. **Audio callback** (`audio.rs`) — cpal callback, reads ring buffer, applies volume gain, taps samples to viz buffer. **Zero locks in the callback**

### Shared State

`PlayerState` in `state.rs` uses 40+ atomics (no Mutex on hot paths). Key patterns:
- **Relaxed ordering** for independent UI state (volume, viz mode, EQ index)
- **Release/Acquire** for `track_transition_count` (producer signals → main thread reads)
- **Swap-to-consume** pattern: `take_skip_next()`, `take_seek()`, `take_eq_changed()` — read-once signals
- **f32-as-bits**: spectrum/peak values stored via `f32::to_bits()` / `f32::from_bits()` in AtomicU32

### Ring Buffers

- **Audio ring buffer**: sized per output rate via `state::ring_capacity_for(rate)` = `rate * 2 * 4` (~4 sec stereo). 48 kHz → 1.5 MB, 96 kHz → 3 MB, 192 kHz exclusive → 6 MB. Capacity is stored on `state.ring_capacity` so decode.rs and ui.rs can compute fill levels without a constant. Re-sized in main.rs whenever the ring is rebuilt (initial setup, exclusive-mode rate switch, stream-error device swap). Lock-free `rtrb` crate
- **Viz ring buffer**: 8,192 samples. Audio callback writes best-effort tap, UI thread reads for FFT/VU analysis

### DSP Chain (producer thread, in order)

`decode → resample → EQ (biquad) → effects (reverb/chorus/delay) → ReplayGain → crossfeed → balance → crossfade mix → peak limiter → ring buffer → [audio callback] → volume → output`

### Consumer-Side Sample Counting

`samples_played` is incremented in the **audio callback** (consumer), not the decode thread (producer). This is critical — the producer runs ~4 seconds ahead of playback. Counting on the producer side causes lyrics sync, progress bar, and seek to be off by the ring buffer depth.

### Pre-Allocated Buffers

The decode loop uses reusable `Vec<f32>` buffers (decoded_buf, eq_buf, fx_buf, etc.) that are `.clear()`ed each iteration to retain capacity. Do not replace with `std::mem::take()` — that drops capacity and forces per-chunk malloc.

### Crossfade Tail

Uses `VecDeque<f32>` to capture the last N samples of a track. `VecDeque::drain(..excess)` from the front is O(1), unlike `Vec::drain` which shifts the entire buffer.

## Platform-Specific Code

- **macOS** (`#[cfg(target_os = "macos")]`): CoreAudio FFI in `audio.rs` for sample rate switching, Bluetooth detection (forces 48kHz), hog mode (exclusive). Uses `coreaudio-sys` bindings
- **Windows**: Larger WASAPI buffer (2048 samples), `winresource` for icon/version embedding in `build.rs`
- **Linux/WSL**: WSL detected via `/proc/version` for buffer sizing. ALSA/PipeWire via cpal

## Stream Error Recovery

When the audio device disconnects (USB DAC unplugged, AirPods removed), the cpal error callback sets `stream_error` atomic. The main loop detects this, joins the producer thread (which may be stuck in the buffer-full sleep loop since the callback stopped draining), drops the old stream, rebuilds on the new default device with fresh ring buffers, and resumes from the current track via `continue 'playlist`.

## Lyrics Loading

Lyrics are resolved in priority order: embedded tags (metadata cache) → standalone tag read → LRCLIB API fetch. The LRCLIB fetch is async — on track transitions, it spawns a thread that sends the result via `mpsc::channel` to `ui.lyrics_receiver`, so the UI doesn't block on HTTP. Duration is passed to LRCLIB for accurate version matching.

## Gapless Transitions

The producer thread runs a continuous loop across tracks — it signals `track_transition_count` (Release) and the main thread picks it up (Acquire) to update UI/metadata. The `samples_played.store(0)` reset happens on the producer side, which means the progress bar snaps to 0:00 slightly before the audio transition is audible (by the ring buffer depth). This is a known minor UI artifact.

## CLI Argument Parsing

Arguments are parsed manually in `main.rs` (no clap dependency). Multiple source paths (files, folders, M3U playlists) are accepted and deduplicated. Running `keet` with no args resumes from `~/.config/keet/state.json`.

## Terminal Safety

A custom panic hook restores terminal from raw mode, shows the cursor, and appends to `~/.config/keet/crash.log`. On startup, the terminal is reset (`\x1Bc`) to clean up after any previous crash.

## Key Design Constraints

- **No locks in the audio callback** — all data exchange via atomics and lock-free ring buffers
- **native-tls, not rustls** — ureq uses OS TLS stack (Schannel/Security.framework) to avoid ~1.5MB binary bloat from rustls/ring. The TLS provider must be set explicitly: `TlsProvider::NativeTls`
- **Metadata scan is multi-threaded** — work-stealing via `AtomicUsize::fetch_add` index, capped at `min(available_parallelism, 4)` threads since it's I/O-bound
- **UI renders sequentially** — top-to-bottom ANSI escape output, cursor-up to redraw. Not a cursor-addressed layout. Terminal width checked but no side-by-side panels
- **Playlist scroll uses scroll margin** (scrolloff=4) — viewport moves before cursor reaches edge, like Vim

## Module Responsibilities

- `state.rs` — `PlayerState` (atomics), `UiState` (UI-local mutable state), constants, ANSI color codes
- `decode.rs` — producer thread: decode loop, gapless transitions, DSP chain application, seek handling
- `audio.rs` — cpal stream setup, audio callback, CoreAudio FFI (macOS), device enumeration
- `ui.rs` — terminal rendering, keyboard input polling, playlist/lyrics view modes
- `metadata.rs` — `MetadataCache` with parallel background scan, tag reading (artist/title/lyrics/ReplayGain)
- `lyrics.rs` — LRC parser, LRCLIB HTTP client, synced/plain lyrics types
- `eq.rs` — biquad parametric EQ, preset loading from JSON
- `effects.rs` — reverb/chorus/delay chain, preset loading
- `crossfeed.rs` — Meier-style headphone crossfeed filter
- `playlist.rs` — recursive folder scan, M3U parse/save, rescan diffing
- `resume.rs` — session state persistence to `~/.config/keet/state.json`
- `viz.rs` — FFT spectrum analysis (31-band ISO 1/3-octave), VU metering, process stats (platform syscalls)
