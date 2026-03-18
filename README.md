# Keet

A high-performance, low-CPU terminal audio player with real-time spectrum visualization and parametric EQ.

## Features

- **Multi-format support**: MP3, FLAC, WAV, OGG, AAC/M4A, ALAC, AIFF
- **Low CPU usage**: <0.5% total system CPU (release mode)
- **Parametric EQ**: Built-in presets (Flat, Bass Boost, Treble Boost, Vocal, Loudness) + custom JSON presets
- **Audio effects**: Reverb, chorus, delay with built-in environment presets + custom JSON presets
- **Crossfade**: Smooth equal-power crossfade between tracks (`--crossfade`)
- **Pre/post-fader metering**: Toggle between raw signal and volume-adjusted visualization
- **Media controls**: AirPods stalk controls, Bluetooth headphone buttons, keyboard media keys (macOS/Windows/Linux)
- **Real-time visualizations**: VU meter, horizontal/vertical spectrum analyzer synced to playback, toggleable bars/dots style
- **Metadata display**: Reads artist/title from ID3, Vorbis, and MP4 tags
- **Format-colored icons**: File type indicated by icon color (green=MP3, cyan=FLAC, yellow=WAV, etc.)
- **Smart audio processing**: Automatic sample rate switching (macOS), Bluetooth detection, conditional resampling, seamless device switching
- **Volume control**: Adjustable 0-150% with per-sample gain
- **Playlist features**: Shuffle, repeat, recursive folder scanning with rescan on repeat
- **HQ resampler mode**: Optional `--quality` flag for audiophile-grade resampling
- **Resilient playback**: Silently skips missing/corrupt files, recovers from device disconnection
- **Terminal-safe UI**: Output adapts to terminal width, no line-wrapping artifacts

## Quick Start

```bash
# Play a single file
cargo run --release -- song.flac

# Play a folder (recursive)
cargo run --release -- ~/Music/

# With shuffle, repeat, and HQ resampler
cargo run --release -- ~/Music/ --shuffle --repeat --quality

# Start with Bass Boost EQ
cargo run --release -- ~/Music/ --eq "Bass Boost"

# With Concert Hall reverb and 3-second crossfade
cargo run --release -- ~/Music/ --fx "Concert Hall" --crossfade 3
```

**Note**: Release mode (`--release`) is required for acceptable performance.

## Keyboard Controls

| Key | Action |
|-----|--------|
| `Space` | Pause/Resume |
| `Up` | Next track |
| `Down` | Previous track |
| `Right` | Seek forward 10s |
| `Left` | Seek backward 10s |
| `V` | Cycle visualization modes |
| `B` | Toggle visualization style (bars/dots) |
| `E` | Cycle EQ presets |
| `R` | Cycle effects presets |
| `F` | Toggle pre/post-fader metering |
| `+` / `=` | Volume up (5%) |
| `-` | Volume down (5%) |
| `Q` / `Esc` | Quit |

## EQ Presets

### Built-in Presets

| Preset | Description |
|--------|-------------|
| Flat | No EQ (passthrough) |
| Bass Boost | +6dB at 32Hz, tapering to +1dB at 250Hz |
| Treble Boost | +2dB at 4kHz, rising to +5dB at 16kHz |
| Vocal | Cuts bass, boosts 1-4kHz midrange |
| Loudness | Boosts lows and highs (smiley curve) |

### Custom Presets

Drop JSON files into `~/.config/keet/eq/` (macOS/Linux) or `%APPDATA%\keet\eq\` (Windows):

```json
{
  "name": "My Preset",
  "bands": [
    {"freq": 60, "gain": 4.0, "q": 0.8},
    {"freq": 250, "gain": -2.0},
    {"freq": 4000, "gain": 3.0, "q": 1.2}
  ]
}
```

- `freq`: Center frequency in Hz
- `gain`: Boost/cut in dB (positive = boost, negative = cut)
- `q`: Filter bandwidth (default: 1.0, lower = wider)

Custom presets appear automatically when cycling with `E`.

Example presets are included in `assets/` — copy them to the presets folders as a starting point:

```bash
# macOS/Linux
mkdir -p ~/.config/keet/eq ~/.config/keet/effects
cp assets/eq-example.json ~/.config/keet/eq/
cp assets/fx-example.json ~/.config/keet/effects/

