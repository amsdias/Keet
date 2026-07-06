use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, AtomicU8, AtomicU32, AtomicU64, AtomicI32, AtomicI64, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;
use std::path::PathBuf;

pub const SUPPORTED_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "aac", "m4a", "mp4", "m4b", "aiff", "aif"];
pub const VIZ_BUFFER_SIZE: usize = 8192; // Small buffer for viz tap from audio callback

/// Ring buffer capacity for a given output rate: ~4 seconds of stereo f32.
/// Sized per-stream rather than worst-case — 48 kHz output => 1.5 MB, 192 kHz => 6 MB.
#[inline]
pub const fn ring_capacity_for(output_rate: u32) -> usize {
    output_rate as usize * 2 * 4
}

// Visualization constants
pub const FFT_SIZE: usize = 4096;
pub const SPECTRUM_BANDS: usize = 31;

// Analysis-spectrogram scroll cadence. Columns are produced at the FFT hop rate
// (output_rate / FFT_SIZE/2), which is ~21/s at 44.1k but ~94/s at 192k — far
// faster than a terminal can smoothly re-draw a full image. To keep scrolling
// even, we aggregate several hops into one column so the column rate stays ~one
// per target frame at any sample rate, and the render loop ticks at the same
// period. Both the aggregation and the render cadence derive from this constant.
pub const SPECTRO_TARGET_FRAME_MS: f32 = 45.0; // ~22 columns/frames per second

/// Number of FFT hops aggregated into one analysis-spectrogram column at the
/// given output rate, so the column-production rate is ~constant across rates.
pub fn spectro_hops_per_col(output_rate: u64) -> usize {
    if output_rate == 0 { return 1; }
    let hop_ms = (FFT_SIZE as f32 / 2.0) * 1000.0 / output_rate as f32;
    ((SPECTRO_TARGET_FRAME_MS / hop_ms).round() as usize).max(1)
}

/// Render/column period (ms) for the analysis spectrogram at the given output
/// rate. Equals the time to produce one aggregated column, so exactly one column
/// lands per frame and the scroll advances evenly.
pub fn spectro_frame_ms(output_rate: u64) -> u64 {
    if output_rate == 0 { return SPECTRO_TARGET_FRAME_MS as u64; }
    let hop_ms = (FFT_SIZE as f32 / 2.0) * 1000.0 / output_rate as f32;
    (spectro_hops_per_col(output_rate) as f32 * hop_ms).round().max(1.0) as u64
}
// Display smoothing for spectrum bars, applied asymmetrically (fast attack / slow
// release): VIZ_ATTACK governs the rise so beats land on time, VIZ_DECAY the fall
// so it stays smooth. A symmetric low-pass here delayed the onset ~150 ms.
pub const VIZ_ATTACK: f32 = 0.3; // Rise smoothing (lower = snappier, more on-beat)
pub const VIZ_DECAY: f32 = 0.70; // Fall smoothing (higher = smoother decay)

// Spectrum bars fall proportionally so tall bars don't crawl down and look
// sluggish during loud passages: BAR_DECAY is the fraction of height kept per
// frame, GRAVITY a small linear floor so a band still settles fully to silence.
pub const BAR_DECAY: f32 = 0.85;    // Proportional fall: lower = faster decay
pub const GRAVITY: f32 = 0.012;     // Linear floor added to the proportional fall
pub const DOT_GRAVITY: f32 = 0.025; // Slower fall for the peak dots
pub const ATTACK: f32 = 0.7;       // Snappiness of the rise
pub const HOLD_TIME: u8 = 10;      // Frames for the dot to "hang"

// ANSI color codes
pub const C_RESET: &str = "\x1B[0m";
pub const C_BOLD: &str = "\x1B[1m";
pub const C_DIM: &str = "\x1B[2m";
pub const C_CYAN: &str = "\x1B[36m";
pub const C_GREEN: &str = "\x1B[32m";
pub const C_YELLOW: &str = "\x1B[33m";
pub const C_MAGENTA: &str = "\x1B[35m";
pub const C_RED: &str = "\x1B[31m";

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RepeatMode {
    Off = 0,
    All = 1,
    One = 2,
}

