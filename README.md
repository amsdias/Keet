# Keet

A high-performance, low-CPU terminal audio player with real-time spectrum visualization, an interactive parametric EQ, synced lyrics, and switchable themes.

## Features

- **Switchable themes**: Three UI themes — **Classic** (green-on-default), **Minimal** (warm-cyan editorial), **Hi-Fi** (amber studio-monitor) — cycled at runtime with `T`, with a launch default via `--theme` or `~/.config/keet/config.json`. Every screen (Player, Library, Lyrics, EQ+FX) is themed
- **Multi-format support**: MP3, FLAC, WAV, OGG, AAC/M4A, ALAC, AIFF — decoded by symphonia 0.6 with SIMD enabled
- **Any channel layout**: Mono, stereo, quad, 5.1, and 7.1 sources all play correctly — surround is downmixed to stereo (ITU-style: center at -3dB into both sides, LFE dropped)
- **Low CPU usage**: <0.5% total system CPU (release mode)
- **64-bit filter math**: EQ and crossfeed biquads run in f64 internally. f32 coefficients disintegrate at low centre frequencies — a 20 Hz Q=10 +12 dB band at 192 kHz lands at +2.4 dB in f32 and +12.0 dB in f64
- **Synced lyrics**: Embedded LRC lyrics + automatic fetching from LRCLIB (~3M songs), with adjustable sync offset
- **10-band parametric EQ**: A full-screen interactive editor (`E`) — every band has filter type (peak / shelves / cuts), frequency, Q, and gain, adjusted live; starts out as a classic ISO-centre graphic EQ; built-in preset shapes (Flat, Bass Boost, Treble Boost, Vocal, Loudness) + custom JSON presets, including AutoEq-style parametric headphone corrections
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
- **Headphone crossfeed**: Meier-style frequency-dependent crossfeed — three built-in presets plus custom JSON presets controlling level, corner frequency and interaural delay
- **Balance control**: Stereo balance with `[`/`]` keys (5% steps, -100 to +100)
- **Clipping indicator**: Persistent dot that turns red when signal exceeds 0dBFS, with peak safety limiter
- **Smart audio processing**: Automatic sample rate switching (macOS), Bluetooth detection, conditional resampling, seamless device switching
- **Volume control**: Adjustable 0-150% with per-sample gain
- **Library browser**: The library view (`L`) has a flat track list *and* an artist → album → track **tree** (`Tab` to switch) with live filtering (`/`), play-node (`Enter` plays a track / album / artist), and remove; folder sources auto-sort into artist → album order once tags load (when not shuffling)
- **Playlist features**: Shuffle (toggling off restores the previous order — M3U order survives), repeat (all/one), recursive folder scanning, playlist view with search/sort/track durations/album column, page navigation (Home/End/PgUp/PgDn + vim `g`/`G`/Ctrl+U/D), tag-based sort (artist → album → disc → track), play queue (enqueue tracks after current), M3U import/export, folder rescan, multiple source paths with deduplication
- **Resume playback**: Save and restore last session (track, position, volume, EQ — including an edited Custom parametric EQ, effects, crossfeed, balance, theme, device, exclusive) automatically
- **HQ resampler mode**: Optional `--quality` flag for audiophile-grade resampling
- **Resilient playback**: Skips missing/corrupt files with a status message, recovers from device disconnection (including USB DAC unplug)
- **Terminal-safe UI**: Output adapts to terminal width, handles terminal resize gracefully
- **Process stats**: Lightweight CPU/memory monitoring via direct platform syscalls (toggle with `I`)

## Installation

