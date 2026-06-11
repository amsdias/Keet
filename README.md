# Keet

A high-performance, low-CPU terminal audio player with real-time spectrum visualization, parametric EQ, and synced lyrics.

## Features

- **Multi-format support**: MP3, FLAC, WAV, OGG, AAC/M4A, ALAC, AIFF
- **Any channel layout**: Mono, stereo, quad, 5.1, and 7.1 sources all play correctly — surround is downmixed to stereo (ITU-style: center at -3dB into both sides, LFE dropped)
- **Low CPU usage**: <0.5% total system CPU (release mode)
- **Synced lyrics**: Embedded LRC lyrics + automatic fetching from LRCLIB (~3M songs), with adjustable sync offset
- **Parametric EQ**: Built-in presets (Flat, Bass Boost, Treble Boost, Vocal, Loudness) + custom JSON presets
- **Audio effects**: Reverb, chorus, delay with built-in environment presets + custom JSON presets
- **Gapless playback**: Sample-accurate track transitions with continuous audio stream
- **ReplayGain**: Loudness normalization with peak-based clipping prevention (`--rg-mode track|album|off`)
- **Crossfade**: Smooth equal-power crossfade between tracks (`--crossfade`)
- **Pre/post-fader metering**: Toggle between raw signal and volume-adjusted visualization
- **Media controls**: AirPods stalk controls, Bluetooth headphone buttons, keyboard media keys, OS seek bar / skip-by seeking (macOS/Windows/Linux)
- **Real-time visualizations**: VU meter, horizontal/vertical spectrum, oscilloscope, lissajous (vector scope), spectrogram, and a high-resolution pixel-image analysis spectrogram (Kitty graphics / Sixel) — toggleable bars/dots styles
- **Album cover art**: Auto-decoded from embedded tags or sidecar files; rendered via Kitty / iTerm2 / Sixel graphics protocols, with truecolor half-block fallback (used automatically for WezTerm on Windows, where ConPTY drops Kitty graphics)
- **Metadata display**: Reads artist/title/album/track number from ID3, Vorbis, and MP4 tags
- **Format-colored icons**: File type indicated by icon color (green=MP3, cyan=FLAC, yellow=WAV, etc.)
- **Output device selection**: `--device` selects by name, `--list-devices` enumerates
- **Exclusive mode**: Per-track sample rate matching, macOS hog mode for bit-perfect playback (`--exclusive`)
- **Headphone crossfeed**: Meier-style frequency-dependent crossfeed with three presets (Light/Medium/Strong)
- **Balance control**: Stereo balance with `[`/`]` keys (5% steps, -100 to +100)
- **Clipping indicator**: Persistent dot that turns red when signal exceeds 0dBFS, with peak safety limiter
- **Smart audio processing**: Automatic sample rate switching (macOS), Bluetooth detection, conditional resampling, seamless device switching
- **Volume control**: Adjustable 0-150% with per-sample gain
- **Playlist features**: Shuffle (toggling off restores the previous order — M3U order survives), repeat (all/one), recursive folder scanning, playlist view with search/sort/track durations/album column, page navigation (Home/End/PgUp/PgDn + vim `g`/`G`/Ctrl+U/D), tag-based sort (artist → album → disc → track), play queue (enqueue tracks after current), M3U import/export, folder rescan, multiple source paths with deduplication
- **Resume playback**: Save and restore last session (track, position, volume, EQ, effects, crossfeed, balance, device, exclusive) automatically
- **HQ resampler mode**: Optional `--quality` flag for audiophile-grade resampling
- **Resilient playback**: Skips missing/corrupt files with a status message, recovers from device disconnection (including USB DAC unplug)
- **Terminal-safe UI**: Output adapts to terminal width, handles terminal resize gracefully
- **Process stats**: Lightweight CPU/memory monitoring via direct platform syscalls (toggle with `I`)

## Quick Start