impl RepeatMode {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => RepeatMode::All,
            2 => RepeatMode::One,
            _ => RepeatMode::Off,
        }
    }

    pub fn next(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            RepeatMode::Off => "",
            RepeatMode::All => " | repeat",
            RepeatMode::One => " | repeat-1",
        }
    }
}

// Visualization style
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VizStyle {
    Bars = 0,
    Dots = 1,
}

impl VizStyle {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => VizStyle::Dots,
            _ => VizStyle::Bars,
        }
    }

}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RgMode {
    Track = 0,
    Album = 1,
    Off = 2,
}

impl RgMode {
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => RgMode::Album,
            2 => RgMode::Off,
            _ => RgMode::Track,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            RgMode::Track => "Track",
            RgMode::Album => "Album",
            RgMode::Off => "Off",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "track" => Some(RgMode::Track),
            "album" => Some(RgMode::Album),
            "off" => Some(RgMode::Off),
            _ => None,
        }
    }
}

// Visualization modes
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VizMode {
    None = 0,
    VuMeter = 1,
    SpectrumHorizontal = 2,
    SpectrumVertical = 3,
    Oscilloscope = 4,
    Lissajous = 5,
    Spectrogram = 6,
    SpectrogramAnalysis = 7,
}

impl VizMode {
    pub fn next(self) -> Self {
        match self {
            VizMode::None => VizMode::VuMeter,
            VizMode::VuMeter => VizMode::SpectrumHorizontal,
            VizMode::SpectrumHorizontal => VizMode::SpectrumVertical,
            VizMode::SpectrumVertical => VizMode::Oscilloscope,
            VizMode::Oscilloscope => VizMode::Lissajous,
            VizMode::Lissajous => VizMode::Spectrogram,
            VizMode::Spectrogram => VizMode::SpectrogramAnalysis,
            VizMode::SpectrogramAnalysis => VizMode::None,
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => VizMode::VuMeter,
            2 => VizMode::SpectrumHorizontal,
            3 => VizMode::SpectrumVertical,
            4 => VizMode::Oscilloscope,
            5 => VizMode::Lissajous,
            6 => VizMode::Spectrogram,
            7 => VizMode::SpectrogramAnalysis,
            _ => VizMode::None,
        }
    }

    /// Parse a config/default viz name. Accepts a few friendly aliases.
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" | "off" => Some(VizMode::None),
            "vu" | "vumeter" | "vu-meter" => Some(VizMode::VuMeter),
            "spectrum" | "spectrum-horizontal" => Some(VizMode::SpectrumHorizontal),
            "spectrum-vertical" | "vertical" => Some(VizMode::SpectrumVertical),
            "oscilloscope" | "scope" => Some(VizMode::Oscilloscope),
            "lissajous" | "vector" => Some(VizMode::Lissajous),
            "spectrogram" => Some(VizMode::Spectrogram),
            "analysis" | "analysis-spectrogram" => Some(VizMode::SpectrogramAnalysis),
            _ => None,
        }
    }

}

pub struct PlayerState {
    // Control flags
    pub(crate) paused: AtomicBool,
    pub(crate) quit: AtomicBool,
    pub(crate) skip_next: AtomicBool,
    pub(crate) skip_prev: AtomicBool,
    pub(crate) seek_request: AtomicI64,
    pub(crate) jump_to_track: AtomicI64,

    // Track info
    pub(crate) current_track: AtomicUsize,
    pub(crate) total_tracks: AtomicUsize,
    pub(crate) sample_rate: AtomicU64,      // Source file sample rate
    pub(crate) output_rate: AtomicU64,      // Output stream sample rate
    pub(crate) total_samples: AtomicU64,    // Total samples in source file
    pub(crate) samples_played: AtomicU64,   // Samples played (at output rate)
    pub(crate) channels: AtomicUsize,
    pub(crate) bits_per_sample: AtomicUsize,