Keet builds from source with a stable Rust toolchain (install one from [rustup.rs](https://rustup.rs) if you don't have it).

```bash
git clone https://github.com/amsdias/Keet.git
cd Keet
cargo install --path .
```

That puts a release-optimized `keet` on your `PATH` (in `~/.cargo/bin`), so you can run it from anywhere:

```bash
keet ~/Music/
```

To update later, `git pull` and re-run `cargo install --path .`. To remove it, `cargo uninstall keet`.

**Linux/WSL** needs ALSA and D-Bus headers before building — see [Building](#building):

```bash
sudo apt install libasound2-dev libdbus-1-dev
```

Prefer not to install system-wide? `cargo build --release` leaves the binary at `target/release/keet`.

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

# Launch in the Hi-Fi theme (or set a permanent default in config.json)
cargo run --release -- ~/Music/ --theme hifi

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
| `T` | Cycle UI theme (Classic → Minimal → Hi-Fi) |
| `V` | Cycle visualization modes |
| `B` | Toggle visualization style (bars/dots) |
| `E` | Open the EQ + FX editor screen |
| `X` | Cycle effects presets |
| `Shift+R` | Cycle repeat mode (Off → All → One) |
| `Z` | Toggle shuffle |
| `R` | Rescan folders for changes |
| `S` | Save playlist as M3U |
| `O` | Open a new source (type a path) |
| `P` | Open a new source (native folder picker) |
| `F` | Toggle pre/post-fader metering |
| `C` | Cycle crossfeed presets (Off/Light/Medium/Strong + custom) |
| `I` | Toggle CPU/memory stats display |
| `[` | Balance left (5% steps) |
| `]` | Balance right (5% steps) |
| `+` / `=` | Volume up (5%) |
| `-` | Volume down (5%) |
| `Q` / `Esc` | Quit |

### Library View Controls

Press `L` to open the library, which replaces the visualization area with the track list. `Tab` switches between two presentations of the same library — a **flat list** and an **artist → album → track tree** — each keeping its own position.

**Flat list:**

| Key | Action |
|-----|--------|
| `Tab` | Switch to the tree |
| `Up` / `Down` | Move cursor |
| `Home` / `End` (or `g` / `G`) | Jump to top / bottom |
| `PgUp` / `PgDn` (or `Ctrl+U` / `Ctrl+D`) | Page up / down |
| `Enter` | Jump to selected track |
| `A` | Enqueue selected track (move it to play next) |
| `Shift+S` | Sort by tags (artist → album → disc → track → title → filename) |
| `/` | Search/filter by filename or tags |
| `D` / `Delete` | Remove selected track |
| `S` | Save playlist as M3U |
| `Esc` / `L` | Close library |

Album and track durations are shown in right-aligned columns. The cursor follows the currently playing track on transitions. Folder sources auto-sort into artist → album order once tags load (unless shuffling); `Shift+S` re-sorts on demand. Sorting turns shuffle off. Untagged tracks fall to the bottom.

**Tree** (`Tab`): the same tracks grouped by artist → album, independent of the play-queue order (so shuffle doesn't scramble it).

| Key | Action |
|-----|--------|
| `Tab` | Switch to the flat list |
| `Up` / `Down` | Move cursor |
| `←` / `→` | Collapse / expand the node |
| `Enter` | Play — a track plays itself; an album/artist plays from its first track |
| `/` | Filter (matches artist, album, or track names at any level; `Esc` clears) |
| `D` | Remove — a track immediately; an album/artist after a `[y/n]` confirm |
| `PgUp` / `PgDn` (or `Ctrl+U` / `Ctrl+D`) | Page up / down |
| `Esc` / `L` | Close library |

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

## Themes

Three UI themes, cycled at runtime with `T`:

| Theme | Look |
|-------|------|
| **Classic** | The original green-on-terminal-default look |
| **Minimal** | Editorial monochrome with a warm-cyan (`#9adcd0`) accent |
| **Hi-Fi** | Amber CRT studio-monitor — double-line header, segmented time box, VU panel, knob rack |

All four screens (Player, Library, Lyrics, EQ + FX) render in each theme. On truecolor terminals the palettes land exactly; 256-color terminals get the nearest colors.

Set a launch default in priority order — `--theme` flag → config file → last-session (resume) → Classic:

```bash
keet ~/Music --theme hifi          # this launch only
```

For a *persistent* default that applies on every launch (including with explicit paths), create `~/.config/keet/config.json` (`%APPDATA%\keet\config.json` on Windows). Every key is optional and applies on top of the resumed session (an explicit CLI flag still wins):

```json
{
  "theme": "minimal",
  "viz": "analysis",
  "rg_mode": "track",
  "eq": "Bass Boost",
  "crossfeed": "off"
}
```

| Key | Values |
|-----|--------|
| `theme` | `classic` \| `minimal` \| `hifi` |
| `viz` | `none` \| `vu` \| `spectrum` \| `spectrum-vertical` \| `oscilloscope` \| `lissajous` \| `spectrogram` \| `analysis` |
| `rg_mode` | `track` \| `album` \| `off` |
| `eq` | any preset name (built-in or custom) |
| `crossfeed` | `off` \| `light` \| `medium` \| `strong` \| any custom preset name |

## EQ + FX Editor

Press `E` to open the EQ + FX editor — a full-screen **10-band parametric EQ**, with the current effects / crossfeed / balance / ReplayGain shown beneath. Every band has four parameters:

| Parameter | Range | Keys |
|-----------|-------|------|
| Gain | ±12 dB — 0.5 dB per press, 0.1 dB with Shift | `↑` / `↓` |
| Filter type | Peak → Low Shelf → High Shelf → Low Cut → High Cut | `t` / `T` |
| Frequency | 20 Hz – 20 kHz, ⅓-octave steps | `<` / `>` |
| Q (bandwidth) | 0.3 – 10, √2 steps | `,` / `.` |

Plus: `←` / `→` select a band, `[` / `]` cycle presets, `0` reset the selected band to its graphic default, `E` / `L` / `Esc` close. All changes apply live, delayed by the audio buffer depth (a few seconds).

Bands start out as the classic graphic-EQ layout — peaking filters at the ISO octave centres (31 62 125 250 500 1k 2k 4k 8k 16k Hz) — so until you reach for the parametric keys it behaves exactly like a 10-band graphic EQ. The filters are standard RBJ-cookbook biquads. **Low Cut** (high-pass) and **High Cut** (low-pass) ignore gain: they filter by frequency and resonance (Q above 0.7 adds a resonant bump at the corner — a Low Cut at 20–30 Hz makes a clean subsonic/rumble filter). The response curve drawn in the editor and the player banner is the exact summed magnitude response of the active filters, so shelves and cuts render truthfully.

Editing any band switches the EQ to **Custom** (starting from the active preset); a Custom EQ persists across sessions, parametric settings included.

### Built-in Presets

10-band gain shapes:

| Preset | Description |
|--------|-------------|
| Flat | No EQ (passthrough) |
| Bass Boost | +6 dB at 31 Hz, tapering to +1 dB by 250 Hz |
| Treble Boost | +2 dB at 4 kHz rising to +5 dB at 16 kHz |
| Vocal | Slight bass cut, +3–4 dB through 1–4 kHz |
| Loudness | Boosts lows and highs (smiley curve) |

### Custom Presets

Drop JSON files into `~/.config/keet/eq/` (macOS/Linux) or `%APPDATA%\keet\eq\` (Windows). Custom presets appear alongside the built-ins when cycling with `[` / `]` in the editor. Two formats:

**Graphic** — one gain (dB) per band, at the 10 fixed ISO centres:

```json
{
  "name": "My Preset",
  "gains": [4.0, 3.0, 1.0, 0.0, 0.0, 0.0, 2.0, 3.0, 4.0, 2.0]
}
```

Short lists are zero-padded and long lists truncated to 10 bands; gains clamp to ±12 dB.

**Parametric** — up to 10 bands of `{type, freq, gain, q}`:

```json
{
  "name": "My Headphones",
  "preamp": -5.5,
  "bands": [
    { "type": "low_cut",    "freq": 20,   "q": 0.71 },
    { "type": "low_shelf",  "freq": 105,  "gain": 5.5,  "q": 0.71 },
    { "type": "peak",       "freq": 3300, "gain": -2.0, "q": 2.0 },
    { "type": "peak",       "freq": 5500, "gain": -4.5, "q": 4.0 },
    { "type": "high_shelf", "freq": 9500, "gain": 1.5,  "q": 0.71 }
  ]
}
```

Types: `peak`, `low_shelf`, `high_shelf`, `low_cut` (high-pass), `high_cut` (low-pass). `gain` defaults to 0 and `q` to 1.41 when omitted; unused slots stay flat. Frequency clamps to 20 Hz–20 kHz, gain to ±12 dB, Q to 0.3–10. When both `gains` and `bands` are present, `bands` wins. The optional `preamp` (dB, default 0, clamped ±12) is a flat gain applied ahead of the filters — set it negative to give boosted bands clipping headroom, exactly like AutoEq's preamp line.

This is the same shape as [AutoEq](https://github.com/jaakkopasanen/AutoEq) parametric exports — to correct your headphones, find their ParametricEQ profile in the AutoEq results and transcribe its rows (LSC → `low_shelf`, PK → `peak`, HSC → `high_shelf`, with the same Fc/gain/Q values) into a preset file. AutoEq's **FixedBandEQ** export matches too, even more directly: it targets the same 10 ISO centres at Q 1.41, so its ten gain values drop straight into the `gains` form. (The GraphicEQ/Wavelet export is a 127-point drawn curve — that one can't be represented.) Copy the AutoEq profile's preamp line into the `preamp` field so boosted bands can't clip. Pairs well with the crossfeed filter.

Example presets are included in `assets/` -- copy them to the presets folders as a starting point:

```bash
# macOS/Linux
mkdir -p ~/.config/keet/eq ~/.config/keet/effects ~/.config/keet/crossfeed
cp assets/eq-example.json assets/eq-parametric-example.json ~/.config/keet/eq/
cp assets/fx-example.json ~/.config/keet/effects/
cp assets/crossfeed-example.json ~/.config/keet/crossfeed/
cp assets/config-example.json ~/.config/keet/config.json   # default theme etc.

# Windows
copy assets\eq-example.json %APPDATA%\keet\eq\
copy assets\eq-parametric-example.json %APPDATA%\keet\eq\
copy assets\fx-example.json %APPDATA%\keet\effects\
copy assets\crossfeed-example.json %APPDATA%\keet\crossfeed\
copy assets\config-example.json %APPDATA%\keet\config.json
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

## Headphone Crossfeed

Press `C` to cycle crossfeed presets. Crossfeed blends a low-passed, slightly delayed copy of each channel into the other, so hard-panned material stops sounding like it's inside your head — a Meier-style approximation of listening to speakers in a room.

| Preset | Level | Corner | ITD |
|--------|-------|--------|-----|
| Off | — | — | — |
| Light | −6 dB | 700 Hz | 300 µs |
| Medium | −4.5 dB | 700 Hz | 300 µs |
| Strong | −3 dB | 700 Hz | 300 µs |

### Custom Presets

Drop JSON files into `~/.config/keet/crossfeed/` (macOS/Linux) or `%APPDATA%\keet\crossfeed\` (Windows) to control all three parameters:

```json
{
  "name": "Wide Soft",
  "level_db": -5.0,
  "cutoff_hz": 900.0,
  "delay_us": 350.0
}
```

- `level_db` — how loud the crossfed channel is. Lower is subtler; around −3 dB is a strong blend.
- `cutoff_hz` *(optional, default 700)* — the corner of the crossfeed low-pass. Higher values cross more of the midrange over and make the effect more aggressive.
- `delay_us` *(optional, default 300)* — the interaural time difference, i.e. how long sound takes to reach the far ear. Roughly 250–400 µs is the natural range.

Custom presets appear alongside the built-ins when cycling with `C`.

## Crossfade

Use `--crossfade <seconds>` (or `-x`) to enable smooth crossfade between tracks:

```bash
cargo run --release -- ~/Music/ --crossfade 3
```

Uses an equal-power crossfade curve for natural-sounding transitions. The previous track's tail is captured and mixed into the next track's beginning.

## Network Libraries (NAS)

Keet has no UPnP/DLNA client, and doesn't need one: point it at a mounted network share and everything works exactly as it does locally — the library tree, seeking, the metadata scan, gapless, ReplayGain, and the full DSP chain. To Keet a mounted share is just a folder of files.

```bash
# macOS — Finder mounts appear under /Volumes
keet /Volumes/nas/Music

# Linux — mount an SMB share, then play it
sudo mount -t cifs //nas.local/Music /mnt/music -o username=you,uid=$UID
keet /mnt/music

# Linux — NFS
sudo mount -t nfs nas.local:/volume1/Music /mnt/music
keet /mnt/music

# Windows — map the drive, or use the UNC path directly
keet \\nas\Music
```

Add the mount to `/etc/fstab` (or macOS login items) to make it persistent, then resume works across reboots like any local folder.

**Performance note:** the metadata scan reads tags from every file, so the first scan of a large library over a slow link takes a while — it runs in the background on up to 4 threads and the UI stays responsive. Playback itself is undemanding: the decode thread stays ~4 seconds ahead, so ordinary Wi-Fi is comfortably fast enough even for hi-res FLAC.

## Visualization Modes

Press `V` to cycle through:

1. **None** - Minimal UI, lower CPU
2. **VU Meter** - Stereo level meters with peak hold dots
3. **Spectrum Horizontal** - Stereo butterfly display (L channel up, R channel down)
4. **Spectrum Vertical** - 31-band analyzer with peak dots and height-based color gradient (green -> yellow -> red)
5. **Oscilloscope** - Mono waveform; `Bars` style uses quadrant blocks for 2x sub-cell resolution
6. **Lissajous** - Stereo vector scope (L vs R); detects mono/stereo/anti-phase imbalance
7. **Spectrogram** - Scrolling time-frequency heatmap, dense colormap palette
8. **Analysis Spectrogram** - High-resolution time × frequency spectrogram with a magma colormap, rendered as a true pixel image — fine enough to reveal detail like images encoded into audio. On Kitty-protocol terminals (Kitty, Ghostty, WezTerm) it uses Kitty graphics with in-place image replacement; on Windows Terminal it uses a custom fixed-palette Sixel encoder that only re-transmits when the image changes, so scrolling stays smooth and shimmer-free even through ConPTY. Falls back to half-block truecolor elsewhere (iTerm2, WezTerm on Windows, plain terminals). The block auto-sizes to fit the window height. Linear frequency axis by default (`B` toggles linear/log); scroll cadence is matched to the sample rate for smooth motion. A frequency legend runs down the left edge, labelled to suit the axis: octave anchors (C1, C2, … — log spacing puts octaves at even intervals, so pitch can be read straight off the image) on the log axis, and Hz landmarks on the linear one. It is dropped below 60 columns so narrow windows keep their width for the image.

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
├── theme.rs       Theme palettes (Classic/Minimal/Hi-Fi), theme resolution
├── config.rs      User preferences from config.json (default theme, …)
├── audio.rs       Audio stream, sample rate switching, CoreAudio FFI
├── decode.rs      Continuous decoder thread, gapless playback, ReplayGain, resampling
├── eq.rs          10-band parametric EQ (RBJ biquads: peak/shelf/cut), exact response curve, JSON presets
├── eq_ui.rs       EQ + FX editor screen renderer (shared across themes)
├── effects.rs     Reverb, chorus, delay effects with preset loading
├── playlist.rs    Playlist builder, M3U parser, shuffle
├── library.rs     Artist→album→track tree (build/filter/navigation) + shared renderer
├── crossfeed.rs   Meier-style headphone crossfeed (level/cutoff/ITD, JSON presets)
├── metadata.rs    Tag reading (artist, title, album, track #, lyrics, ReplayGain), background scan
├── lyrics.rs      LRC parser, LRCLIB API client, synced/plain lyrics state
├── cover.rs       Album cover decoding, Kitty/iTerm2/Sixel/half-block rendering
├── resume.rs      Resume state persistence (save/restore sessions)
├── viz.rs         VizAnalyser, StatsMonitor, spectrum/oscilloscope/lissajous/spectrogram rendering
├── media_keys.rs  OS media transport controls (souvlaki)
├── ui.rs          Terminal UI, keyboard input, the Classic renderer + dispatch
├── ui_minimal.rs  Minimal theme renderers (Player/Library/Lyrics)
└── ui_hifi.rs     Hi-Fi theme renderers (Player/Library/Lyrics)
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
  --theme <name>    UI theme: classic (default), minimal, hifi
```

Multiple files, folders, and M3U playlists can be passed as arguments. Duplicates are removed automatically. Running `keet` with no arguments resumes the last session.

## Dependencies

| Crate | Purpose |
|-------|---------|
| cpal 0.18 | Cross-platform audio I/O (native PipeWire + PulseAudio hosts on Linux) |
| symphonia 0.6 | Audio decoding (MP3, FLAC, WAV, OGG, AAC, ALAC, AIFF, isomp4), SIMD by default |
| rubato 4.0 | Sample rate conversion |
| crossterm 0.29 | Terminal UI |
| rtrb 0.3 | Lock-free ring buffer |
| realfft 3.4 | FFT for spectrum analysis |
| serde 1.0 | JSON deserialization for EQ/effects/crossfeed presets |
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
cargo build --release          # binary at target/release/keet
cargo install --path .         # or install it onto your PATH
```

Release mode is not optional — a debug build cannot keep the decode thread ahead of the audio callback and will glitch.

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