```bash
# Play a single file
cargo run --release -- song.flac

# Play a folder (recursive)
cargo run --release -- ~/Music/

# Multiple folders
cargo run --release -- ~/Music/Jazz ~/Music/Rock

# Mix M3U playlist with a folder
keet ~/Music/favorites.m3u ~/Music/NewAlbum

# Multiple files and folders (duplicates removed automatically)
keet song.flac ~/Music/Jazz ~/Music/Rock

# With shuffle, repeat, and HQ resampler
cargo run --release -- ~/Music/ --shuffle --repeat --quality

# Start with Bass Boost EQ
cargo run --release -- ~/Music/ --eq "Bass Boost"

# With Concert Hall reverb and 3-second crossfade
cargo run --release -- ~/Music/ --fx "Concert Hall" --crossfade 3

# List available output devices
keet --list-devices

# Play on a specific device with exclusive mode
keet ~/Music/ --device "USB Audio DAC" --exclusive

# Resume last session (no arguments)
keet

# Play an M3U playlist
keet ~/Music/favorites.m3u
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
| `L` | Toggle playlist view |
| `Y` | Toggle lyrics view |
| `V` | Cycle visualization modes |
| `B` | Toggle visualization style (bars/dots) |
| `E` | Cycle EQ presets |
| `X` | Cycle effects presets |
| `Shift+R` | Cycle repeat mode (Off → All → One) |
| `Z` | Toggle shuffle |
| `R` | Rescan folders for changes |
| `S` | Save playlist as M3U |
| `O` | Open a new source (type a path) |
| `P` | Open a new source (native folder picker) |
| `F` | Toggle pre/post-fader metering |
| `C` | Cycle crossfeed presets (Off/Light/Medium/Strong) |
| `I` | Toggle CPU/memory stats display |
| `[` | Balance left (5% steps) |
| `]` | Balance right (5% steps) |
| `+` / `=` | Volume up (5%) |
| `-` | Volume down (5%) |
| `Q` / `Esc` | Quit |

### Playlist View Controls

Press `L` to open the playlist view, which replaces the visualization area with a scrollable track list.

| Key | Action |
|-----|--------|
| `Up` / `Down` | Move cursor |
| `Home` / `End` (or `g` / `G`) | Jump to top / bottom |
| `PgUp` / `PgDn` (or `Ctrl+U` / `Ctrl+D`) | Page up / down |
| `Enter` | Jump to selected track |
| `A` | Enqueue selected track (move it to play next) |
| `Shift+S` | Sort by tags (artist → album → disc → track → title → filename) |
| `/` | Search/filter by filename or tags |
| `D` / `Delete` | Remove selected track |
| `S` | Save playlist as M3U |
| `Esc` / `L` | Close playlist view |

Album and track durations are shown in right-aligned columns. The cursor follows the currently playing track on transitions. Sort works with whatever metadata is loaded — untagged tracks fall to the bottom and sort by filename. Sorting also turns shuffle off.

While searching (`/`), type to filter tracks by filename (case-insensitive). Press `Enter` to jump to the selected match, or `Esc` to cancel.

### Lyrics View Controls

Press `Y` to open the lyrics view. Synced lyrics auto-scroll to the current line; plain lyrics show as static text.

| Key | Action |
|-----|--------|
| `W` / `S` | Scroll up/down (disables auto-scroll for synced lyrics) |
| `A` / `D` | Adjust sync offset -/+0.5s (synced lyrics only) |
| `Up` / `Down` | Next/previous track (global) |
| `Left` / `Right` | Seek +/-10s (global) |
| `Esc` / `Y` | Close lyrics view |

Lyrics are loaded from embedded tags first (USLT/ID3v2, Vorbis comments, iTunes atoms), then fetched from [LRCLIB](https://lrclib.net) if not found. LRCLIB matches by artist, title, and duration for accurate results. Synced lyrics (LRC format) are preferred over plain text.

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

Example presets are included in `assets/` -- copy them to the presets folders as a starting point:

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
    "wet": 0.5,
    "room_size": 0.7,
    "damping": 0.5
  },
  "chorus": {
    "wet": 0.3,
    "rate": 1.5,
    "depth": 3.0
  },
  "delay": {
    "wet": 0.2,
    "delay_ms": 400.0,
    "feedback": 0.3
  }
}
```