    // Producer status
    pub(crate) producer_done: AtomicBool,
    pub(crate) track_info_ready: AtomicBool,

    // Buffer level (updated by producer, read by UI)
    pub(crate) buffer_level: AtomicUsize,

    // Ring buffer capacity in samples — sized per output rate, so decode.rs and UI
    // can compute fill percentage / drain thresholds without a hard-coded constant.
    pub(crate) ring_capacity: AtomicUsize,

    // Signal consumer to drain the ring buffer (for seek/skip).
    // Triggers an immediate full drain in the audio callback.
    pub(crate) reset_consumer_counter: AtomicBool,

    // Visualization state
    pub(crate) viz_mode: AtomicU8,
    pub(crate) peak_left: AtomicU32,
    pub(crate) peak_right: AtomicU32,
    pub(crate) spectrum: [AtomicU32; SPECTRUM_BANDS],   // L channel (or mono for vertical)
    pub(crate) spectrum_r: [AtomicU32; SPECTRUM_BANDS], // R channel
    pub(crate) peak_dots: [AtomicU32; SPECTRUM_BANDS],

    pub(crate) vu_peak_dot_l: AtomicU32,
    pub(crate) vu_peak_dot_r: AtomicU32,

    // Volume (0-150, stored as percentage, 100 = unity gain)
    pub(crate) volume: AtomicU32,

    // EQ preset index and count
    pub(crate) eq_preset_index: AtomicUsize,
    pub(crate) eq_preset_count: AtomicUsize,
    pub(crate) eq_changed: AtomicBool,
    // Live 10-band graphic-EQ gains (dB, f32-as-bits) — the producer's source of
    // truth. `eq_custom` = the gains were edited away from the named preset.
    pub(crate) eq_gains: [AtomicU32; crate::eq::EQ_BANDS],
    pub(crate) eq_custom: AtomicBool,

    // Effects preset index and count
    pub(crate) effects_preset_index: AtomicUsize,
    pub(crate) effects_preset_count: AtomicUsize,
    pub(crate) effects_changed: AtomicBool,

    // Pre/post-fader metering (false = post-fader, true = pre-fader)
    pub(crate) pre_fader: AtomicBool,

    // Show CPU/memory stats in status line
    pub(crate) show_stats: AtomicBool,

    // Crossfade duration in seconds (0 = disabled)
    pub(crate) crossfade_secs: AtomicU32,

    // Visualization style (bars vs dots)
    pub(crate) viz_style: AtomicU8,

    // Decode error from producer thread (None = no error)
    pub(crate) decode_error: Mutex<Option<String>>,

    // Track transition signaling (gapless playback)
    pub(crate) track_transition_count: AtomicUsize,
    pub(crate) producer_track_index: AtomicUsize,

    // ReplayGain mode
    pub(crate) rg_mode: AtomicU8,

    // Clipping indicator
    pub(crate) clipping: AtomicBool,

    // Crossfeed preset index and count
    pub(crate) crossfeed_preset_index: AtomicUsize,
    pub(crate) crossfeed_preset_count: AtomicUsize,
    pub(crate) crossfeed_changed: AtomicBool,

    // Stereo balance (-100 to +100, 0 = center)
    pub(crate) balance: AtomicI32,

    // Exclusive mode
    pub(crate) exclusive: AtomicBool,
    pub(crate) rate_change_needed: AtomicBool,
    pub(crate) next_track_rate: AtomicU32,

    // Stream error (device disconnected etc.)
    pub(crate) stream_error: AtomicBool,

    // Repeat mode (Off/All/One) — readable by producer for repeat-one
    pub(crate) repeat_mode: AtomicU8,

    // Active UI theme (ThemeKind as u8). UI-thread only; cheap to read each
    // frame. Cycled at runtime via the T key.
    pub(crate) theme: AtomicU8,
}