# Windows
copy assets\eq-example.json %APPDATA%\keet\eq\
copy assets\fx-example.json %APPDATA%\keet\effects\
```

## Effects Presets

### Built-in Presets

| Preset | Description |
|--------|-------------|
| None | No effects (passthrough) |
| Small Room | Subtle room ambience |
| Concert Hall | Large hall reverb |
| Cathedral | Long, spacious reverb |
| Studio | Tight reverb + light chorus |
| Chorus | Stereo chorus effect |
| Echo | Rhythmic delay with feedback |

### Custom Presets

Drop JSON files into `~/.config/keet/effects/` (macOS/Linux) or `%APPDATA%\keet\effects\` (Windows):

```json
{
  "name": "My Environment",
  "reverb": {
    "mix": 0.3,
    "room_size": 0.7,
    "damping": 0.5
  },
  "chorus": {
    "mix": 0.3,
    "rate": 1.5,
    "depth": 0.003
  },
  "delay": {
    "mix": 0.2,
    "time": 0.4,
    "feedback": 0.3
  }
}
```

All effect sections are optional — omit any to disable that effect. Custom presets appear when cycling with `R`.

Processing order: chorus → delay → reverb.

## Crossfade

Use `--crossfade <seconds>` (or `-x`) to enable smooth crossfade between tracks:

```bash
cargo run --release -- ~/Music/ --crossfade 3
```

Uses an equal-power crossfade curve for natural-sounding transitions. The previous track's tail is captured and mixed into the next track's beginning.

## Visualization Modes

Press `V` to cycle through:

1. **None** - Minimal UI, lower CPU
2. **VU Meter** - Stereo level meters with peak hold dots
3. **Spectrum Horizontal** - Stereo butterfly display (L channel up, R channel down)
4. **Spectrum Vertical** - 31-band analyzer with peak dots and height-based color gradient (green -> yellow -> red)

Press `B` to toggle between two visualization styles:
- **Dots** (default) - Braille characters (⣿⣀) for progress/VU, braille spectrum bars
- **Bars** - Block characters (█░) for VU, thin partials (▏▎▍▌▋▊▉) for progress

Press `F` to toggle between post-fader (shows volume-adjusted levels) and pre-fader (shows raw signal levels) metering.

The spectrum analyzer features:
- 31-band ISO 1/3-octave analysis (20Hz - 20kHz)
- Per-channel L/R FFT processing (4096-point)
- Unweighted display (no A-weighting — accurate for spectrum analysis)
- Fractional bin edge weighting for accurate low-frequency bands
- Hann window correction and dBFS-calibrated scale
- Spectral tilt correction (+3dB/octave relative to 1kHz)
- Peak hold dots with gravity

## Architecture

```
+-----------+    +------------------+   Ring Buffer   +------------------+
| Main      |    | Producer Thread  | --------------> | Audio Callback   |
| Thread    |    | (decode/resample)|   (lock-free)   | (playback/gain)  |
|           |    | (EQ/FX/xfade)   |                 +--------+---------+
| UI/input  |    +------------------+                          |
| viz/stats |    Viz Ring Buffer (stereo tap)                   |
|           | <------------------------------------------------+
+-----------+
              All shared state via atomics (Relaxed ordering)
```

### Source Layout

```
src/
├── main.rs      Entry point, CLI args, playlist loop
├── state.rs     PlayerState, VizMode, constants, ANSI colors
├── audio.rs     Audio stream, sample rate switching, CoreAudio FFI
├── decode.rs    Decoder thread, resampling, sample conversion
├── eq.rs        Biquad EQ filters, preset loading, JSON parsing
├── effects.rs   Reverb, chorus, delay effects with preset loading
├── playlist.rs  Playlist builder, metadata reader, shuffle
├── viz.rs       VizAnalyser, StatsMonitor, spectrum rendering
├── media_keys.rs  OS media transport controls (souvlaki)
└── ui.rs        Terminal UI, keyboard input, progress display
```

### Resampler Modes

| Mode | sinc_len | Interpolation | Use case |
|------|----------|---------------|----------|
| Default | 64 | Linear | Low CPU, transparent quality |
| `--quality` | 256 | Cubic | Negligible difference, peace of mind |

## Command Line

```
keet <file-or-folder> [options]

Options:
  --shuffle, -s     Randomize playlist order (re-shuffles on each repeat)
  --repeat, -r      Loop playlist (rescans folder for new files each cycle)
  --quality, -q     HQ resampler (higher CPU, inaudible difference)
  --eq, -e <name>   Start with EQ preset by name or JSON file path
  --fx <name>       Start with effects preset by name or JSON file path
  --crossfade, -x <secs>  Crossfade duration between tracks (0 = disabled)
```

## Dependencies

| Crate | Purpose |
|-------|---------|
| cpal 0.17 | Cross-platform audio I/O |
| symphonia 0.5 | Audio decoding (MP3, FLAC, WAV, OGG, AAC, ALAC, AIFF) |
| rubato 1.0 | Sample rate conversion |
| crossterm 0.29 | Terminal UI |
| rtrb 0.3 | Lock-free ring buffer |
| realfft 3.4 | FFT for spectrum analysis |
| sysinfo 0.32 | CPU/memory monitoring |
| serde 1.0 | JSON deserialization for EQ/effects presets |
| souvlaki 0.8 | OS media transport controls (media keys, AirPods, Bluetooth) |

## Platform Notes

- **macOS**: Automatic sample rate switching via CoreAudio; Bluetooth devices (AirPods etc.) detected and locked to native 48kHz; seamless device switching when audio output changes mid-playback; media keys via MPRemoteCommandCenter
- **Linux**: Works with PipeWire/PulseAudio/ALSA; falls back to device default rate if unsupported; media keys via MPRIS/D-Bus
- **Windows**: WASAPI shared mode with larger buffer (2048 samples) for lower CPU overhead; media keys via SMTC
- **WSL**: Auto-detected via `/proc/version`; uses larger buffer (2048 samples) to reduce crackling from PulseAudio virtualization

## Building

### Linux/WSL Dependencies

```bash
sudo apt install libasound2-dev libdbus-1-dev
```

- `libasound2-dev` — ALSA headers (required by cpal)
- `libdbus-1-dev` — D-Bus headers (required by souvlaki for MPRIS media keys)

### Compile

```bash
cargo build --release
```

The binary is at `target/release/keet`. Copy to `/usr/local/bin/` for system-wide access.

### macOS .app Bundle

```bash
bash scripts/bundle-macos.sh
```

Creates `Keet.app` with the app icon, ready to drag to `/Applications`.

Since Keet is a terminal app, launch it from Terminal after installing:

```bash
/Applications/Keet.app/Contents/MacOS/keet ~/Music/ --shuffle --repeat
```

### Windows

The `.exe` automatically includes the app icon when built on Windows.

## Future Improvements

- **GUI file picker**: Native file/folder dialog so the .app bundle can be launched without Terminal arguments
- **Drag-and-drop**: Drop audio files or folders onto the .app icon to start playback

## License

GPL-3.0