All effect sections are optional -- omit any to disable that effect. Custom presets appear when cycling with `X`.

Processing order: chorus -> delay -> reverb.

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
5. **Oscilloscope** - Mono waveform; `Bars` style uses quadrant blocks for 2x sub-cell resolution
6. **Lissajous** - Stereo vector scope (L vs R); detects mono/stereo/anti-phase imbalance
7. **Spectrogram** - Scrolling time-frequency heatmap, dense colormap palette
8. **Analysis Spectrogram** - High-resolution time × frequency spectrogram with a magma colormap, rendered as a true pixel image — fine enough to reveal detail like images encoded into audio. On Kitty-protocol terminals (Kitty, Ghostty, WezTerm) it uses Kitty graphics with in-place image replacement; on Windows Terminal it uses a custom fixed-palette Sixel encoder that only re-transmits when the image changes, so scrolling stays smooth and shimmer-free even through ConPTY. Falls back to half-block truecolor elsewhere (iTerm2, WezTerm on Windows, plain terminals). The block auto-sizes to fit the window height. Linear frequency axis by default (`B` toggles linear/log); scroll cadence is matched to the sample rate for smooth motion.

Press `B` to toggle between two visualization styles:
- **Dots** - Braille characters for sub-cell precision (used by VU/oscilloscope/lissajous/spectrogram)
- **Bars** - Block characters; oscilloscope uses 2x2 quadrant blocks for smoother edges

(In **Analysis Spectrogram** mode, `B` instead toggles the frequency axis between linear and logarithmic.)

Press `F` to toggle between post-fader (shows volume-adjusted levels) and pre-fader (shows raw signal levels) metering.

The spectrum analyzer features:
- 31-band ISO 1/3-octave analysis (20Hz - 20kHz)
- Per-channel L/R FFT processing (4096-point)
- Unweighted display (no A-weighting -- accurate for spectrum analysis)
- Fractional bin edge weighting for accurate low-frequency bands
- Hann window correction and dBFS-calibrated scale
- Spectral tilt correction (+3dB/octave relative to 1kHz)
- Peak hold dots with gravity

## Architecture

```
+-----------+    +------------------+   Ring Buffer   +------------------+
| Main      |    | Producer Thread  | --------------> | Audio Callback   |
| Thread    |    | (decode/resample)|   (lock-free)   | (playback/gain)  |
|           |    | (EQ/FX/RG/CF/BAL/xfade)|           +--------+---------+
| UI/input  |    | (gapless loop)  |                          |
| viz/stats |    +------------------+  Viz Ring Buffer         |
|           | <------------------------------------------------+
+-----------+
              All shared state via atomics (Release/Acquire for transitions)
```

DSP chain: `decode -> to-stereo -> resample -> EQ -> effects -> RG gain -> crossfeed -> balance -> crossfade -> peak limiter -> clipping check -> ring buffer -> volume -> output`

Playback position is tracked on the consumer side (audio callback) for accurate time display and lyrics sync.

### Source Layout

```
src/
├── main.rs        Entry point, CLI args, playlist loop, lyrics loading
├── state.rs       PlayerState, UiState, ViewMode, constants, ANSI colors
├── audio.rs       Audio stream, sample rate switching, CoreAudio FFI
├── decode.rs      Continuous decoder thread, gapless playback, ReplayGain, resampling
├── eq.rs          Biquad EQ filters, preset loading, JSON parsing
├── effects.rs     Reverb, chorus, delay effects with preset loading
├── playlist.rs    Playlist builder, M3U parser, shuffle
├── crossfeed.rs   Meier-style headphone crossfeed filter
├── metadata.rs    Tag reading (artist, title, album, track #, lyrics, ReplayGain), background scan
├── lyrics.rs      LRC parser, LRCLIB API client, synced/plain lyrics state
├── cover.rs       Album cover decoding, Kitty/iTerm2/Sixel/half-block rendering
├── resume.rs      Resume state persistence (save/restore sessions)
├── viz.rs         VizAnalyser, StatsMonitor, spectrum/oscilloscope/lissajous/spectrogram rendering
├── media_keys.rs  OS media transport controls (souvlaki)
└── ui.rs          Terminal UI, keyboard input, progress display, lyrics/playlist views
```