impl PlayerState {
    pub fn new() -> Self {
        Self {
            paused: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            skip_next: AtomicBool::new(false),
            skip_prev: AtomicBool::new(false),
            seek_request: AtomicI64::new(0),
            jump_to_track: AtomicI64::new(-1),
            current_track: AtomicUsize::new(0),
            total_tracks: AtomicUsize::new(0),
            sample_rate: AtomicU64::new(44100),
            output_rate: AtomicU64::new(44100),
            total_samples: AtomicU64::new(0),
            samples_played: AtomicU64::new(0),
            channels: AtomicUsize::new(2),
            bits_per_sample: AtomicUsize::new(16),
            producer_done: AtomicBool::new(false),
            track_info_ready: AtomicBool::new(false),
            buffer_level: AtomicUsize::new(0),
            ring_capacity: AtomicUsize::new(ring_capacity_for(48_000)),
            reset_consumer_counter: AtomicBool::new(false),
            viz_mode: AtomicU8::new(VizMode::None as u8),
            peak_left: AtomicU32::new(0),
            peak_right: AtomicU32::new(0),
            spectrum: std::array::from_fn(|_| AtomicU32::new(0)),
            spectrum_r: std::array::from_fn(|_| AtomicU32::new(0)),
            peak_dots: std::array::from_fn(|_| AtomicU32::new(0)),
            vu_peak_dot_l: AtomicU32::new(0),
            vu_peak_dot_r: AtomicU32::new(0),
            volume: AtomicU32::new(100),
            eq_preset_index: AtomicUsize::new(0),
            eq_preset_count: AtomicUsize::new(0),
            eq_changed: AtomicBool::new(false),
            eq_gains: std::array::from_fn(|_| AtomicU32::new(0)),
            eq_custom: AtomicBool::new(false),
            effects_preset_index: AtomicUsize::new(0),
            effects_preset_count: AtomicUsize::new(0),
            effects_changed: AtomicBool::new(false),
            pre_fader: AtomicBool::new(false),
            show_stats: AtomicBool::new(false),
            crossfade_secs: AtomicU32::new(0),
            viz_style: AtomicU8::new(VizStyle::Dots as u8),
            decode_error: Mutex::new(None),
            track_transition_count: AtomicUsize::new(0),
            producer_track_index: AtomicUsize::new(0),
            rg_mode: AtomicU8::new(RgMode::Track as u8),
            clipping: AtomicBool::new(false),
            crossfeed_preset_index: AtomicUsize::new(0),
            crossfeed_preset_count: AtomicUsize::new(0),
            crossfeed_changed: AtomicBool::new(false),
            balance: AtomicI32::new(0),
            exclusive: AtomicBool::new(false),
            rate_change_needed: AtomicBool::new(false),
            next_track_rate: AtomicU32::new(0),
            stream_error: AtomicBool::new(false),
            repeat_mode: AtomicU8::new(RepeatMode::Off as u8),
            theme: AtomicU8::new(crate::theme::ThemeKind::Classic as u8),
        }
    }

    pub fn theme_kind(&self) -> crate::theme::ThemeKind {
        crate::theme::ThemeKind::from_u8(self.theme.load(Ordering::Relaxed))
    }

    pub fn set_theme(&self, kind: crate::theme::ThemeKind) {
        self.theme.store(kind as u8, Ordering::Relaxed);
        // HiFi's static VU panel needs live peak data to be useful — without
        // a viz mode driving the analyser, the bars sit flat at -∞. Default
        // viz on when entering HiFi, but only if the user hasn't already
        // picked a mode.
        if kind == crate::theme::ThemeKind::HiFi && self.viz_mode() == VizMode::None {
            self.viz_mode.store(VizMode::VuMeter as u8, Ordering::Relaxed);
        }
    }

    pub fn cycle_theme(&self) -> crate::theme::ThemeKind {
        let next = self.theme_kind().next();
        self.set_theme(next);
        next
    }

    pub fn repeat_mode(&self) -> RepeatMode {
        RepeatMode::from_u8(self.repeat_mode.load(Ordering::Relaxed))
    }

    pub fn toggle_pause(&self) { self.paused.fetch_xor(true, Ordering::Relaxed); }
    pub fn is_paused(&self) -> bool { self.paused.load(Ordering::Relaxed) }
    pub fn quit(&self) { self.quit.store(true, Ordering::Relaxed); }
    pub fn should_quit(&self) -> bool { self.quit.load(Ordering::Relaxed) }
    pub fn next(&self) { self.skip_next.store(true, Ordering::Relaxed); }
    pub fn prev(&self) { self.skip_prev.store(true, Ordering::Relaxed); }
    pub fn jump_to(&self, index: usize) {
        self.jump_to_track.store(index as i64, Ordering::Relaxed);
    }

    pub fn take_jump(&self) -> Option<usize> {
        let val = self.jump_to_track.swap(-1, Ordering::Relaxed);
        if val >= 0 { Some(val as usize) } else { None }
    }
    pub fn take_skip_next(&self) -> bool { self.skip_next.swap(false, Ordering::Relaxed) }
    pub fn take_skip_prev(&self) -> bool { self.skip_prev.swap(false, Ordering::Relaxed) }
    pub fn seek(&self, secs: i64) { self.seek_request.store(secs, Ordering::Relaxed); }
    pub fn take_seek(&self) -> i64 { self.seek_request.swap(0, Ordering::Relaxed) }

    pub fn volume_up(&self) {
        let cur = self.volume.load(Ordering::Relaxed);
        self.volume.store((cur + 5).min(150), Ordering::Relaxed);
    }
    pub fn volume_down(&self) {
        let cur = self.volume.load(Ordering::Relaxed);
        self.volume.store(cur.saturating_sub(5), Ordering::Relaxed);
    }
    pub fn volume_gain(&self) -> f32 {
        self.volume.load(Ordering::Relaxed) as f32 / 100.0
    }

    pub fn time_secs(&self) -> f64 {
        let s = self.samples_played.load(Ordering::Relaxed) as f64;
        let r = self.output_rate.load(Ordering::Relaxed) as f64;
        if r > 0.0 { s / r } else { 0.0 }
    }

    pub fn total_secs(&self) -> f64 {
        let s = self.total_samples.load(Ordering::Relaxed) as f64;
        let r = self.sample_rate.load(Ordering::Relaxed) as f64;
        if r > 0.0 { s / r } else { 0.0 }
    }

    pub fn viz_mode(&self) -> VizMode {
        VizMode::from_u8(self.viz_mode.load(Ordering::Relaxed))
    }

    pub fn cycle_viz_mode(&self) {
        let current = self.viz_mode();
        self.viz_mode.store(current.next() as u8, Ordering::Relaxed);
    }

    /// Move the selected preset by `dir` (+1/-1), wrapping. Selecting a preset
    /// drops out of Custom (its gains become the live EQ again).
    pub fn step_eq_preset(&self, dir: i32) {
        let count = self.eq_preset_count.load(Ordering::Relaxed);
        if count == 0 { return; }
        let cur = self.eq_preset_index.load(Ordering::Relaxed) as i32;
        let next = (cur + dir).rem_euclid(count as i32) as usize;
        self.eq_preset_index.store(next, Ordering::Relaxed);
        self.eq_custom.store(false, Ordering::Relaxed);
        self.eq_changed.store(true, Ordering::Relaxed);
    }

    pub fn eq_index(&self) -> usize {
        self.eq_preset_index.load(Ordering::Relaxed)
    }

    pub fn take_eq_changed(&self) -> bool {
        self.eq_changed.swap(false, Ordering::Relaxed)
    }

    // --- Live graphic-EQ gains ---

    pub fn eq_gains_array(&self) -> [f32; crate::eq::EQ_BANDS] {
        std::array::from_fn(|i| f32::from_bits(self.eq_gains[i].load(Ordering::Relaxed)))
    }