### Resampler Modes

| Mode | sinc_len | Interpolation | Use case |
|------|----------|---------------|----------|
| Default | 64 | Linear | Low CPU, transparent quality |
| `--quality` | 256 | Cubic | Negligible difference, peace of mind |

## Command Line

```
keet <file-or-folder>... [options]

Options:
  --shuffle, -s     Randomize playlist order (re-shuffles on each repeat)
  --repeat, -r      Loop playlist (rescans sources for new files each cycle)
  --quality, -q     HQ resampler (higher CPU, inaudible difference)
  --eq, -e <name>   Start with EQ preset by name or JSON file path
  --fx <name>       Start with effects preset by name or JSON file path
  --crossfade, -x <secs>  Crossfade duration between tracks (0 = disabled)
  --rg-mode <mode>  ReplayGain mode: track (default), album, or off
  --device <name>   Select output device by name (substring match)
  --list-devices    List available output devices and exit
  --exclusive       Exclusive mode: per-track rate matching, device lock (macOS)
  --no-cover        Disable album cover display
```

Multiple files, folders, and M3U playlists can be passed as arguments. Duplicates are removed automatically. Running `keet` with no arguments resumes the last session.

## Dependencies

| Crate | Purpose |
|-------|---------|
| cpal 0.17 | Cross-platform audio I/O |
| symphonia 0.5 | Audio decoding (MP3, FLAC, WAV, OGG, AAC, ALAC, AIFF, isomp4) |
| rubato 1.0 | Sample rate conversion |
| crossterm 0.29 | Terminal UI |
| rtrb 0.3 | Lock-free ring buffer |
| realfft 3.4 | FFT for spectrum analysis |
| serde 1.0 | JSON deserialization for EQ/effects presets |
| souvlaki 0.8 | OS media transport controls (media keys, AirPods, Bluetooth) |
| ureq 3 | HTTP client for LRCLIB lyrics fetching (native-tls, no rustls bloat) |
| image 0.25 | Album cover decoding/resizing (jpeg/png/webp) |
| icy_sixel 0.5 | Sixel encoder for album covers on terminals without Kitty/iTerm2 graphics (the analysis spectrogram uses a built-in fixed-palette sixel encoder instead) |

## Platform Notes

- **macOS**: Automatic sample rate switching via CoreAudio; exclusive (hog) mode for bit-perfect playback with per-track rate matching; Bluetooth devices (AirPods etc.) detected and locked to native 48kHz; seamless device switching when audio output changes mid-playback; media keys via MPRemoteCommandCenter
- **Linux**: Works with PipeWire/PulseAudio/ALSA; falls back to device default rate if unsupported; media keys via MPRIS/D-Bus
- **Windows**: WASAPI shared mode with larger buffer (2048 samples) for lower CPU overhead; media keys via SMTC
- **WSL**: Auto-detected via `/proc/version`; uses larger buffer (2048 samples) to reduce crackling from PulseAudio virtualization

## Building

### Linux/WSL Dependencies

```bash
sudo apt install libasound2-dev libdbus-1-dev
```

- `libasound2-dev` -- ALSA headers (required by cpal)
- `libasound2-plugins` -- ALSA-to-PulseAudio plugin (automatic ALSA apps routing)
- `libdbus-1-dev` -- D-Bus headers (required by souvlaki for MPRIS media keys)

Create/edit ~/.asoundrc:

```bash
cat > ~/.asoundrc << 'EOF'
pcm.default pulse
ctl.default pulse
EOF
```

### Compile

```bash
cargo build --release
```

The binary is at `target/release/keet`. Copy to `/usr/local/bin/` for system-wide access.

Version is embedded automatically from git tags via `build.rs`.

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

The `.exe` automatically includes the app icon and version metadata (from git tags) when built on Windows.

## License

GPL-3.0