    pub fn set_eq_gains(&self, gains: &[f32; crate::eq::EQ_BANDS]) {
        for (i, &g) in gains.iter().enumerate() {
            self.eq_gains[i].store(g.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn is_eq_custom(&self) -> bool {
        self.eq_custom.load(Ordering::Relaxed)
    }

    /// Nudge one band's gain by `delta` dB (clamped ±limit), mark the EQ Custom,
    /// and signal the producer. Returns the new gain.
    pub fn nudge_eq_gain(&self, band: usize, delta: f32) -> f32 {
        if band >= crate::eq::EQ_BANDS {
            return 0.0;
        }
        let cur = f32::from_bits(self.eq_gains[band].load(Ordering::Relaxed));
        let next = (cur + delta).clamp(-crate::eq::EQ_GAIN_LIMIT, crate::eq::EQ_GAIN_LIMIT);
        self.eq_gains[band].store(next.to_bits(), Ordering::Relaxed);
        self.eq_custom.store(true, Ordering::Relaxed);
        self.eq_changed.store(true, Ordering::Relaxed);
        next
    }

    pub fn cycle_effects(&self) {
        let count = self.effects_preset_count.load(Ordering::Relaxed);
        if count == 0 { return; }
        let cur = self.effects_preset_index.load(Ordering::Relaxed);
        self.effects_preset_index.store((cur + 1) % count, Ordering::Relaxed);
        self.effects_changed.store(true, Ordering::Relaxed);
    }

    pub fn effects_index(&self) -> usize {
        self.effects_preset_index.load(Ordering::Relaxed)
    }

    pub fn take_effects_changed(&self) -> bool {
        self.effects_changed.swap(false, Ordering::Relaxed)
    }

    pub fn toggle_pre_fader(&self) {
        self.pre_fader.fetch_xor(true, Ordering::Relaxed);
    }

    pub fn is_pre_fader(&self) -> bool {
        self.pre_fader.load(Ordering::Relaxed)
    }

    pub fn toggle_stats(&self) {
        self.show_stats.fetch_xor(true, Ordering::Relaxed);
    }

    pub fn show_stats(&self) -> bool {
        self.show_stats.load(Ordering::Relaxed)
    }

    pub fn viz_style(&self) -> VizStyle {
        VizStyle::from_u8(self.viz_style.load(Ordering::Relaxed))
    }

    pub fn toggle_viz_style(&self) {
        let cur = self.viz_style.load(Ordering::Relaxed);
        self.viz_style.store(if cur == 0 { 1 } else { 0 }, Ordering::Relaxed);
    }

    pub fn signal_next_track(&self, index: usize) {
        self.producer_track_index.store(index, Ordering::Relaxed);
        self.track_transition_count.fetch_add(1, Ordering::Release);
    }

    pub fn rg_mode(&self) -> RgMode {
        RgMode::from_u8(self.rg_mode.load(Ordering::Relaxed))
    }

    pub fn is_clipping(&self) -> bool {
        self.clipping.swap(false, Ordering::Relaxed)
    }

    pub fn cycle_crossfeed(&self) {
        let count = self.crossfeed_preset_count.load(Ordering::Relaxed);
        if count == 0 { return; }
        let cur = self.crossfeed_preset_index.load(Ordering::Relaxed);
        self.crossfeed_preset_index.store((cur + 1) % count, Ordering::Relaxed);
        self.crossfeed_changed.store(true, Ordering::Relaxed);
    }

    pub fn crossfeed_index(&self) -> usize {
        self.crossfeed_preset_index.load(Ordering::Relaxed)
    }

    pub fn take_crossfeed_changed(&self) -> bool {
        self.crossfeed_changed.swap(false, Ordering::Relaxed)
    }

    pub fn balance_left(&self) {
        let cur = self.balance.load(Ordering::Relaxed);
        self.balance.store((cur - 5).max(-100), Ordering::Relaxed);
    }

    pub fn balance_right(&self) {
        let cur = self.balance.load(Ordering::Relaxed);
        self.balance.store((cur + 5).min(100), Ordering::Relaxed);
    }

    pub fn balance_value(&self) -> i32 {
        self.balance.load(Ordering::Relaxed)
    }

    pub fn set_peaks(&self, left: f32, right: f32) {
        self.peak_left.store(left.to_bits(), Ordering::Relaxed);
        self.peak_right.store(right.to_bits(), Ordering::Relaxed);
    }

    pub fn get_peaks(&self) -> (f32, f32) {
        let left = f32::from_bits(self.peak_left.load(Ordering::Relaxed));
        let right = f32::from_bits(self.peak_right.load(Ordering::Relaxed));
        (left, right)
    }

    pub fn set_spectrum(&self, bands: &[f32; SPECTRUM_BANDS]) {
        for (i, &val) in bands.iter().enumerate() {
            self.spectrum[i].store(val.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn get_spectrum(&self) -> [f32; SPECTRUM_BANDS] {
        std::array::from_fn(|i| f32::from_bits(self.spectrum[i].load(Ordering::Relaxed)) )
    }

    pub fn set_spectrum_r(&self, bands: &[f32; SPECTRUM_BANDS]) {
        for (i, &val) in bands.iter().enumerate() {
            self.spectrum_r[i].store(val.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn get_spectrum_r(&self) -> [f32; SPECTRUM_BANDS] {
        std::array::from_fn(|i| f32::from_bits(self.spectrum_r[i].load(Ordering::Relaxed)))
    }

    pub fn set_dots(&self, dots: &[f32; SPECTRUM_BANDS]) {
        for (i, &val) in dots.iter().enumerate() {
            self.peak_dots[i].store(val.to_bits(), Ordering::Relaxed);
        }
    }

    pub fn get_dots(&self) -> [f32; SPECTRUM_BANDS] {
        std::array::from_fn(|i| f32::from_bits(self.peak_dots[i].load(Ordering::Relaxed)))
    }

    pub fn set_vu_dots(&self, left: f32, right: f32) {
        self.vu_peak_dot_l.store(left.to_bits(), Ordering::Relaxed);
        self.vu_peak_dot_r.store(right.to_bits(), Ordering::Relaxed);
    }

    pub fn get_vu_dots(&self) -> (f32, f32) {
        let left = f32::from_bits(self.vu_peak_dot_l.load(Ordering::Relaxed));
        let right = f32::from_bits(self.vu_peak_dot_r.load(Ordering::Relaxed));
        (left, right)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Player,
    Playlist,
    Lyrics,
    Eq,
}

#[derive(Clone, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Search(String),
    SavePlaylist(String),
}

pub struct UiState {
    pub view_mode: ViewMode,
    pub input_mode: InputMode,
    pub scroll_offset: usize,
    pub cursor: usize,
    pub filtered_indices: Vec<usize>,
    pub current: usize,
    pub source_paths: Vec<PathBuf>,
    pub shuffle: bool,
    /// Playlist order snapshot taken when shuffle is toggled ON, so toggling it
    /// OFF can restore the real prior order (an M3U's curated order isn't
    /// reconstructible by sorting).
    pub pre_shuffle_order: Option<Vec<PathBuf>>,
    pub repeat_mode: RepeatMode,
    pub enqueue_count: usize,
    pub status_message: Option<(String, Instant)>,
    pub metadata_cache: std::sync::Arc<crate::metadata::MetadataCache>,
    pub scan_handle: Option<JoinHandle<()>>,
    /// One-shot flag: when the background scan finishes and we're not shuffling,
    /// auto-sort a folder-sourced playlist into artist→album order. Armed at
    /// each folder rebuild (startup / rescan / source-switch); the sort can't
    /// run at build time because tags load asynchronously.
    pub auto_sort_pending: bool,
    /// Library view presentation: false = flat list, true = artist→album tree.
    pub library_tree_mode: bool,
    /// The artist→album→track tree (a projection of the playlist by tags) and its
    /// own navigation state, independent of the flat list's cursor/scroll so the
    /// two presentations don't fight (esp. when the play queue is shuffled).
    pub library_tree: crate::library::LibraryTree,
    pub tree_fold: crate::library::FoldState,
    pub tree_cursor: usize,
    pub tree_scroll: usize,
    /// Last rendered tree body height, so PageUp/Down can step by a viewport.
    pub tree_view_height: usize,
    /// The tree needs rebuilding (playlist changed, or tags still loading).
    pub tree_dirty: bool,
    /// Selected band (0..EQ_BANDS) in the EQ editor view.
    pub eq_band: usize,
    /// Active tree filter (from `/`). Empty = show the full fold-based tree.
    pub tree_filter: String,
    /// A pending bulk remove awaiting `[y/n]` confirmation: (label, playlist indices).
    pub tree_pending_remove: Option<(String, Vec<usize>)>,
    pub removed_paths: std::collections::HashSet<PathBuf>,
    pub banner_lines: usize,
    pub banner_text: String,
    pub banner_tail: String,
    pub banner_dirty: bool,
    pub playlist_dirty: bool,
    pub current_track_removed: bool,
    pub terminal_resized: bool,
    /// Whether the previous frame left the analysis-spectrogram sixel block on
    /// screen untouched. Cleared at the top of every print_status frame and
    /// re-asserted by the analysis branch — any other frame (playlist/lyrics
    /// view, another viz mode) leaves it false, forcing a full re-emit when
    /// the spectrogram next renders over whatever that frame painted.
    pub spectro_block_intact: bool,
    pub lyrics: Option<crate::lyrics::Lyrics>,
    pub lyrics_receiver: Option<std::sync::mpsc::Receiver<Option<crate::lyrics::Lyrics>>>,
    pub lyrics_scroll: usize,
    pub lyrics_auto_scroll: bool,
    pub lyrics_offset: f64, // seconds, positive = lyrics later, negative = lyrics earlier
    /// Monotonic counter incremented on each lyrics spawn. Workers capture a snapshot
    /// and abort their slow LRCLIB fetch if the counter has advanced (i.e. user skipped).
    pub lyrics_gen: std::sync::Arc<std::sync::atomic::AtomicU64>,

    /// Currently-resolved album cover (half-block pre-rendered lines if decoded).
    pub cover: Option<crate::cover::CoverImage>,
    pub cover_receiver: Option<std::sync::mpsc::Receiver<Option<crate::cover::CoverImage>>>,
    pub cover_gen: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub cover_enabled: bool,
    /// Visible playlist rows from the last render — used by PageUp/PageDown.
    pub last_visible_rows: usize,
}

impl UiState {
    pub fn new(source_paths: Vec<PathBuf>, metadata_cache: std::sync::Arc<crate::metadata::MetadataCache>) -> Self {
        Self {
            view_mode: ViewMode::Player,
            input_mode: InputMode::Normal,
            scroll_offset: 0,
            cursor: 0,
            filtered_indices: Vec::new(),
            current: 0,
            source_paths,
            shuffle: false,
            pre_shuffle_order: None,
            repeat_mode: RepeatMode::Off,
            enqueue_count: 0,
            status_message: None,
            metadata_cache,
            scan_handle: None,
            auto_sort_pending: false,
            library_tree_mode: false,
            library_tree: crate::library::LibraryTree::default(),
            tree_fold: crate::library::FoldState::default(),
            tree_cursor: 0,
            tree_scroll: 0,
            tree_view_height: 0,
            tree_dirty: true,
            eq_band: 0,
            tree_filter: String::new(),
            tree_pending_remove: None,
            removed_paths: std::collections::HashSet::new(),
            banner_lines: 0,
            banner_text: String::new(),
            banner_tail: String::new(),
            banner_dirty: false,
            playlist_dirty: false,
            current_track_removed: false,
            terminal_resized: false,
            spectro_block_intact: false,
            lyrics: None,
            lyrics_receiver: None,
            lyrics_scroll: 0,
            lyrics_auto_scroll: true,
            lyrics_offset: 0.0,
            lyrics_gen: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cover: None,
            cover_receiver: None,
            cover_gen: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cover_enabled: true,
            last_visible_rows: 20,
        }
    }

    pub fn set_status(&mut self, msg: String) {
        self.status_message = Some((msg, Instant::now()));
    }

    pub fn take_status(&mut self) -> Option<String> {
        if let Some((ref msg, when)) = self.status_message {
            if when.elapsed() < std::time::Duration::from_secs(2) {
                return Some(msg.clone());
            }
            self.status_message = None;
        }
        None
    }
}
