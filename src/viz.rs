// This module is pixel/grid-heavy (FFT bins, braille dot grids, scope/spectrogram
// canvases) where `for i in 0..n { buf[row*w + col] }` index math is clearer than
// iterator gymnastics. Allow the range-loop lint module-wide rather than scatter it.
#![allow(clippy::needless_range_loop)]

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Process-global, monotonically increasing id for the newest analysis-spectrogram
/// column. Used to key the paused-frame render cache so it can never reuse a stale
/// image across a track change (each new column gets a fresh, never-repeated id).
static SPECTRO_GEN: AtomicU64 = AtomicU64::new(0);

use realfft::{RealFftPlanner, RealToComplex};

use crate::state::{
    PlayerState, VizMode, VizStyle, SPECTRUM_BANDS, FFT_SIZE, VIZ_DECAY,
    BAR_DECAY, GRAVITY, DOT_GRAVITY, ATTACK, VIZ_ATTACK, HOLD_TIME,
    C_RESET, C_DIM, C_CYAN, C_GREEN, C_YELLOW, C_MAGENTA, C_RED,
};
use crate::theme::{palette as theme_palette, ThemeKind};

/// Per-theme color set used by the viz renderers. `low/mid/hot` map onto the
/// classic green→yellow→red gradient; non-Classic themes collapse them onto
/// the active palette so the spectrum/VU adopt the theme's identity.
struct VizPalette {
    low: &'static str,
    mid: &'static str,
    hot: &'static str,
    #[allow(dead_code)]
    accent: &'static str,
    dim: &'static str,
    reset: &'static str,
}

fn viz_palette(kind: ThemeKind) -> VizPalette {
    match kind {
        ThemeKind::Classic => VizPalette {
            low: C_GREEN,
            mid: C_YELLOW,
            hot: C_RED,
            accent: C_CYAN,
            dim: C_DIM,
            reset: C_RESET,
        },
        ThemeKind::Minimal => {
            // Monochrome: the warm-cyan accent for content, danger only for clip/hot.
            let p = theme_palette(ThemeKind::Minimal);
            VizPalette {
                low: p.accent,
                mid: p.accent,
                hot: p.danger,
                accent: p.accent,
                dim: p.dim,
                reset: p.reset,
            }
        }
        ThemeKind::HiFi => {
            // Amber gradient: dim→fg→accent→danger.
            let p = theme_palette(ThemeKind::HiFi);
            VizPalette {
                low: p.fg,
                mid: p.accent,
                hot: p.danger,
                accent: p.accent,
                dim: p.dim,
                reset: p.reset,
            }
        }
    }
}

/// Per-band color for the spectrum ribbon. Classic uses the rainbow gradient
/// already baked into `BAND_COLORS`; Minimal/HiFi project onto a 3-stop ramp
/// (low→mid→hot) sized to the band index so the visual identity stays
/// consistent with the rest of the theme.
/// ISO ⅓-octave centre frequency of each spectrum band. Drives both the band
/// energies and the frequency legend, so the labels cannot drift from the bins.
pub(crate) const ISO_CENTERS: [f32; SPECTRUM_BANDS] = [
    20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0,
    200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0,
    2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0, 10000.0, 12500.0, 16000.0,
    20000.0,
];

fn band_color(idx: usize, vp: &VizPalette, kind: ThemeKind) -> &'static str {
    if matches!(kind, ThemeKind::Classic) {
        BAND_COLORS.get(idx).copied().unwrap_or(C_YELLOW)
    } else {
        // Map idx in 0..SPECTRUM_BANDS onto the 3 ramp stops.
        let third = SPECTRUM_BANDS / 3;
        if idx < third { vp.low }
        else if idx < third * 2 { vp.mid }
        else { vp.hot }
    }
}

// --- Lightweight process stats (replaces sysinfo dependency) ---

/// Returns (cumulative_cpu_time_microseconds, resident_memory_bytes).
#[cfg(target_os = "macos")]
fn process_stats() -> (u64, u64) {
    #[repr(C)]
    struct TimeValue { seconds: i32, microseconds: i32 }
    #[repr(C)]
    struct TaskThreadTimesInfo {
        user_time: TimeValue,
        system_time: TimeValue,
    }
    #[repr(C)]
    struct TaskVmInfo {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
        _pad: [u64; 16],
    }
    extern "C" {
        fn mach_task_self() -> u32;
        fn task_info(target: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
    }
    const TASK_THREAD_TIMES_INFO: u32 = 3;
    const TASK_VM_INFO: u32 = 22;
    unsafe {
        let task = mach_task_self();

        // CPU times via TASK_THREAD_TIMES_INFO (flavor 3)
        let mut times: TaskThreadTimesInfo = std::mem::zeroed();
        let mut count = (std::mem::size_of::<TaskThreadTimesInfo>() / 4) as u32;
        let cpu_us = if task_info(task, TASK_THREAD_TIMES_INFO,
                                  &mut times as *mut _ as *mut i32, &mut count) == 0 {
            times.user_time.seconds as u64 * 1_000_000 + times.user_time.microseconds as u64
            + times.system_time.seconds as u64 * 1_000_000 + times.system_time.microseconds as u64
        } else { 0 };

        // Memory via TASK_VM_INFO (flavor 22) - Private footprint
        let mut info: TaskVmInfo = std::mem::zeroed();
        count = (std::mem::size_of::<TaskVmInfo>() / 4) as u32;
        let mem = if task_info(task, TASK_VM_INFO,
                               &mut info as *mut _ as *mut i32, &mut count) == 0 {
            info.phys_footprint
        } else { 0 };

        (cpu_us, mem)
    }
}

#[cfg(target_os = "linux")]
fn process_stats() -> (u64, u64) {
    let cpu_us = std::fs::read_to_string("/proc/self/stat").ok().and_then(|stat| {
        let fields: Vec<&str> = stat.split_whitespace().collect();
        if fields.len() > 15 {
            let utime: u64 = fields[13].parse().ok()?;
            let stime: u64 = fields[14].parse().ok()?;
            // Clock ticks to microseconds (100 Hz on virtually all Linux systems)
            Some((utime + stime) * 10_000)
        } else { None }
    }).unwrap_or(0);

    let mem = std::fs::read_to_string("/proc/self/status").ok().and_then(|status| {
        status.lines()
            .find(|l| l.starts_with("RssAnon:"))
            .or_else(|| status.lines().find(|l| l.starts_with("VmRSS:")))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|kb| kb * 1024)
    }).unwrap_or(0);

    (cpu_us, mem)
}

#[cfg(target_os = "windows")]
fn process_stats() -> (u64, u64) {
    use std::ffi::c_void;
    #[repr(C)]
    struct FILETIME { low: u32, high: u32 }
    // Extended version includes PrivateUsage (matches Task Manager's "Memory" column)
    #[repr(C)]
    struct PROCESS_MEMORY_COUNTERS_EX {
        cb: u32, page_fault_count: u32,
        peak_working_set_size: usize, working_set_size: usize,
        quota_peak_paged_pool_usage: usize, quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize, quota_non_paged_pool_usage: usize,
        pagefile_usage: usize, peak_pagefile_usage: usize,
        private_usage: usize,
    }
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessTimes(h: *mut c_void, c: *mut FILETIME, e: *mut FILETIME, k: *mut FILETIME, u: *mut FILETIME) -> i32;
        fn K32GetProcessMemoryInfo(h: *mut c_void, info: *mut PROCESS_MEMORY_COUNTERS_EX, cb: u32) -> i32;
    }
    unsafe {
        let h = GetCurrentProcess();
        let (mut c, mut e, mut k, mut u) = (std::mem::zeroed::<FILETIME>(), std::mem::zeroed::<FILETIME>(),
                                             std::mem::zeroed::<FILETIME>(), std::mem::zeroed::<FILETIME>());
        let cpu_us = if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 {
            let k100 = (k.high as u64) << 32 | k.low as u64;
            let u100 = (u.high as u64) << 32 | u.low as u64;
            (k100 + u100) / 10 // 100ns → µs
        } else { 0 };

        let mut mi: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        mi.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let mem = if K32GetProcessMemoryInfo(h, &mut mi, mi.cb) != 0 {
            mi.private_usage as u64
        } else { 0 };
        (cpu_us, mem)
    }
}

pub struct StatsMonitor {
    num_cpus: f32,
    last_update: Instant,
    prev_cpu_us: u64,
    prev_wall: Instant,
    pub(crate) cpu_usage: f32,
    pub(crate) memory_mb: f64,
    pub(crate) smoothed_buf_pct: f32,
}

impl StatsMonitor {
    pub fn new() -> Self {
        let (cpu_us, _) = process_stats();
        let num_cpus = std::thread::available_parallelism()
            .map(|n| n.get() as f32)
            .unwrap_or(1.0);
        Self {
            num_cpus,
            last_update: Instant::now(),
            prev_cpu_us: cpu_us,
            prev_wall: Instant::now(),
            cpu_usage: 0.0,
            memory_mb: 0.0,
            smoothed_buf_pct: 0.0,
        }
    }

    pub fn update(&mut self) {
        if self.last_update.elapsed() >= Duration::from_millis(500) {
            let (cpu_us, mem_bytes) = process_stats();
            let wall_elapsed = self.prev_wall.elapsed().as_micros() as f64;
            if wall_elapsed > 0.0 {
                let cpu_delta = cpu_us.saturating_sub(self.prev_cpu_us) as f64;
                // Total system % (cpu time / wall time / cores)
                self.cpu_usage = (cpu_delta / wall_elapsed / self.num_cpus as f64 * 100.0) as f32;
            }
            self.memory_mb = mem_bytes as f64 / 1024.0 / 1024.0;
            self.prev_cpu_us = cpu_us;
            self.prev_wall = Instant::now();
            self.last_update = Instant::now();
        }
    }

    pub fn update_buf(&mut self, raw_pct: f32) {
        self.smoothed_buf_pct = self.smoothed_buf_pct * 0.85 + raw_pct * 0.15;
    }
}

struct ChannelBands {
    sample_buffer: VecDeque<f32>,
    smoothed: [f32; SPECTRUM_BANDS],
    heights: [f32; SPECTRUM_BANDS],
}

impl ChannelBands {
    fn new() -> Self {
        Self {
            sample_buffer: VecDeque::with_capacity(FFT_SIZE * 2),
            smoothed: [0.0; SPECTRUM_BANDS],
            heights: [0.0; SPECTRUM_BANDS],
        }
    }
}

// Size of the recent stereo sample ring used by oscilloscope/lissajous.
// 1024 stereo pairs ≈ 21 ms at 48 kHz — enough trace for a clear pattern.
pub const WAVEFORM_BUF_SIZE: usize = 1024;
// Max spectrogram columns kept in history (time axis). The render shows up to the
// terminal width; this is the cap (and history depth) for very wide terminals.
/// Stored spectrogram history, and therefore the widest the character
/// spectrogram can be drawn — it is the only viz whose WIDTH is capped by data
/// rather than by the display. 31 floats a hop, so depth is nearly free.
pub const SPECTROGRAM_COLS: usize = 512;
// Each column averages this many FFT hops, dilating the time axis so the display
// scrolls slower and smoother. At ~43 ms/hop: 1 = ~2.6 s window (was), 4 = ~10 s.
pub const SPECTROGRAM_HOPS_PER_COL: usize = 4;

// Analysis spectrogram: history depth (time window, columns ≈ FFT hops),
// vertical rows, and dB contrast window.
const SPECTRO_ANALYSIS_COLS: usize = 512;
const SPECTRO_ANALYSIS_ROWS: usize = 16;
/// Full-window ceiling for the analysis block. Sixel cost scales with the pixel
/// count, and the image is re-encoded whenever a hop lands, so this is a
/// throughput limit as much as a visual one.
const SPECTRO_ANALYSIS_MAX_ROWS: usize = 32;
// dB contrast window (tune by eye). With the 1/FFT_SIZE magnitude normalization a
// full-scale tone peaks near -12 dB, so the ceiling sits a little below that and
// the floor spans a ~60 dB range down to quiet detail. CEIL = brightest, FLOOR = dark.
const SPECTRO_ANALYSIS_FLOOR_DB: f32 = -80.0;
const SPECTRO_ANALYSIS_CEIL_DB: f32 = -20.0;

pub struct VizAnalyser {
    fft: Arc<dyn RealToComplex<f32>>,
    fft_input: Vec<f32>,
    fft_output: Vec<realfft::num_complex::Complex<f32>>,
    // Reused FFT scratch so the per-hop transform doesn't allocate on the UI thread.
    fft_scratch: Vec<realfft::num_complex::Complex<f32>>,
    window: Vec<f32>,
    ch_l: ChannelBands,
    ch_r: ChannelBands,
    // Peak dots computed from mono (L+R average), used by vertical spectrum
    peak_hold: [f32; SPECTRUM_BANDS],
    peak_hold_timer: [u8; SPECTRUM_BANDS],
    smoothed_peak_l: f32,
    smoothed_peak_r: f32,
    vu_peak_hold_l: f32,
    vu_peak_hold_r: f32,
    vu_peak_timer_l: u8,
    vu_peak_timer_r: u8,
    sample_rate: u32,
    // Recent raw (L, R) samples, newest at back. Used by oscilloscope and lissajous.
    pub(crate) waveform_buf: VecDeque<(f32, f32)>,
    // History of mono spectrum frames, newest at back. Used by spectrogram.
    pub(crate) spectrogram_history: VecDeque<[f32; SPECTRUM_BANDS]>,
    // Analysis spectrogram: per-hop dB magnitude columns (one Vec<f32> of length
    // = FFT bins), newest at back. Captured only while the mode is active.
    spectro_raw_history: VecDeque<Vec<f32>>,
    // Reusable scratch holding the L-channel magnitudes between the L and R FFTs.
    spectro_mag_l: Vec<f32>,
    // Per-bin magnitude accumulator + hop count for the in-progress column. Several
    // hops are averaged into one column so the column rate stays ~constant (and
    // smoothly scroll-able) regardless of sample rate.
    spectro_mag_accum: Vec<f32>,
    spectro_mag_count: usize,
    // Generation id of the newest pushed column (from SPECTRO_GEN). Keys the paused render cache.
    spectro_last_gen: u64,
    // Running sum of hops for the in-progress spectrogram column (time dilation).
    spectrogram_accum: [f32; SPECTRUM_BANDS],
    spectrogram_accum_count: usize,
}

impl VizAnalyser {
    pub fn new(sample_rate: u32) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let fft_input = fft.make_input_vec();
        let fft_output = fft.make_output_vec();
        let fft_scratch = fft.make_scratch_vec();
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 *(1.0 - (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos()))
            .collect();

        Self {
            fft,
            fft_input,
            fft_output,
            fft_scratch,
            window,
            ch_l: ChannelBands::new(),
            ch_r: ChannelBands::new(),
            peak_hold: [0.0; SPECTRUM_BANDS],
            peak_hold_timer: [0; SPECTRUM_BANDS],
            smoothed_peak_l: 0.0,
            smoothed_peak_r: 0.0,
            vu_peak_hold_l: 0.0,
            vu_peak_hold_r: 0.0,
            vu_peak_timer_l: 0,
            vu_peak_timer_r: 0,
            sample_rate,
            waveform_buf: VecDeque::with_capacity(WAVEFORM_BUF_SIZE),
            spectrogram_history: VecDeque::with_capacity(SPECTROGRAM_COLS),
            spectro_raw_history: VecDeque::with_capacity(SPECTRO_ANALYSIS_COLS),
            spectro_mag_l: Vec::new(),
            spectro_mag_accum: Vec::new(),
            spectro_mag_count: 0,
            spectro_last_gen: 0,
            spectrogram_accum: [0.0; SPECTRUM_BANDS],
            spectrogram_accum_count: 0,
        }
    }

    pub fn process(&mut self, samples: &[f32], channels: usize, state: &PlayerState) {
        if channels == 0 || samples.is_empty() {
            return;
        }

        // Calculate peak levels per channel
        let mut peak_l: f32 = 0.0;
        let mut peak_r: f32 = 0.0;

        let frames = samples.len() / channels;
        for f in 0..frames {
            let l_raw = samples[f * channels];
            let l = l_raw.abs();
            peak_l = peak_l.max(l);
            let r_raw = if channels >= 2 {
                let r = samples[f * channels + 1];
                peak_r = peak_r.max(r.abs());
                self.ch_l.sample_buffer.push_back(l_raw);
                self.ch_r.sample_buffer.push_back(r);
                r
            } else {
                peak_r = peak_l;
                self.ch_l.sample_buffer.push_back(l_raw);
                self.ch_r.sample_buffer.push_back(l_raw);
                l_raw
            };
            if self.waveform_buf.len() == WAVEFORM_BUF_SIZE {
                self.waveform_buf.pop_front();
            }
            self.waveform_buf.push_back((l_raw, r_raw));
        }

        // Smooth peak levels with fast attack, slow decay (VU meter behavior)
        const ATTACK_FACTOR: f32 = 0.3;
        const DECAY_FACTOR: f32 = 0.92;

        if peak_l > self.smoothed_peak_l {
            self.smoothed_peak_l = self.smoothed_peak_l * ATTACK_FACTOR + peak_l * (1.0 - ATTACK_FACTOR);
        } else {
            self.smoothed_peak_l *= DECAY_FACTOR;
        }

        if peak_r > self.smoothed_peak_r {
            self.smoothed_peak_r = self.smoothed_peak_r * ATTACK_FACTOR + peak_r * (1.0 - ATTACK_FACTOR);
        } else {
            self.smoothed_peak_r *= DECAY_FACTOR;
        }

        state.set_peaks(self.smoothed_peak_l, self.smoothed_peak_r);

        // VU peak dots
        if self.smoothed_peak_l >= self.vu_peak_hold_l {
            self.vu_peak_hold_l = self.smoothed_peak_l;
            self.vu_peak_timer_l = HOLD_TIME;
        } else if self.vu_peak_timer_l > 0 {
            self.vu_peak_timer_l -= 1;
        } else {
            self.vu_peak_hold_l = (self.vu_peak_hold_l - DOT_GRAVITY).max(0.0);
        }

        if self.smoothed_peak_r >= self.vu_peak_hold_r {
            self.vu_peak_hold_r = self.smoothed_peak_r;
            self.vu_peak_timer_r = HOLD_TIME;
        } else if self.vu_peak_timer_r > 0 {
            self.vu_peak_timer_r -= 1;
        } else {
            self.vu_peak_hold_r = (self.vu_peak_hold_r - DOT_GRAVITY).max(0.0);
        }

        state.set_vu_dots(self.vu_peak_hold_l, self.vu_peak_hold_r);

        // Process FFT for each channel when enough samples collected
        while self.ch_l.sample_buffer.len() >= FFT_SIZE && self.ch_r.sample_buffer.len() >= FFT_SIZE {
            // Process L channel
            for (i, (&sample, &w)) in self.ch_l.sample_buffer.iter().take(FFT_SIZE).zip(&self.window).enumerate() {
                self.fft_input[i] = sample * w;
            }
            let l_bands = Self::run_fft_and_compute(&*self.fft, &mut self.fft_input, &mut self.fft_output, &mut self.fft_scratch, self.sample_rate);

            // Analysis-spectrogram capture: stash L magnitudes (fft_output now holds L's spectrum).
            let capture = state.viz_mode() == VizMode::SpectrogramAnalysis;
            if capture {
                let nbins = self.fft_output.len();
                self.spectro_mag_l.resize(nbins, 0.0);
                for (i, c) in self.fft_output.iter().enumerate() {
                    self.spectro_mag_l[i] = c.norm();
                }
            }

            // Process R channel
            for (i, (&sample, &w)) in self.ch_r.sample_buffer.iter().take(FFT_SIZE).zip(&self.window).enumerate() {
                self.fft_input[i] = sample * w;
            }
            let r_bands = Self::run_fft_and_compute(&*self.fft, &mut self.fft_input, &mut self.fft_output, &mut self.fft_scratch, self.sample_rate);

            // Analysis-spectrogram capture: accumulate mono magnitude = avg(|L|,|R|)
            // per bin, and push one averaged dB column every `hops_per_col` hops so
            // the column rate stays ~constant (and smoothly scroll-able) across
            // sample rates.
            if capture {
                let nbins = self.fft_output.len();
                let norm = 1.0 / FFT_SIZE as f32;
                if self.spectro_mag_accum.len() != nbins {
                    self.spectro_mag_accum = vec![0.0; nbins];
                    self.spectro_mag_count = 0;
                }
                for i in 0..nbins {
                    self.spectro_mag_accum[i] += (self.spectro_mag_l[i] + self.fft_output[i].norm()) * 0.5 * norm;
                }
                self.spectro_mag_count += 1;

                let hops_per_col = crate::state::spectro_hops_per_col(self.sample_rate as u64);
                if self.spectro_mag_count >= hops_per_col {
                    let inv = 1.0 / self.spectro_mag_count as f32;
                    let mut col = if self.spectro_raw_history.len() >= SPECTRO_ANALYSIS_COLS {
                        self.spectro_raw_history.pop_front().unwrap()
                    } else {
                        Vec::with_capacity(nbins)
                    };
                    col.clear();
                    for &acc in self.spectro_mag_accum.iter() {
                        col.push(20.0 * (acc * inv + 1e-9).log10());
                    }
                    self.spectro_raw_history.push_back(col);
                    self.spectro_last_gen = SPECTRO_GEN.fetch_add(1, Ordering::Relaxed);
                    for v in self.spectro_mag_accum.iter_mut() { *v = 0.0; }
                    self.spectro_mag_count = 0;
                }
            } else if !self.spectro_raw_history.is_empty() {
                self.spectro_raw_history.clear();
                self.spectro_mag_accum.clear();
                self.spectro_mag_count = 0;
            }

            // Apply ballistics per channel
            Self::apply_ballistics(&l_bands, &mut self.ch_l.heights, &mut self.ch_l.smoothed);
            Self::apply_ballistics(&r_bands, &mut self.ch_r.heights, &mut self.ch_r.smoothed);

            // Mono average for peak dots (used by vertical spectrum)
            let mono: [f32; SPECTRUM_BANDS] = std::array::from_fn(|i| {
                (self.ch_l.smoothed[i] + self.ch_r.smoothed[i]) / 2.0
            });
            for i in 0..SPECTRUM_BANDS {
                if mono[i] >= self.peak_hold[i] {
                    self.peak_hold[i] = mono[i];
                    self.peak_hold_timer[i] = HOLD_TIME;
                } else if self.peak_hold_timer[i] > 0 {
                    self.peak_hold_timer[i] -= 1;
                } else {
                    self.peak_hold[i] = (self.peak_hold[i] - DOT_GRAVITY).max(0.0);
                }
                self.peak_hold[i] = self.peak_hold[i].max(mono[i]);
            }

            // Update shared state
            state.set_spectrum(&self.ch_l.smoothed);
            state.set_spectrum_r(&self.ch_r.smoothed);
            state.set_dots(&self.peak_hold);

            // Accumulate hops into the current spectrogram column; push a column
            // (the hop average) only every SPECTROGRAM_HOPS_PER_COL hops. This
            // dilates the time axis so the spectrogram scrolls slower and smoother.
            // mono = L+R average of the smoothed bands.
            for (acc, &m) in self.spectrogram_accum.iter_mut().zip(mono.iter()) {
                *acc += m;
            }
            self.spectrogram_accum_count += 1;
            if self.spectrogram_accum_count >= SPECTROGRAM_HOPS_PER_COL {
                let inv = 1.0 / self.spectrogram_accum_count as f32;
                let col: [f32; SPECTRUM_BANDS] = std::array::from_fn(|i| self.spectrogram_accum[i] * inv);
                if self.spectrogram_history.len() == SPECTROGRAM_COLS {
                    self.spectrogram_history.pop_front();
                }
                self.spectrogram_history.push_back(col);
                self.spectrogram_accum = [0.0; SPECTRUM_BANDS];
                self.spectrogram_accum_count = 0;
            }

            // 50% overlap
            self.ch_l.sample_buffer.drain(..FFT_SIZE / 2);
            self.ch_r.sample_buffer.drain(..FFT_SIZE / 2);
        }
    }

    /// Run FFT on samples and return raw band values (no ballistics)
    fn run_fft_and_compute(
        fft: &dyn RealToComplex<f32>,
        fft_input: &mut [f32],
        fft_output: &mut [realfft::num_complex::Complex<f32>],
        scratch: &mut [realfft::num_complex::Complex<f32>],
        sample_rate: u32,
    ) -> [f32; SPECTRUM_BANDS] {
        if fft.process_with_scratch(fft_input, fft_output, scratch).is_err() {
            return [0.0; SPECTRUM_BANDS];
        }

        let nyquist = sample_rate as f32 / 2.0;
        let n_bins = fft_output.len();
        let bin_hz = nyquist / n_bins as f32;
        let n = FFT_SIZE as f32;
        let window_correction = 2.0;
        let psd_norm = 2.0 / (n * n);

        let factor = 2.0f32.powf(1.0 / 6.0);
        let mut freq_bands = [0.0f32; SPECTRUM_BANDS + 1];
        for i in 0..SPECTRUM_BANDS {
            freq_bands[i] = ISO_CENTERS[i] / factor;
        }
        freq_bands[SPECTRUM_BANDS] = ISO_CENTERS[SPECTRUM_BANDS - 1] * factor;

        let mut bands = [0.0f32; SPECTRUM_BANDS];

        for (band_idx, bw) in freq_bands.windows(2).enumerate() {
            let f_lo = bw[0];
            let f_hi = bw[1];
            let center_freq = ISO_CENTERS[band_idx];

            let bin_lo_exact = f_lo / bin_hz;
            let bin_hi_exact = f_hi / bin_hz;
            let bin_lo = bin_lo_exact.floor() as usize;
            let bin_hi = (bin_hi_exact.ceil() as usize).min(n_bins);

            let mut sum_power = 0.0f32;
            let mut weight_sum = 0.0f32;
            for bin in bin_lo..bin_hi {
                let bin_start = bin as f32;
                let bin_end = bin_start + 1.0;
                let overlap_lo = bin_start.max(bin_lo_exact);
                let overlap_hi = bin_end.min(bin_hi_exact);
                let weight = (overlap_hi - overlap_lo).max(0.0);

                let mag = fft_output[bin].norm() * window_correction;
                sum_power += mag * mag * psd_norm * weight;
                weight_sum += weight;
            }

            let rms_power = if weight_sum > 0.0 { sum_power / weight_sum } else { 0.0 };

            // Spectral Tilt Correction (+3dB per octave relative to 1kHz)
            // Compensates for pink-noise spectral slope, no A-weighting
            // (A-weighting is for SPL meters, not spectrum analyzers)
            let tilt_db = (center_freq / 1000.0).log2() * 3.0;

            let raw_db = 10.0 * (rms_power + 1e-12).log10();
            let processed_db = raw_db + tilt_db;

            bands[band_idx] = ((processed_db + 90.0) / 90.0).clamp(0.0, 1.0);
        }

        bands
    }

    /// Apply bar ballistics (attack/decay/smoothing) to raw band values
    fn apply_ballistics(
        bands: &[f32; SPECTRUM_BANDS],
        heights: &mut [f32; SPECTRUM_BANDS],
        smoothed: &mut [f32; SPECTRUM_BANDS],
    ) {
        for i in 0..SPECTRUM_BANDS {
            if bands[i] > heights[i] {
                heights[i] = heights[i] * (1.0 - ATTACK) + bands[i] * ATTACK;
            } else {
                // Proportional fall + small linear floor: high bars fall at the same
                // rate as low ones, so loud passages stay responsive instead of the
                // bars crawling down from a fixed per-frame step.
                heights[i] = (heights[i] * BAR_DECAY - GRAVITY).max(0.0);
            }
            // Fast attack so beats land on time, slow release so the fall stays
            // smooth. A symmetric low-pass here added ~150 ms of onset lag.
            smoothed[i] = if heights[i] > smoothed[i] {
                smoothed[i] * VIZ_ATTACK + heights[i] * (1.0 - VIZ_ATTACK)
            } else {
                smoothed[i] * VIZ_DECAY + heights[i] * (1.0 - VIZ_DECAY)
            };
        }
    }
}

const SPECTRUM_H_CHARS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

const VU_MAX_ROWS: usize = 40;
/// Bar thickness caps here. It buys weight, not information — the meter still
/// shows two numbers — but a full window has the room, and a bar that fills it
/// reads better than one stranded at the top. Always an ODD number of rows so
/// the channel label has a true middle row to sit on.
const VU_MAX_THICKNESS: usize = 7;
/// Level history for the full-window strip, newest last.
const VU_HISTORY: usize = 512;
/// Height of the bars alone: both channels at the thickness cap, the gap, and
/// the permanent scale row.
const VU_BARS_ROWS: usize = 2 * VU_MAX_THICKNESS + 2;


/// How the VU meter spends `rows`.
///
/// Two peak values plus two hold dots is all the data there is, so extra height
/// cannot be filled by drawing them larger indefinitely. It buys weight first
/// (thickness, capped), then a dB ruler, and everything beyond that goes to a
/// level-over-time strip — the one part of a tall VU meter that shows something
/// a single row could not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct VuLayout {
    thickness: usize,
    gap: usize,
    ruler: usize,
    history: usize,
    /// Rows the budget gave us that nothing wants — only when the history strip
    /// is switched off and the block was sized for it.
    blank: usize,
}

#[cfg(test)]
impl VuLayout {
    /// Every row the layout accounts for; must equal the budget it was given.
    fn total(self) -> usize {
        self.thickness * 2 + self.gap + self.ruler + self.history + self.blank
    }
}

fn vu_layout(rows: usize, style: VizStyle, extras: bool) -> VuLayout {
    if rows == 0 {
        return VuLayout { thickness: 0, gap: 0, ruler: 0, history: 0, blank: 0 };
    }
    if rows < 4 {
        // Natural size: one row per channel, and the Bars style's spacer.
        let gap = if rows >= 3 { rows - 2 } else { 0 };
        let thickness = if rows == 1 { 0 } else { 1 };
        // rows == 1 has no room for two channels; give the single row to L.
        return VuLayout {
            thickness: thickness.max(if rows == 1 { 0 } else { 1 }),
            gap,
            ruler: 0,
            history: 0,
            blank: if rows == 1 { 1 } else { 0 },
        };
    }
    // The scale is permanent from here up: a meter without one is just a
    // moving bar, and it costs a single row.
    let ruler = 1;
    let gap = if matches!(style, VizStyle::Bars) { 1 } else { 0 };
    let body = rows - ruler - gap;
    // Snap down to odd: 1, 3, 5, 7. An even block has no middle row, so the
    // channel label would have to sit off-centre.
    let raw = (body / 2).clamp(1, VU_MAX_THICKNESS);
    let thickness = if raw.is_multiple_of(2) { raw - 1 } else { raw };
    let spare = rows - (thickness * 2 + gap + ruler);
    let history = if extras { spare } else { 0 };
    VuLayout { thickness, gap, ruler, history, blank: spare - history }
}

/// Push this frame's levels onto the history ring and read it back.
///
/// Render-driven rather than analyser-driven: the meter is the only consumer,
/// and it is drawn once per UI frame, so the strip advances at a steady ~20 px
/// per second without another buffer in the analyser.
fn vu_push_history(l: f32, r: f32) -> Vec<(f32, f32)> {
    use std::cell::RefCell;
    thread_local! {
        static HIST: RefCell<std::collections::VecDeque<(f32, f32)>> =
            const { RefCell::new(std::collections::VecDeque::new()) };
    }
    HIST.with(|h| {
        let mut h = h.borrow_mut();
        if h.len() == VU_HISTORY {
            h.pop_front();
        }
        h.push_back((l, r));
        h.iter().copied().collect()
    })
}

/// Marks for the dB scale: label and its column on a `bar_width` bar.
///
/// The bar is linear in amplitude, so a decibel sits at 10^(dB/20) of its
/// length — 0 dB at the far right, -6 dB halfway, -20 dB at a tenth. Marks are
/// dropped from the quiet end first when they would collide, since that end is
/// where a linear bar crowds them together.
fn vu_scale_marks(bar_width: usize) -> Vec<(usize, &'static str)> {
    const DB: [(f32, &str); 6] = [
        (-40.0, "-40"), (-20.0, "-20"), (-12.0, "-12"),
        (-6.0, "-6"), (-3.0, "-3"), (0.0, "0"),
    ];
    let mut out: Vec<(usize, &'static str)> = Vec::new();
    for (db, text) in DB.iter().rev() {
        let frac = 10f32.powf(db / 20.0);
        let col = ((frac * bar_width as f32).round() as usize)
            .min(bar_width.saturating_sub(text.len()));
        // Walking loud-to-quiet, keep a mark only if it clears the one already
        // placed to its right.
        if out.last().is_some_and(|(c, _): &(usize, &str)| col + text.len() + 1 > *c) {
            continue;
        }
        out.push((col, text));
    }
    out.reverse();
    out
}

/// The permanent dB scale under the bars.
fn vu_scale_row(bar_width: usize, label_w: usize, vp: &VizPalette) -> String {
    let mut cells = vec![' '; bar_width];
    for (col, text) in vu_scale_marks(bar_width) {
        for (i, ch) in text.chars().enumerate() {
            if let Some(slot) = cells.get_mut(col + i) {
                *slot = ch;
            }
        }
    }
    let mut line = String::from("  ");
    for _ in 0..label_w {
        line.push(' ');
    }
    line.push_str(vp.dim);
    line.extend(cells);
    line.push_str(vp.reset);
    line
}

/// The level-over-time strip: newest at the right, filled from the bottom.
fn vu_history_lines(hist: &[(f32, f32)], w: usize, rows: usize, style: VizStyle,
                    vp: &VizPalette) -> Vec<String> {
    // The strip is drawn in the same alphabet as the bars above it — braille in
    // Dots, blocks in Bars — or the two halves of the meter read as different
    // visualizations stacked on top of each other.
    const BLOCKS: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];
    const BRAILLE: [char; 8] = [' ', '⣀', '⣀', '⣤', '⣤', '⣶', '⣶', '⣿'];
    let partials = match style { VizStyle::Bars => BLOCKS, VizStyle::Dots => BRAILLE };
    let full = match style { VizStyle::Bars => '█', VizStyle::Dots => '⣿' };
    let mut lines = Vec::with_capacity(rows);
    let start = hist.len().saturating_sub(w);
    let shown = &hist[start..];
    for row in 0..rows {
        // Row 0 is the top of the strip, so it covers the highest level slice.
        let row_bottom = (rows - 1 - row) as f32 / rows as f32;
        let row_top = (rows - row) as f32 / rows as f32;
        let mut line = String::from("  ");
        let pad = w.saturating_sub(shown.len());
        for _ in 0..pad {
            line.push(' ');
        }
        let mut last = "";
        for &(l, r) in shown {
            let level = l.max(r).clamp(0.0, 1.0);
            let color = if level >= 0.8 { vp.hot } else if level >= 0.6 { vp.mid } else { vp.low };
            if color != last {
                line.push_str(color);
                last = color;
            }
            if level >= row_top {
                line.push(full);
            } else if level > row_bottom {
                let frac = (level - row_bottom) / (row_top - row_bottom);
                line.push(partials[((frac * 7.0) as usize).clamp(1, 7)]);
            } else {
                line.push(' ');
            }
        }
        line.push_str(vp.reset);
        lines.push(line);
    }
    lines
}

/// The width half of the renderer contract ("exactly `rows` lines, none wider
/// than the window"), enforced in one place per renderer instead of inside
/// each layout. Several layouts have a floor — the VU label column plus a
/// 10-cell bar, an 8-column scope grid — and below it they drew past the
/// window edge (a 14-column VU line in a 1-column window). An over-wide line
/// wraps into a row nobody budgeted for and drifts the frame, so a too-small
/// window gets a clipped picture rather than a broken frame.
fn fit_width(lines: Vec<String>, width: usize) -> Vec<String> {
    lines
        .into_iter()
        .map(|l| {
            if crate::ansi::visible_len(&l) <= width {
                l
            } else {
                // The cut drops the line's trailing SGR reset with the text
                // after it; restore one so the colour can't bleed on.
                format!("{}\x1B[0m", crate::ansi::truncate_ansi(&l, width))
            }
        })
        .collect()
}

/// See `render_vu_meter_body`; this enforces the width half of the renderer contract
/// (`fit_width`).
pub fn render_vu_meter(state: &PlayerState, style: VizStyle, width: usize, rows: usize, extras: bool) -> Vec<String> {
    fit_width(render_vu_meter_body(state, style, width, rows, extras), width)
}

fn render_vu_meter_body(state: &PlayerState, style: VizStyle, width: usize, rows: usize,
                       extras: bool) -> Vec<String> {
    let (left, right) = state.get_peaks();
    let (dot_l, dot_r) = state.get_vu_dots();
    // Fill the width: 2-space pad + "L " label (2) + 2-col safety margin = 6 overhead.
    let bar_width = width.saturating_sub(6).max(10);
    let vp = viz_palette(state.theme_kind());

    fn make_bar(level: f32, dot_val: f32, label: &str, width: usize, style: VizStyle, vp: &VizPalette) -> String {
        let full = (level.clamp(0.0, 1.0) * width as f32) as usize;
        let dot_idx = (dot_val.clamp(0.0, 1.0) * width as f32) as usize;

        let yellow_start = width * 6 / 10 + 1;
        let red_start = width * 8 / 10 + 1;

        let mut bar = format!("  {dim}{label}{rst} ", dim = vp.dim, rst = vp.reset, label = label);
        let mut last_color = "";
        for i in 0..width {
            let color = if i >= red_start { vp.hot }
                        else if i >= yellow_start { vp.mid }
                        else { vp.low };
            if color != last_color {
                bar.push_str(color);
                last_color = color;
            }

            match style {
                VizStyle::Dots => {
                    if i < full {
                        bar.push('⣿');
                    } else if i == dot_idx && dot_idx > 0 {
                        bar.push_str(vp.reset);
                        bar.push_str(color);
                        last_color = color;
                        bar.push('⠅');
                    } else {
                        if last_color != vp.dim { bar.push_str(vp.dim); last_color = vp.dim; }
                        bar.push('⣀');
                    }
                }
                VizStyle::Bars => {
                    if i < full {
                        bar.push('█');
                    } else if i == dot_idx && dot_idx > 0 {
                        // Bright thin bar as peak dot
                        bar.push_str(vp.reset);
                        bar.push_str(color);
                        last_color = color;
                        bar.push('▏');
                    } else {
                        if last_color != vp.dim { bar.push_str(vp.dim); last_color = vp.dim; }
                        bar.push('▏');
                    }
                }
            }
        }
        bar.push_str(vp.reset);
        bar
    }

    let hist = vu_push_history(left, right);
    let lay = vu_layout(rows, style, extras);
    let mut lines: Vec<String> = Vec::with_capacity(rows);
    if lay.thickness > 0 {
        // One label per channel, on the block's middle row. Repeating it on
        // every row of a thick bar reads as three separate meters.
        let mid = lay.thickness / 2;
        for i in 0..lay.thickness {
            let label = if i == mid { "L" } else { " " };
            lines.push(make_bar(left, dot_l, label, bar_width, style, &vp));
        }
        for _ in 0..lay.gap {
            lines.push(String::new());
        }
        for i in 0..lay.thickness {
            let label = if i == mid { "R" } else { " " };
            lines.push(make_bar(right, dot_r, label, bar_width, style, &vp));
        }
    }
    for _ in 0..lay.ruler {
        lines.push(vu_scale_row(bar_width, 2, &vp));
    }
    if lay.history > 0 {
        lines.extend(vu_history_lines(&hist, bar_width + 2, lay.history, style, &vp));
    }
    // The layout is the contract: return exactly the rows we were given.
    lines.truncate(rows);
    while lines.len() < rows {
        lines.push(String::new());
    }
    lines
}

// The horizontal spectrum is stacked over SPECTRUM_H_ROWS braille rows per channel
// (was a single row) for more height. Per-row partial fills index by quarters filled
// (1..4): up fills from the bottom (L channel), down fills from the top (R channel).
/// Ceiling both spectrum modes grow to in full window. Rows are pure magnitude
/// resolution here, so the cap is about taste rather than data — past this the
/// bars read as a wall.
const SPECTRUM_MAX_ROWS: usize = 40;
const H_UP_BRAILLE: [char; 5] = [' ', '⣀', '⣤', '⣶', '⣿'];
const H_DN_BRAILLE: [char; 5] = [' ', '⠉', '⠛', '⠿', '⣿'];
// Block chars inverted: index N → bar fills N/8 from the top
const SPECTRUM_H_BLOCKS_DN: &[char] = &[' ', '▇', '▆', '▅', '▄', '▃', '▂', '▁', '█'];

// 31-band color gradient: sub-bass → bass → mid → upper-mid → treble → air
const BAND_COLORS: [&str; 31] = [
    C_CYAN, C_CYAN, C_CYAN, C_CYAN,           // 20-40Hz sub-bass
    C_GREEN, C_GREEN, C_GREEN, C_GREEN,         // 50-100Hz bass
    C_GREEN, C_GREEN, C_GREEN,                  // 125-200Hz upper bass
    C_YELLOW, C_YELLOW, C_YELLOW, C_YELLOW,     // 250-500Hz low-mid
    C_YELLOW, C_YELLOW, C_YELLOW, C_YELLOW,     // 630-1.6kHz mid
    C_RED, C_RED, C_RED, C_RED,                 // 2-4kHz presence
    C_RED, C_RED, C_RED,                        // 5-8kHz brilliance
    C_MAGENTA, C_MAGENTA, C_MAGENTA, C_MAGENTA, // 10-20kHz air
    C_MAGENTA,
];

/// See `render_spectrum_horizontal_body`; this enforces the width half of the renderer contract
/// (`fit_width`).
pub fn render_spectrum_horizontal(state: &PlayerState, style: VizStyle, width: usize, rows: usize, extras: bool) -> Vec<String> {
    fit_width(render_spectrum_horizontal_body(state, style, width, rows, extras), width)
}

fn render_spectrum_horizontal_body(state: &PlayerState, style: VizStyle, width: usize,
                                  rows: usize, extras: bool) -> Vec<String> {
    let spec_l = state.get_spectrum();
    let spec_r = state.get_spectrum_r();
    let kind = state.theme_kind();
    let vp = viz_palette(kind);
    let cw = spectrum_cell_w(width);
    let cols = spectrum_col_count(width, cw);
    let groups = spectrum_columns(cols);
    let indent = " ".repeat(spectrum_indent(width, groups.len(), cw));
    // Rows split evenly between the two channels; an odd budget leaves one
    // blank row on the centre line rather than making the channels unequal.
    // With the legend on, the centre line between the up-growing L bars and the
    // down-growing R bars is exactly where a frequency axis belongs.
    let legend = extras && rows >= 3;
    let n = ((rows.saturating_sub(usize::from(legend))) / 2).max(1);
    let centre = rows.saturating_sub(n * 2);
    let mut lines: Vec<String> = Vec::with_capacity(rows);

    // L channel: bars grow upward. Rows print top→bottom, so row 0 covers the
    // highest magnitude slice [(n-1)/n, 1.0] and the last row the base [0, 1/n].
    for r in 0..n {
        let lo = (n - 1 - r) as f32 / n as f32;
        let hi = (n - r) as f32 / n as f32;
        let mut line = indent.clone();
        for &g in &groups {
            let (level, src) = spectrum_group(&spec_l, g);
            let color = band_color(src, &vp, kind);
            line.push_str(&h_cell(level, lo, hi, style, color, true, cw));
        }
        line.push_str(vp.reset);
        lines.push(line);
    }

    for i in 0..centre {
        if legend && i == 0 {
            lines.push(spectrum_legend_row(&groups, cw, indent.len(), &vp));
        } else {
            lines.push(String::new());
        }
    }

    // R channel: bars grow downward. Rows print top→bottom, so row 0 is the base
    // [0, 1/n] just under the L bars and the last row the deepest [(n-1)/n, 1.0].
    for r in 0..n {
        let lo = r as f32 / n as f32;
        let hi = (r + 1) as f32 / n as f32;
        let mut line = indent.clone();
        for &g in &groups {
            let (level, src) = spectrum_group(&spec_r, g);
            let color = band_color(src, &vp, kind);
            line.push_str(&h_cell(level, lo, hi, style, color, false, cw));
        }
        line.push_str(vp.reset);
        lines.push(line);
    }

    lines.truncate(rows);
    lines
}

/// Render one 2-char-wide spectrum cell for a horizontal-spectrum row spanning the
/// magnitude range `[lo, hi)`. `up` selects bottom-up fill (L channel) vs top-down
/// fill (R channel).
fn h_cell(level: f32, lo: f32, hi: f32, style: VizStyle, color: &str, up: bool, cw: usize) -> String {
    if level >= hi {
        let full = match style { VizStyle::Bars => '█', VizStyle::Dots => '⣿' };
        return spectrum_cell(color, full, cw);
    }
    if level <= lo {
        return " ".repeat(cw);
    }
    let frac = (level - lo) / (hi - lo); // fraction of this row that's filled
    match style {
        VizStyle::Dots => {
            let idx = ((frac * 4.0).ceil() as usize).clamp(1, 4);
            let ch = if up { H_UP_BRAILLE[idx] } else { H_DN_BRAILLE[idx] };
            spectrum_cell(color, ch, cw)
        }
        VizStyle::Bars => {
            let idx = ((frac * 8.0).ceil() as usize).clamp(1, 8);
            if up {
                spectrum_cell(color, SPECTRUM_H_CHARS[idx], cw)
            } else if idx >= 8 {
                spectrum_cell(color, '█', cw)
            } else {
                // Reverse video: FG becomes BG and vice versa, so the block's
                // "empty" part uses the terminal's real background (invisible),
                // making the block fill from the top of the cell.
                // Reverse video fills from the top of the cell; the glyph is
                // repeated across the bar's width like every other cell.
                format!("{}\x1B[7m{}\x1B[27m{C_RESET} ", color,
                        SPECTRUM_H_BLOCKS_DN[idx].to_string().repeat(cw - 1))
            }
        }
    }
}

/// See `render_spectrum_vertical_body`; this enforces the width half of the renderer contract
/// (`fit_width`).
pub fn render_spectrum_vertical(state: &PlayerState, style: VizStyle, width: usize, rows: usize, extras: bool) -> Vec<String> {
    fit_width(render_spectrum_vertical_body(state, style, width, rows, extras), width)
}

fn render_spectrum_vertical_body(state: &PlayerState, style: VizStyle, width: usize,
                                rows: usize, extras: bool) -> Vec<String> {
    const LOWER_BLOCKS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];
    const BRAILLE_V: &[char] = &[' ', '⣀', '⣀', '⣤', '⣤', '⣶', '⣶', '⣿'];
    let spec_l = state.get_spectrum();
    let spec_r = state.get_spectrum_r();
    let spectrum: [f32; SPECTRUM_BANDS] = std::array::from_fn(|i| (spec_l[i] + spec_r[i]) / 2.0);
    let dots = state.get_dots();
    // No room reserved means draw nothing, not one row anyway.
    if rows == 0 {
        return Vec::new();
    }
    // The legend takes the bottom row, so the bars lose one.
    let legend = extras && rows >= 2;
    let height = rows - usize::from(legend);
    let cw = spectrum_cell_w(width);
    let groups = spectrum_columns(spectrum_col_count(width, cw));
    let indent = " ".repeat(spectrum_indent(width, groups.len(), cw));
    let mut lines = vec![String::new(); height];

    let vp = viz_palette(state.theme_kind());
    // Top rows are "hot" (loud), bottom rows are quiet. Keyed off the row's
    // position in the range rather than a fixed 8-entry table, so the ramp
    // holds at any height: the top quarter is hot, the next quarter mid.
    let row_color = |row: usize| -> &str {
        let frac = row as f32 / height as f32;
        if frac < 0.25 { vp.hot } else if frac < 0.5 { vp.mid } else { vp.low }
    };

    let partials = match style {
        VizStyle::Bars => LOWER_BLOCKS,
        VizStyle::Dots => BRAILLE_V,
    };

    for row in 0..height {
        lines[row].push_str(&indent);
        let row_bottom = (height - 1 - row) as f32 / height as f32;
        let row_top = (height - row) as f32 / height as f32;
        let color = row_color(row);

        for &g in &groups {
            let (level, _) = spectrum_group(&spectrum, g);
            let (dot, _) = spectrum_group(&dots, g);
            let dot_in_row = dot >= row_bottom && dot < row_top;
            let bar_partial = level > row_bottom && level < row_top;
            let bar_full = level >= row_top;

            if bar_full {
                let ch = match style { VizStyle::Bars => '█', VizStyle::Dots => '⣿' };
                lines[row].push_str(C_RESET);
                lines[row].push_str(&spectrum_cell(color, ch, cw));
            } else if bar_partial && dot_in_row {
                let frac = (dot - row_bottom) / (row_top - row_bottom);
                let idx = (frac * 7.0).clamp(1.0, 7.0) as usize;
                lines[row].push_str(C_RESET);
                lines[row].push_str(&spectrum_cell(color, partials[idx], cw));
            } else if dot_in_row {
                let dot_ch = match style {
                    VizStyle::Dots => '⣀',
                    VizStyle::Bars => {
                        let frac = (dot - row_bottom) / (row_top - row_bottom);
                        let idx = (frac * 7.0).clamp(1.0, 7.0) as usize;
                        LOWER_BLOCKS[idx.min(2)]
                    }
                };
                lines[row].push_str(C_RESET);
                lines[row].push_str(&spectrum_cell(color, dot_ch, cw));
            } else if bar_partial {
                let frac = (level - row_bottom) / (row_top - row_bottom);
                let idx = (frac * 7.0).max(1.0) as usize;
                lines[row].push_str(C_RESET);
                lines[row].push_str(&spectrum_cell(color, partials[idx], cw));
            } else {
                lines[row].push_str(vp.reset);
                for _ in 0..cw { lines[row].push(' '); }
            }
        }
        lines[row].push_str(vp.reset);
    }
    if legend {
        lines.push(spectrum_legend_row(&groups, cw, indent.len(), &vp));
    }
    lines
}

/// Glyphs per spectrum band. The band count is fixed at 31 by the atomics that
/// carry it, so extra width goes into bar WEIGHT, in whole steps: one glyph,
/// then two, then three, each with a one-column gap. Padding a single glyph out
/// with spaces instead would make a wide window look sparser, not bigger.
/// Deriving more than 31 bands from the FFT's 2048 bins is the real horizontal
/// win and needs the state pipeline widened first.
fn spectrum_bar_w(width: usize) -> usize {
    (width.saturating_sub(2) / SPECTRUM_BANDS).saturating_sub(1).clamp(1, 3)
}

/// Columns one band occupies: its bars plus the gap after them.
fn spectrum_cell_w(width: usize) -> usize {
    spectrum_bar_w(width) + 1
}

/// Source band range for each display column when `SPECTRUM_BANDS` has to fit
/// into `cols` columns.
///
/// A window too narrow for all 31 bands gets a coarser spectrum, never a
/// truncated one: dropping the top columns would silently cut the treble off
/// the display and misreport what is playing. Each column takes the peak of
/// its group, the same way the character spectrogram folds bands into octaves.
fn spectrum_columns(cols: usize) -> Vec<(usize, usize)> {
    let cols = cols.clamp(1, SPECTRUM_BANDS);
    (0..cols)
        .map(|i| {
            let lo = i * SPECTRUM_BANDS / cols;
            let hi = ((i + 1) * SPECTRUM_BANDS / cols).max(lo + 1);
            (lo, hi.min(SPECTRUM_BANDS))
        })
        .collect()
}

/// The band whose colour and centre frequency represent a group.
fn spectrum_group_src(group: (usize, usize)) -> usize {
    let (lo, hi) = group;
    (lo + (hi - lo) / 2).min(SPECTRUM_BANDS - 1)
}

/// Peak of a band group, and the source band that represents it.
fn spectrum_group(spec: &[f32; SPECTRUM_BANDS], group: (usize, usize)) -> (f32, usize) {
    let (lo, hi) = group;
    let peak = spec[lo..hi].iter().copied().fold(0.0f32, f32::max);
    (peak, spectrum_group_src(group))
}

/// Short label for a band centre, in at most three columns and never
/// ambiguous: `fmt_gutter_hz` rounds 1000, 1250 and 1600 all to "1k", which
/// would print the same label over three different bins.
fn fmt_band_hz(f: f32) -> String {
    let hz = f.round() as i64;
    if hz < 1000 {
        return format!("{hz}");
    }
    if hz >= 10_000 {
        return format!("{}k", hz / 1000);
    }
    let tenth = (hz % 1000) / 100;
    if tenth == 0 { format!("{}k", hz / 1000) } else { format!("{}k{}", hz / 1000, tenth) }
}

/// How many spectrum columns fit in `width`, given the per-band cell width.
fn spectrum_col_count(width: usize, cw: usize) -> usize {
    (width.saturating_sub(2) / cw.max(1)).clamp(1, SPECTRUM_BANDS)
}

/// Left margin that centres the band block in the window.
///
/// Cell width moves in whole columns, so 31 bands rarely divide the window
/// exactly — at 90 columns they occupy 64. Centring spends the remainder on
/// both margins instead of leaving a ragged gap down the right-hand side.
/// Never less than the frame's own two-column indent.
fn spectrum_indent(width: usize, cols: usize, cw: usize) -> usize {
    let used = cols * cw;
    2 + width.saturating_sub(2 + used) / 2
}

/// A frequency legend row for the spectrum bins.
///
/// 31 labels never fit, so this walks the columns and keeps a label only when
/// it clears the previous one — the same thinning the analysis spectrogram's
/// octave ladder uses. Labels name the band actually under them, taken from the
/// same `ISO_CENTERS` table that defines the bins, so they cannot drift.
fn spectrum_legend_row(groups: &[(usize, usize)], cw: usize, indent: usize,
                       vp: &VizPalette) -> String {
    let width = groups.len() * cw;
    let mut cells = vec![' '; width];
    let mut next_free = 0usize;
    for (i, &g) in groups.iter().enumerate() {
        let text = fmt_band_hz(ISO_CENTERS[spectrum_group_src(g)]);
        let col = i * cw;
        if col < next_free || col + text.len() > width {
            continue;
        }
        for (k, ch) in text.chars().enumerate() {
            cells[col + k] = ch;
        }
        // One clear column between labels, or they read as one number.
        next_free = col + text.len() + 1;
    }
    let mut line = " ".repeat(indent);
    line.push_str(vp.dim);
    line.extend(cells);
    line.push_str(vp.reset);
    line
}

/// One spectrum cell: `cw - 1` glyphs, then the gap column.
fn spectrum_cell(color: &str, ch: char, cw: usize) -> String {
    let mut s = String::with_capacity(color.len() + cw);
    s.push_str(color);
    for _ in 1..cw { s.push(ch); }
    s.push(' ');
    s
}

/// Rows the viz body occupies given the rows the frame can spare. Modes that
/// scale clamp into their own range; the rest ignore the budget and keep their
/// fixed height. Callers add one for the separator line above the block —
/// `get_viz_line_count` is that total.
pub fn viz_body_rows(mode: VizMode, avail: usize, fullscreen: bool, extras: bool) -> usize {
    match mode {
        VizMode::None => 0,
        // No lower bound: `avail` is what the window actually has, and
        // returning more than that reserves rows past the terminal's bottom.
        // Zero means draw nothing.
        _ => avail.min(viz_max_rows(mode, fullscreen, extras)),
    }
}

/// Ceiling for a scaled viz body.
///
/// A resize is responsive on its own: give a mode more room and it grows in
/// steps, without any key press. Full window lifts the ceiling so the mode can
/// take the screen; at normal size it stops well short, or a tall terminal
/// would hand the whole frame to the visualization and push everything else
/// out. The normal VU ceiling is deliberately enough for triple-height bars.
fn viz_max_rows(mode: VizMode, fullscreen: bool, extras: bool) -> usize {
    match mode {
        // Without the history strip the meter has nothing to do with extra
        // rows, so it stops asking for them rather than reserving blanks.
        // The meter's own structure sets this: two bars at the odd thickness
        // cap, the gap between them, and the scale. History on must never make
        // the block SMALLER than history off, or the toggle shrinks the meter.
        VizMode::VuMeter => match (extras, fullscreen) {
            (false, _) => VU_BARS_ROWS,
            (true, false) => VU_BARS_ROWS + 6,
            (true, true) => VU_MAX_ROWS,
        },
        // Everything else follows one policy: a normal frame grows to about
        // twice its natural height — visibly responsive without swallowing the
        // window — and full window runs to whatever the DATA supports.
        VizMode::SpectrumHorizontal => if fullscreen { SPECTRUM_MAX_ROWS } else { 12 },
        VizMode::SpectrumVertical => if fullscreen { SPECTRUM_MAX_ROWS } else { 16 },
        VizMode::Oscilloscope => if fullscreen { OSCILLOSCOPE_MAX_ROWS } else { 16 },
        VizMode::Lissajous => if fullscreen { LISSAJOUS_MAX_ROWS } else { 16 },
        // One row per band is the real ceiling: past 31 rows there are no more
        // ⅓-octave bands left to separate.
        VizMode::Spectrogram => if fullscreen { SPECTRUM_BANDS } else { 20 },
        // The pixel image gains the most from height — more pixel rows per
        // octave is what makes pitch readable off it — but each row is more
        // Sixel to encode per hop, so its normal ceiling stays put.
        VizMode::SpectrogramAnalysis =>
            if fullscreen { SPECTRO_ANALYSIS_MAX_ROWS } else { SPECTRO_ANALYSIS_ROWS },
        _ => 0,
    }
}



// --- Oscilloscope -----------------------------------------------------------

const OSCILLOSCOPE_MAX_ROWS: usize = 40;
/// Height at which the trace splits into one per channel. Below this there are
/// too few braille sub-rows to tell two traces apart, so the mono sum is the
/// honest display; above it the stereo difference is information the mono sum
/// throws away.
const OSCILLOSCOPE_DUAL_ROWS: usize = 10;
/// Widest trace worth drawing: the renderer samples two sub-columns per cell,
/// so this is exactly the waveform buffer's depth — past it the same sample
/// would be plotted twice.
const OSCILLOSCOPE_MAX_COLS: usize = WAVEFORM_BUF_SIZE / 2;

// Bit offsets within a braille cell for dot (px, py) where px∈0..2, py∈0..4.
const BRAILLE_BITS: [[u32; 4]; 2] = [
    [0x01, 0x02, 0x04, 0x40],
    [0x08, 0x10, 0x20, 0x80],
];

/// See `render_oscilloscope_body`; this enforces the width half of the renderer contract
/// (`fit_width`).
pub fn render_oscilloscope(analyser: &VizAnalyser, style: VizStyle, width: usize, rows: usize) -> Vec<String> {
    fit_width(render_oscilloscope_body(analyser, style, width, rows), width)
}

fn render_oscilloscope_body(analyser: &VizAnalyser, style: VizStyle, width: usize, rows: usize) -> Vec<String> {
    // Fill the terminal width (2-space pad + 2-col safety margin), with a sane cap.
    let cols = width.saturating_sub(4).clamp(8, OSCILLOSCOPE_MAX_COLS);
    if rows == 0 {
        return Vec::new();
    }
    // Both axes are latent: braille packs 2x4 sub-cells and the waveform buffer
    // holds far more samples than any width shows, so height is pure resolution
    // — rows x 4 vertical steps in Dots, rows x 2 in Bars.
    match style {
        VizStyle::Dots => render_oscilloscope_dots(analyser, cols, rows),
        VizStyle::Bars => render_oscilloscope_bars(analyser, cols, rows),
    }
}

fn render_oscilloscope_bars(analyser: &VizAnalyser, cols: usize, rows: usize) -> Vec<String> {
    let buf = &analyser.waveform_buf;
    // 2× horizontal resolution via quadrant blocks: sample at 2× cell width.
    let sub_cols = cols * 2;
    let sub_rows: usize = rows * 2;
    let mut col_values = vec![0.0f32; sub_cols];
    if !buf.is_empty() {
        let n = buf.len();
        for x in 0..sub_cols {
            let idx = x * (n - 1) / sub_cols.max(1);
            let (l, r) = buf[idx];
            col_values[x] = ((l + r) * 0.5).clamp(-1.0, 1.0);
        }
    }
    // Mark filled sub-cells (2 sub-cols × 2 sub-rows per terminal cell).
    let mid_sub = sub_rows as f32 / 2.0;
    let mut sub_grid = vec![false; sub_rows * sub_cols];
    for (x, &v) in col_values.iter().enumerate() {
        let wave_sub = mid_sub - v * mid_sub;
        let (lo, hi) = if wave_sub < mid_sub { (wave_sub, mid_sub) } else { (mid_sub, wave_sub) };
        let lo_i = lo.floor() as usize;
        let hi_i = (hi.ceil() as usize).min(sub_rows);
        for sy in lo_i..hi_i {
            sub_grid[sy * sub_cols + x] = true;
        }
    }
    // Quadrant block lookup indexed by (TL, TR, BL, BR) packed as a 4-bit nibble.
    const QUAD: [char; 16] = [
        ' ', '▘', '▝', '▀',  // 0000 0001 0010 0011
        '▖', '▌', '▞', '▛',  // 0100 0101 0110 0111
        '▗', '▚', '▐', '▜',  // 1000 1001 1010 1011
        '▄', '▙', '▟', '█',  // 1100 1101 1110 1111
    ];
    let mut lines = Vec::with_capacity(rows);
    for cy in 0..rows {
        let from_edge = cy.min(rows - 1 - cy);
        let color = match from_edge {
            0 => C_RED,
            1 => C_YELLOW,
            _ => C_GREEN,
        };
        let mut line = String::from("  ");
        line.push_str(color);
        let top_row = cy * 2;
        let bot_row = cy * 2 + 1;
        for cx in 0..cols {
            let lx = cx * 2;
            let rx = cx * 2 + 1;
            let tl = sub_grid[top_row * sub_cols + lx] as u8;
            let tr = sub_grid[top_row * sub_cols + rx] as u8;
            let bl = sub_grid[bot_row * sub_cols + lx] as u8;
            let br = sub_grid[bot_row * sub_cols + rx] as u8;
            let idx = (tl) | (tr << 1) | (bl << 2) | (br << 3);
            line.push(QUAD[idx as usize]);
        }
        line.push_str(C_RESET);
        lines.push(line);
    }
    lines
}

fn render_oscilloscope_dots(analyser: &VizAnalyser, cols: usize, rows: usize) -> Vec<String> {
    let buf = &analyser.waveform_buf;
    let dots_w = cols * 2; // braille 2 dots/cell
    let dots_h = rows * 4; // braille 4 dots/cell
    // Two traces, one per channel. Below the dual-trace height there isn't the
    // vertical resolution to tell them apart, so they collapse to the mono sum
    // and the display is what it always was.
    let dual = rows >= OSCILLOSCOPE_DUAL_ROWS;
    let mut grid_l = vec![0u32; dots_w * dots_h];
    let mut grid_r = vec![0u32; dots_w * dots_h];

    if !buf.is_empty() {
        let n = buf.len();
        let mid = (dots_h / 2) as i32;
        let trace = |grid: &mut [u32], pick: &dyn Fn(f32, f32) -> f32| {
            let mut prev_y: Option<i32> = None;
            for x in 0..dots_w {
                // Map column to sample index (newest on right).
                let idx = x * (n - 1) / dots_w.max(1);
                let (l, r) = buf[idx];
                let v = pick(l, r);
                let y = mid - (v.clamp(-1.0, 1.0) * mid as f32) as i32;
                let y = y.clamp(0, (dots_h - 1) as i32);
                // Connect the previous sample's y so the trace is continuous.
                let y0 = prev_y.unwrap_or(y);
                let (lo, hi) = if y0 < y { (y0, y) } else { (y, y0) };
                for yi in lo..=hi {
                    let (gx, gy) = (x, yi as usize);
                    if gx < dots_w && gy < dots_h {
                        grid[gy * dots_w + gx] = 1;
                    }
                }
                prev_y = Some(y);
            }
        };
        if dual {
            trace(&mut grid_l, &|l, _| l);
            trace(&mut grid_r, &|_, r| r);
        } else {
            trace(&mut grid_l, &|l, r| (l + r) * 0.5);
        }
    }

    // Render grid row-by-row. Color by distance from center (green → yellow → red).
    let mut lines = Vec::with_capacity(rows);
    for cy in 0..rows {
        let mut line = String::from("  ");
        let mut last_color = "";
        for cx in 0..cols {
            let (mut bits, mut bits_l, mut bits_r) = (0u32, 0u32, 0u32);
            for py in 0..4 {
                for px in 0..2 {
                    let gx = cx * 2 + px;
                    let gy = cy * 4 + py;
                    let bit = BRAILLE_BITS[px][py];
                    if grid_l[gy * dots_w + gx] != 0 {
                        bits |= bit;
                        bits_l |= bit;
                    }
                    if grid_r[gy * dots_w + gx] != 0 {
                        bits |= bit;
                        bits_r |= bit;
                    }
                }
            }
            let color = if dual {
                // Stereo width read straight off the colour: cells where both
                // traces pass are mono content, one-sided cells are the
                // difference between the channels.
                match (bits_l != 0, bits_r != 0) {
                    (true, true) => C_GREEN,
                    (true, false) => C_CYAN,
                    (false, true) => C_MAGENTA,
                    (false, false) => C_GREEN,
                }
            } else {
                // Colour by row — rows near the edges are louder, so redder.
                match cy.min(rows - 1 - cy) {
                    0 => C_RED,
                    1 => C_YELLOW,
                    _ => C_GREEN,
                }
            };
            if color != last_color {
                line.push_str(color);
                last_color = color;
            }
            let ch = char::from_u32(0x2800 + bits).unwrap_or(' ');
            line.push(ch);
        }
        line.push_str(C_RESET);
        lines.push(line);
    }
    lines
}

// --- Lissajous / Vectorscope ------------------------------------------------

const LISSAJOUS_MAX_ROWS: usize = 40;
/// Columns the side panel needs before it is worth drawing.
const LISSAJOUS_PANEL_W: usize = 12;

/// Inter-channel correlation and balance over the waveform buffer.
///
/// Correlation is the normalised dot product: +1 mono, 0 uncorrelated, -1 the
/// channels cancelling — the number a vectorscope exists to show, and the one
/// thing its picture makes you estimate by eye. Balance is the signed share of
/// energy, negative to the left.
fn lissajous_stats(buf: &std::collections::VecDeque<(f32, f32)>) -> (f32, f32) {
    let (mut lr, mut ll, mut rr) = (0.0f64, 0.0f64, 0.0f64);
    for &(l, r) in buf.iter() {
        lr += (l * r) as f64;
        ll += (l * l) as f64;
        rr += (r * r) as f64;
    }
    let denom = (ll * rr).sqrt();
    let corr = if denom > 1e-12 { (lr / denom) as f32 } else { 0.0 };
    let total = ll + rr;
    let bal = if total > 1e-12 { ((rr - ll) / total) as f32 } else { 0.0 };
    (corr.clamp(-1.0, 1.0), bal.clamp(-1.0, 1.0))
}
/// Terminal cells are roughly 1:2, so a square box needs twice as many columns
/// as rows. A vectorscope that is not square lies about phase.
const LISSAJOUS_ASPECT: usize = 2;

/// See `render_lissajous_body`; this enforces the width half of the renderer contract
/// (`fit_width`).
pub fn render_lissajous(analyser: &VizAnalyser, style: VizStyle, width: usize, rows: usize) -> Vec<String> {
    fit_width(render_lissajous_body(analyser, style, width, rows), width)
}

fn render_lissajous_body(analyser: &VizAnalyser, style: VizStyle, width: usize, rows: usize) -> Vec<String> {
    // Unlike every other mode the two axes are ONE knob: whichever of height or
    // width runs out first sets the box, and the leftover width stays margin
    // rather than being stretched into.
    let avail_cols = width.saturating_sub(4);
    let side = rows.min(avail_cols / LISSAJOUS_ASPECT);
    if side == 0 {
        return vec![String::new(); rows];
    }
    let cols = side * LISSAJOUS_ASPECT;
    let pad = (width.saturating_sub(cols) / 2).max(2);
    let mut lines = match style {
        VizStyle::Dots => render_lissajous_dots(analyser, pad, side, cols),
        VizStyle::Bars => render_lissajous_bars(analyser, pad, side, cols),
    };
    // A square box in a wide window leaves two margins that must not be
    // stretched into. Spend the right-hand one on the numbers a vectorscope is
    // read FOR — correlation and channel balance — which is the honest use of
    // space that cannot hold more scatter.
    let right = width.saturating_sub(pad + cols);
    if right >= LISSAJOUS_PANEL_W && lines.len() >= 3 {
        let (corr, bal) = lissajous_stats(&analyser.waveform_buf);
        let panel = [
            format!("corr {corr:+.2}"),
            match bal {
                b if b.abs() < 0.02 => "bal  ctr".to_string(),
                b if b < 0.0 => format!("bal  L{:.0}%", -b * 100.0),
                b => format!("bal  R{:.0}%", b * 100.0),
            },
            // Correlation near -1 means the channels cancel in mono.
            if corr < -0.2 { "MONO RISK".to_string() } else { String::new() },
        ];
        let top = lines.len().saturating_sub(panel.len()) / 2;
        for (i, text) in panel.iter().enumerate() {
            if text.is_empty() {
                continue;
            }
            if let Some(line) = lines.get_mut(top + i) {
                line.push_str(&format!("  {C_DIM}{text}{C_RESET}"));
            }
        }
    }
    // The box is square, the budget may not be: pad the remainder so the frame
    // still gets exactly the rows it accounted for.
    while lines.len() < rows {
        lines.push(String::new());
    }
    lines.truncate(rows);
    lines
}

fn render_lissajous_bars(analyser: &VizAnalyser, pad: usize, rows: usize, cols: usize) -> Vec<String> {
    let buf = &analyser.waveform_buf;
    let mut counts = vec![0u32; cols * rows];
    let inv_sqrt2 = std::f32::consts::FRAC_1_SQRT_2;
    let w_half = cols as f32 / 2.0;
    let h_half = rows as f32 / 2.0;
    for &(l, r) in buf.iter() {
        let side = (l - r) * inv_sqrt2;
        let mid = (l + r) * inv_sqrt2;
        let x = (w_half + side.clamp(-1.0, 1.0) * (w_half - 0.5)) as i32;
        let y = (h_half - mid.clamp(-1.0, 1.0) * (h_half - 0.5)) as i32;
        if x >= 0 && (x as usize) < cols && y >= 0 && (y as usize) < rows {
            counts[y as usize * cols + x as usize] += 1;
        }
    }
    let max = counts.iter().copied().max().unwrap_or(1).max(1) as f32;

    let mut lines = Vec::with_capacity(rows);
    for cy in 0..rows {
        let mut line = " ".repeat(pad);
        line.push_str(C_CYAN);
        for cx in 0..cols {
            let f = counts[cy * cols + cx] as f32 / max;
            let ch = if f == 0.0 { ' ' }
                else if f < 0.25 { '░' }
                else if f < 0.5  { '▒' }
                else if f < 0.75 { '▓' }
                else { '█' };
            line.push(ch);
        }
        line.push_str(C_RESET);
        lines.push(line);
    }
    lines
}

fn render_lissajous_dots(analyser: &VizAnalyser, pad: usize, rows: usize, cols: usize) -> Vec<String> {
    let (dots_w, dots_h) = (cols * 2, rows * 4);
    let buf = &analyser.waveform_buf;
    let mut grid = vec![0u32; dots_w * dots_h];

    // Rotated 45° (mid/side): mono signals appear as a vertical line.
    // X = side = (L - R) / sqrt(2); Y = mid = (L + R) / sqrt(2). Terminal Y grows down.
    let inv_sqrt2 = std::f32::consts::FRAC_1_SQRT_2;
    let w_half = (dots_w / 2) as f32;
    let h_half = (dots_h / 2) as f32;
    for &(l, r) in buf.iter() {
        let side = (l - r) * inv_sqrt2;
        let mid = (l + r) * inv_sqrt2;
        let x = (w_half + side.clamp(-1.0, 1.0) * (w_half - 1.0)) as i32;
        let y = (h_half - mid.clamp(-1.0, 1.0) * (h_half - 1.0)) as i32;
        if x >= 0 && (x as usize) < dots_w && y >= 0 && (y as usize) < dots_h {
            grid[y as usize * dots_w + x as usize] = 1;
        }
    }

    let mut lines = Vec::with_capacity(rows);
    for cy in 0..rows {
        let mut line = " ".repeat(pad);
        line.push_str(C_CYAN);
        for cx in 0..cols {
            let mut bits: u32 = 0;
            for py in 0..4 {
                for px in 0..2 {
                    let gx = cx * 2 + px;
                    let gy = cy * 4 + py;
                    if grid[gy * dots_w + gx] != 0 {
                        bits |= BRAILLE_BITS[px][py];
                    }
                }
            }
            let ch = char::from_u32(0x2800 + bits).unwrap_or(' ');
            line.push(ch);
        }
        line.push_str(C_RESET);
        lines.push(line);
    }
    lines
}

// --- Spectrogram ------------------------------------------------------------

// One octave per row: ISO ⅓-octave = 3 bands/octave, 31 bands ≈ 10 octaves.
const SPECTROGRAM_ROWS: usize = 10;

// 31 bands → 10 octave rows (top = highest freq). Colors mirror BAND_COLORS by region.
const SPECTROGRAM_ROW_COLORS: [&str; SPECTROGRAM_ROWS] = [
    C_MAGENTA, C_RED, C_RED, C_YELLOW, C_YELLOW,
    C_YELLOW, C_GREEN, C_GREEN, C_GREEN, C_CYAN,
];

// 9-level braille fill, one extra dot per step so each magnitude maps to a
// visibly distinct glyph (the shared SPECTRUM_H_BRAILLE table has duplicates).
const SPECTROGRAM_DOTS: &[char] = &[' ', '⡀', '⣀', '⣄', '⣤', '⣦', '⣶', '⣷', '⣿'];

// Spectrogram contrast window: the magnitude slice [FLOOR, CEIL] is mapped across
// the full glyph height. Bands are dB-scaled into [0,1] over 90 dB and music only
// occupies a narrow part of that, so mapping the whole [0,1] bunches everything
// mid-scale (~3 dots) no matter the gain — a window spreads the relevant range so
// the rows actually move. Below FLOOR = empty; at/above CEIL = full height.
const SPECTROGRAM_FLOOR: f32 = 0.30; // raise to darken / drop weak bands
const SPECTROGRAM_CEIL: f32 = 0.62;  // lower to make peaks reach full height sooner

/// See `render_spectrogram_body`; this enforces the width half of the renderer contract
/// (`fit_width`).
pub fn render_spectrogram(analyser: &VizAnalyser, style: VizStyle, width: usize, rows: usize) -> Vec<String> {
    fit_width(render_spectrogram_body(analyser, style, width, rows), width)
}

fn render_spectrogram_body(analyser: &VizAnalyser, style: VizStyle, width: usize, rows: usize) -> Vec<String> {
    if rows == 0 {
        return Vec::new();
    }
    let hist = &analyser.spectrogram_history;
    // Fill the terminal width (2-space pad + 2-col safety margin), capped at history.
    let cols = width.saturating_sub(4).clamp(8, SPECTROGRAM_COLS);
    let chars: &[char] = match style {
        VizStyle::Bars => SPECTRUM_H_CHARS,
        VizStyle::Dots => SPECTROGRAM_DOTS,
    };
    // Width of the contrast window (guard against a zero/inverted span).
    let span = (SPECTROGRAM_CEIL - SPECTROGRAM_FLOOR).max(1e-3);

    // Group 31 bands into 10 octave rows, top-to-bottom = highest-to-lowest freq.
    // One octave (3 ⅓-octave bands) per row; the top row absorbs the spare 20 kHz
    // band. Row i pulls the max over its group for snappier high-freq response.
    // Rows are latent detail, not decoration: 31 ⅓-octave bands fold into
    // whatever height there is, one octave per row at the natural 10 and
    // approaching one BAND per row as the block grows. Top row = highest
    // frequency, so the groups run backwards down the band list.
    let band_groups: Vec<(usize, usize)> = (0..rows)
        .map(|r| {
            let from_bottom = rows - 1 - r;
            let lo = from_bottom * SPECTRUM_BANDS / rows;
            let hi = ((from_bottom + 1) * SPECTRUM_BANDS / rows).max(lo + 1);
            (lo, hi.min(SPECTRUM_BANDS))
        })
        .collect();

    let mut lines = Vec::with_capacity(rows);
    for (row, &(lo, hi)) in band_groups.iter().enumerate() {
        let mut line = String::from("  ");
        // The colour ramp is a fixed table read by position, not by index, so
        // it stretches over any number of rows instead of running off its end.
        let color = SPECTROGRAM_ROW_COLORS[row * SPECTROGRAM_ROW_COLORS.len() / rows];
        line.push_str(color);
        // Show the newest `cols` columns: oldest on the left, newest on the right.
        // Pad with spaces when history hasn't filled the visible width yet.
        let n = hist.len().min(cols);
        let start = hist.len() - n;
        let pad = cols - n;
        for _ in 0..pad {
            line.push(' ');
        }
        for col in start..hist.len() {
            let frame = &hist[col];
            let mut v: f32 = 0.0;
            for b in lo..hi {
                v = v.max(frame[b]);
            }
            // Linear within the [FLOOR, CEIL] window (i.e. linear in dB, the standard
            // sonogram mapping): below FLOOR → empty, at/above CEIL → full height.
            let v_norm = ((v - SPECTROGRAM_FLOOR) / span).clamp(0.0, 1.0);
            let idx = ((v_norm * 8.0) as usize).min(8);
            line.push(chars[idx]);
        }
        line.push_str(C_RESET);
        lines.push(line);
    }
    lines
}

// --- SpectrogramAnalysis helpers --------------------------------------------

/// Fill `rgb` (resized to width_px·height_px·3, reused across frames) with the
/// analysis-spectrogram image: x = time (newest right), y = frequency (linear or
/// log), pixel = colormap(dB in contrast window). `log_axis` selects the freq map.
fn analysis_levels_into(out: &mut Vec<u8>, analyser: &VizAnalyser, width_px: usize, height_px: usize, log_axis: bool, logical_cols: usize, left_px: usize) {
    out.clear();
    out.resize(width_px * height_px, 0);
    let hist = &analyser.spectro_raw_history;
    let nbins = hist.back().map(|c| c.len()).unwrap_or(0);
    if nbins == 0 {
        return;
    }
    let n = hist.len();
    let row_bin: Vec<usize> = (0..height_px)
        .map(|y| if log_axis {
            analysis_row_to_bin_log(y, height_px, nbins, analyser.sample_rate as f32)
        } else {
            analysis_row_to_bin_linear(y, height_px, nbins)
        })
        .collect();
    // Pixel columns map through a fixed timeline of `logical_cols` slots
    // (newest at the right) so the pixel width is decoupled from the history
    // depth. Kitty renders 1 px per slot; Sixel images are wider than the
    // history is deep and stretch each slot across several pixels (mapping
    // 1:1 instead leaves everything left of the last 512 px permanently
    // black); half-block passes logical_cols == width_px, keeping its 1:1
    // most-recent-hops window. Unfilled slots stay black (fill-from-right).
    let filled = n.min(logical_cols);
    let blank_slots = logical_cols - filled;
    let oldest_shown = n - filled;
    // The leftmost `left_px` columns are reserved for the frequency legend and
    // stay at the floor; the timeline is mapped across what remains.
    let content_w = width_px.saturating_sub(left_px);
    for x in 0..content_w {
        let slot = x * logical_cols / content_w.max(1);
        if slot < blank_slots { continue; }
        let col = &hist[oldest_shown + (slot - blank_slots)];
        for (y, &bin) in row_bin.iter().enumerate() {
            let db = col.get(bin).copied().unwrap_or(SPECTRO_ANALYSIS_FLOOR_DB);
            let t = analysis_intensity(db, SPECTRO_ANALYSIS_FLOOR_DB, SPECTRO_ANALYSIS_CEIL_DB);
            out[y * width_px + left_px + x] = (t * 255.0).round() as u8;
        }
    }
}

/// Expand quantized intensity levels to truecolor via the colormap (Kitty PNG
/// and half-block paths; the sixel path maps levels into a fixed palette).
fn colorize_levels(levels: &[u8], rgb: &mut Vec<u8>) {
    rgb.clear();
    rgb.reserve(levels.len() * 3);
    for &lv in levels {
        let (r, g, b) = analysis_colormap(lv as f32 / 255.0);
        rgb.extend_from_slice(&[r, g, b]);
    }
}

/// 128-entry fixed palette for the indexed sixel path: the colormap sampled
/// uniformly, so intensity `level >> 1` is exactly the palette index. A fixed
/// palette keeps unchanged pixels byte-identical between emissions — the
/// per-frame re-quantization it replaces made the scrolling image shimmer.
/// Palette slot for the in-image legend: one past the 128 colormap entries,
/// so `level >> 1` can never collide with it.
const LEGEND_IDX: u8 = 128;
const LEGEND_RGB: (u8, u8, u8) = (190, 190, 190);

fn analysis_sixel_palette() -> &'static [(u8, u8, u8)] {
    static PALETTE: OnceLock<Vec<(u8, u8, u8)>> = OnceLock::new();
    // 128 colormap entries (index = level >> 1) plus one for the in-image
    // frequency legend, which must not be any shade the spectrogram can paint.
    PALETTE.get_or_init(|| {
        let mut p: Vec<(u8, u8, u8)> =
            (0..128).map(|i| analysis_colormap(i as f32 / 127.0)).collect();
        p.push(LEGEND_RGB);
        p
    })
}

/// See `render_spectrogram_analysis_body`. A window too narrow to hold the
/// image (or the half-block grid) gets blank rows: a picture cannot be cut to
/// width the way text can — a Kitty/Sixel image placed wider than the window
/// overflows it whatever the text around it says.
pub fn render_spectrogram_analysis(analyser: &VizAnalyser, width: usize, log_axis: bool, paused: bool, rows: usize, force: bool) -> Vec<String> {
    let lines = render_spectrogram_analysis_body(analyser, width, log_axis, paused, rows, force);
    if lines.iter().any(|l| crate::ansi::visible_len(l) > width) {
        return vec![String::new(); rows];
    }
    lines
}

fn render_spectrogram_analysis_body(analyser: &VizAnalyser, width: usize, log_axis: bool, paused: bool, rows: usize, force: bool) -> Vec<String> {
    use std::cell::RefCell;
    thread_local! {
        // Reused across frames so the per-frame image buffers aren't reallocated each tick.
        static RGB_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        static LEVELS_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
        // Paused-frame cache: while paused the image is frozen, so reuse the last
        // rendered lines (keyed by column generation + geometry) to skip the encode.
        static CACHE: RefCell<(u64, usize, bool, usize, Vec<String>)> =
            const { RefCell::new((u64::MAX, 0, false, 0, Vec::new())) };
        // Key of the sixel block last emitted to the terminal. Sixel pixels
        // persist as cell content, so an unchanged frame can be skipped
        // entirely — no re-encode, no re-transmission through ConPTY.
        static LAST_EMIT: RefCell<(u64, usize, bool, usize)> =
            const { RefCell::new((u64::MAX, 0, false, 0)) };
    }
    let sixel = analysis_needs_raw_lines();
    let protocol = crate::cover::detect_protocol();
    // Frequency legend. Both image protocols draw it INTO the image: cells and
    // pixels are different coordinate systems, and a legend snapped to cell
    // rows clumps badly — roughly 1.1 rows per octave means integer rounding
    // bunches C1/C2/C3 together and then skips a row. In pixel space an octave
    // is ~15 px and every label lands on its own frequency. Only half-block,
    // which really is cells, takes the legend as prefix text.
    let (gutter, gutter_w) = if protocol_draws_image_legend(protocol) {
        (Vec::new(), 0)
    } else {
        analysis_gutter(width, rows, log_axis, analyser.sample_rate as f32)
    };
    let cols = analysis_cols_for(width, gutter_w);
    let gen = analyser.spectro_last_gen;
    let key = (gen, width, log_axis, rows);

    // Sixel emit-on-change: the image only changes when a new hop lands
    // (gen bump) or the geometry/axis changes. Re-encoding + re-sending the
    // blob 20×/s through ConPTY saturates Windows Terminal's CPU-side sixel
    // pipeline and makes the scroll cadence jerky. Passive empty lines move
    // the cursor over the block without touching its cells.
    if analysis_can_skip_emit(sixel, force, key, LAST_EMIT.with(|c| *c.borrow())) {
        return vec![String::new(); rows];
    }

    // When paused the content can't change, so reuse the cached render and skip the
    // per-frame image build + PNG re-encode entirely.
    if paused {
        let hit = CACHE.with(|c| {
            let c = c.borrow();
            if c.0 == gen && c.1 == width && c.2 == log_axis && c.3 == rows && !c.4.is_empty() {
                Some(c.4.clone())
            } else {
                None
            }
        });
        if let Some(lines) = hit {
            if sixel {
                LAST_EMIT.with(|c| *c.borrow_mut() = key);
            }
            return lines;
        }
    }

    // Protocol-dependent geometry: Some((w, h)) renders a pixel image that big,
    // None falls back to half-block truecolor (text, so it redraws cleanly and
    // fills the width). Per-protocol rationale lives in analysis_image_geometry.
    let image_geom = analysis_image_geometry(protocol, cols, rows);
    // Half-block: 1 px/col, 2 px-rows/char row, sized directly to the cell area.
    let (w, h) = image_geom.unwrap_or((cols, rows * 2));
    let lines = LEVELS_BUF.with(|lcell| {
        let mut lguard = lcell.borrow_mut();
        let levels: &mut Vec<u8> = &mut lguard;
        // Image paths map pixels through a logical timeline of history slots;
        // half-block keeps its 1:1 most-recent-hops window. Kitty stretches
        // the full 512-hop history (terminal scales smoothly). Sixel shows
        // the most recent w/k hops at exactly k px each (w is a multiple of
        // k, see analysis_image_geometry) — uniform hop widths, no shimmer.
        // Sixel reserves a pixel strip on the left for the legend, then trims
        // what remains to a whole number of px-per-hop so scrolling features
        // keep a uniform width (see analysis_image_geometry). The trim's
        // remainder is absorbed into the strip, which is only background.
        let scale = legend_scale(h, rows);
        let strip = if image_geom.is_some() {
            legend_strip_w(h, log_axis, analyser.sample_rate as f32, scale)
        } else {
            0
        };
        let (left_px, logical_cols) = if image_geom.is_some() && sixel {
            // Sixel is 1:1 pixels, so what remains after the strip is trimmed
            // to a whole number of px-per-hop (uniform hop widths, no shimmer);
            // the trim remainder is absorbed into the strip, which is only
            // background. Kitty scales its image, so it needs no such trim.
            let content = w.saturating_sub(strip);
            let k = content.div_ceil(SPECTRO_ANALYSIS_COLS).max(1);
            let content = (content / k) * k;
            (w - content, content / k)
        } else if image_geom.is_some() {
            (strip, SPECTRO_ANALYSIS_COLS)
        } else {
            (0, w)
        };
        analysis_levels_into(levels, analyser, w, h, log_axis, logical_cols, left_px);
        let mut lines = if image_geom.is_some() && sixel {
            // Fixed-palette indexed sixel — no quantizer, no shimmer.
            for lv in levels.iter_mut() { *lv >>= 1; }
            // Painted into the same pixels as the spectrogram, so the two share
            // one coordinate space and cannot drift apart.
            let mut plot = |x: usize, y: usize| {
                if x < w && y < h {
                    levels[y * w + x] = LEGEND_IDX;
                }
            };
            draw_pixel_legend(
                &mut plot, h, left_px, log_axis, analyser.sample_rate as f32, scale,
            );
            crate::cover::render_viz_sixel_indexed(
                levels, analysis_sixel_palette(), w, h, cols as u32, rows as u32,
                crate::cover::Gutter::NONE,
            )
        } else {
            RGB_BUF.with(|cell| {
                let mut guard = cell.borrow_mut();
                let rgb: &mut Vec<u8> = &mut guard;
                colorize_levels(levels, rgb);
                if image_geom.is_some() {
                    // Drawn after colorising: the legend is a flat colour, not
                    // a colormap entry, so it must not go through the ramp.
                    let mut plot = |x: usize, y: usize| {
                        if x < w && y < h {
                            let i = (y * w + x) * 3;
                            rgb[i] = LEGEND_RGB.0;
                            rgb[i + 1] = LEGEND_RGB.1;
                            rgb[i + 2] = LEGEND_RGB.2;
                        }
                    };
                    draw_pixel_legend(
                        &mut plot, h, left_px, log_axis, analyser.sample_rate as f32, scale,
                    );
                    crate::cover::render_image_block(rgb.as_slice(), w as u32, h as u32, cols as u32, rows as u32)
                } else {
                    crate::cover::render_half_block_public(w as u32, h as u32, rgb.as_slice())
                }
            })
        };
        // Sixel weaves the gutter in itself — its self-erase pass would wipe a
        // plain prefix off row 0 (see cover::viz_sixel_lines). Kitty places the
        // image at the cursor and half-block is ordinary text, so both just
        // take the legend as a prefix.
        if gutter_w > 0 && !(image_geom.is_some() && sixel) {
            for (line, g) in lines.iter_mut().zip(gutter.iter()) {
                line.insert_str(0, g);
            }
        }
        for line in lines.iter_mut() { line.insert_str(0, "  "); }
        lines
    });

    if paused {
        CACHE.with(|c| *c.borrow_mut() = (gen, width, log_axis, rows, lines.clone()));
    }
    if sixel {
        LAST_EMIT.with(|c| *c.borrow_mut() = key);
    }
    lines
}

/// Whether this frame's sixel analysis block can be left untouched on screen
/// (no re-encode, no re-transmission). Only valid when the protocol is Sixel
/// (cell content persists untouched), nothing repainted over the block
/// (`force`), and the content/geometry key is unchanged since the last
/// emission. Kitty/half-block always re-emit (cheap; id-replaced or text).
fn analysis_can_skip_emit(
    sixel: bool,
    force: bool,
    key: (u64, usize, bool, usize),
    last: (u64, usize, bool, usize),
) -> bool {
    sixel && !force && key == last
}


/// Blank rows to put ABOVE a viz block that stops short of its budget, so it
/// sits centred in the space instead of pinned to the top.
///
/// Only meaningful in full window: a normal frame has chrome directly below the
/// block, so there is no free space to centre within. Modes that stop short are
/// the ones with a shape of their own — the VU meter with its history off, a
/// square vectorscope in a tall window.
pub fn viz_top_pad(avail: usize, body: usize, fullscreen: bool) -> usize {
    if fullscreen { avail.saturating_sub(body) / 2 } else { 0 }
}

/// Rows the frame can spare for a viz body: what the window has left after the
/// content above it and whatever must stay visible below.
///
/// No floor. A minimum enforced when the window has no room for it reserves
/// rows past the terminal's own bottom, and a Sixel image painted there
/// auto-scrolls the status line away on every frame. Zero means no room, and
/// the caller draws nothing.
pub fn viz_rows_available(term_h: usize, rows_above: usize, below: usize) -> usize {
    term_h.saturating_sub(rows_above + below)
}

/// Whether the analysis-spectrogram lines must be printed WITHOUT the usual
/// per-line erase-to-EOL. Sixel pixels are ordinary cell content: the row-1
/// transmit paints the whole block downward, so erasing rows 2..N afterwards
/// wipes the image to a 1-row strip. The sixel first line erases the block
/// itself before painting (see cover::viz_sixel_lines).
pub fn analysis_needs_raw_lines() -> bool {
    matches!(
        crate::cover::detect_protocol(),
        crate::cover::GraphicsProtocol::Sixel
    )
}

/// Decide the analysis-spectrogram render geometry for a graphics protocol:
/// `Some((w, h))` = render the image that big; `None` = half-block fallback.
/// Sixel sizing uses the probed terminal cell metrics when available
/// (pixel-exact block fill), else the conservative 8×16 px floor.
/// Whether this protocol renders a pixel image, and so carries the frequency
/// legend INSIDE it rather than as prefix text. Must agree with
/// `analysis_image_geometry_with` returning `Some` — a mismatch either strands
/// the legend outside the image or reserves cells nothing draws into.
fn protocol_draws_image_legend(protocol: crate::cover::GraphicsProtocol) -> bool {
    use crate::cover::GraphicsProtocol as GP;
    matches!(protocol, GP::Kitty | GP::Sixel)
}

/// Assumed terminal cell size in px for Sixel sizing: small enough that the
/// image cannot spill past its reserved rows on any real terminal.
pub(crate) const SIXEL_CELL_FLOOR: (usize, usize) = (8, 16);

fn analysis_image_geometry(
    protocol: crate::cover::GraphicsProtocol,
    cols: usize,
    rows: usize,
) -> Option<(usize, usize)> {
    // A conservative floor, never queried. Sixel must not overflow its block
    // (auto-scroll storm), and underfilling is only cosmetic now that the
    // frequency legend is drawn inside the image rather than in cells beside
    // it — nothing depends on knowing the terminal's real cell size.
    analysis_image_geometry_with(protocol, cols, rows, SIXEL_CELL_FLOOR)
}

/// Pure core of `analysis_image_geometry`, parameterized on the cell size.
fn analysis_image_geometry_with(
    protocol: crate::cover::GraphicsProtocol,
    cols: usize,
    rows: usize,
    cell: (usize, usize),
) -> Option<(usize, usize)> {
    use crate::cover::GraphicsProtocol as GP;
    match protocol {
        // Kitty scales the image to the cell box and its id-addressed images
        // survive the per-frame cursor-up redraw as a separate layer: render
        // one pixel column per stored hop (history depth) + oversampled height.
        // Kitty scales the image into its cell box, so the image is sized to
        // that box using the SAME assumed cell as Sixel. Rendering a fixed 512
        // px width instead made the terminal stretch it much further
        // horizontally than vertically, and the legend glyphs came out wide
        // and coarse. Matching the cell aspect keeps the terminal's scale
        // factor equal in both axes, whatever the real cell size is. The full
        // hop history is still stretched across the width (logical_cols stays
        // at SPECTRO_ANALYSIS_COLS), so depth is unchanged.
        GP::Kitty => Some((cols * cell.0, rows * cell.1)),
        // Sixel renders 1:1 pixels with no scaling. `cell` is the probed cell
        // size (pixel-exact fill) or the conservative 8×16 floor when the
        // CSI 16 t probe got no answer. Overflowing the reserved block is
        // catastrophic — an image whose bottom edge passes the screen bottom
        // triggers sixel auto-scroll EVERY frame (status line marches up
        // forever) — while underfilling just leaves blank cells at the
        // block's bottom/right. The per-frame erase/repaint cycle is hidden
        // by the DEC 2026 synchronized-update wrap (main.rs).
        //
        // Width is then trimmed to a multiple of the pixels-per-hop k so every
        // displayed hop is exactly k px wide. With a fractional ratio some
        // hops render 1px and some 2px in a fixed screen pattern, and
        // scrolling features alternate fat/thin — visible shimmer. Uniform
        // hops make motion a rigid k-px translation of identical bytes.
        GP::Sixel => {
            let raw_w = cols * cell.0;
            let k = raw_w.div_ceil(SPECTRO_ANALYSIS_COLS).max(1);
            Some((((raw_w / k) * k).max(k), rows * cell.1))
        }
        // iTerm2 images are cell content the redraw's erase wipes (flicker),
        // and OSC 1337 has no in-place replacement. Half-block fallback.
        GP::Iterm2 | GP::HalfBlock => None,
    }
}

/// Map a dB value to [0,1] across the contrast window [floor, ceil].
fn analysis_intensity(db: f32, floor_db: f32, ceil_db: f32) -> f32 {
    let span = (ceil_db - floor_db).max(1e-3);
    ((db - floor_db) / span).clamp(0.0, 1.0)
}

/// Map a display row (0 = top = highest freq) to an FFT bin index, linear in Hz.
fn analysis_row_to_bin_linear(row: usize, rows: usize, nbins: usize) -> usize {
    if rows <= 1 || nbins == 0 { return 0; }
    let frac = (rows - 1 - row) as f32 / (rows - 1) as f32; // 0 at bottom, 1 at top
    ((frac * (nbins - 1) as f32).round() as usize).min(nbins - 1)
}

/// 3x5 bitmaps for the characters the frequency legend uses, one byte per
/// glyph row with bit 2 as the leftmost pixel. Small, but it is drawn INTO the
/// spectrogram image rather than printed as terminal text, which is the whole
/// point: pixels and cells are different coordinate systems, and a legend in
/// cells can only line up with a pixel image if the terminal's cell size is
/// known. In the image it shares one coordinate space and cannot drift.
const GLYPH_W: usize = 3;
const GLYPH_H: usize = 5;

fn glyph(c: char) -> Option<[u8; GLYPH_H]> {
    Some(match c {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b010, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        'C' => [0b111, 0b100, 0b100, 0b100, 0b111],
        'k' => [0b100, 0b101, 0b110, 0b101, 0b101],
        _ => return None,
    })
}

/// Width in pixels of `text` rendered at `scale`, including inter-glyph gaps.
fn label_px_w(text: &str, scale: usize) -> usize {
    let n = text.chars().count();
    if n == 0 { return 0; }
    (n * GLYPH_W + n - 1) * scale
}

/// Blit `text` at (x, y) = top-left by calling `plot` for each lit pixel.
/// Indexed (Sixel) and truecolor (Kitty) images differ only in how one pixel
/// is stored, so the glyph rasterising is shared and each caller supplies a
/// plotter that clips to its own bounds.
fn draw_label(plot: &mut impl FnMut(usize, usize), x: usize, y: usize, text: &str, scale: usize) {
    let mut pen = x;
    for ch in text.chars() {
        let Some(rows) = glyph(ch) else { continue };
        for (gy, bits) in rows.iter().enumerate() {
            for gx in 0..GLYPH_W {
                if bits & (1 << (GLYPH_W - 1 - gx)) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        plot(pen + gx * scale + sx, y + gy * scale + sy);
                    }
                }
            }
        }
        pen += (GLYPH_W + 1) * scale;
    }
}

/// Pixel row (0 = top) where `f` Hz lands on an image `h` pixels tall — the
/// exact inverse of the row/frequency mapping the spectrogram itself uses.
fn freq_to_pixel_row(f: f32, h: usize, log_axis: bool, sample_rate: f32) -> usize {
    if h <= 1 {
        return 0;
    }
    let nyquist = sample_rate * 0.5;
    if nyquist <= 0.0 {
        return h - 1;
    }
    let frac = if log_axis {
        if f <= SPECTRO_LOG_F_MIN {
            0.0
        } else {
            (f / SPECTRO_LOG_F_MIN).ln() / (nyquist / SPECTRO_LOG_F_MIN).ln()
        }
    } else {
        f / nyquist
    };
    let y = (1.0 - frac.clamp(0.0, 1.0)) * (h - 1) as f32;
    (y.round() as usize).min(h - 1)
}

/// Glyph scale for an image `h` px tall covering `rows` cells: big enough to
/// read, never so tall that consecutive octaves overlap.
fn legend_scale(h: usize, rows: usize) -> usize {
    let px_per_row = if rows == 0 { GLYPH_H + 2 } else { h / rows.max(1) };
    (px_per_row / (GLYPH_H + 2)).clamp(1, 3)
}

/// Legend ticks as (pixel row, label), thinned so labels cannot overlap.
/// Built bottom-up: on a log axis the low octaves are the ones worth keeping
/// when there isn't room for all of them.
fn pixel_legend(h: usize, log_axis: bool, sample_rate: f32, scale: usize) -> Vec<(usize, String)> {
    let glyph_h = GLYPH_H * scale;
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut last: Option<usize> = None;
    for (f, text) in gutter_ladder(log_axis, sample_rate) {
        let y = freq_to_pixel_row(f, h, log_axis, sample_rate);
        if last.is_some_and(|p| p.abs_diff(y) < glyph_h + 1) {
            continue;
        }
        last = Some(y);
        out.push((y, text));
    }
    out
}

/// Width in pixels the legend strip needs: widest label, a gap, and the tick.
fn legend_strip_w(h: usize, log_axis: bool, sample_rate: f32, scale: usize) -> usize {
    let widest = pixel_legend(h, log_axis, sample_rate, scale)
        .iter()
        .map(|(_, t)| label_px_w(t, scale))
        .max()
        .unwrap_or(0);
    if widest == 0 { 0 } else { widest + 2 * scale + scale }
}

/// Draw the frequency legend into the left `strip_w` px of an indexed image.
/// Labels are centred on their tick, which sits at the exact pixel row that
/// frequency occupies in the spectrogram beside it.
fn draw_pixel_legend(plot: &mut impl FnMut(usize, usize), h: usize, strip_w: usize,
                     log_axis: bool, sample_rate: f32, scale: usize) {
    if strip_w == 0 {
        return;
    }
    let (glyph_h, tick) = (GLYPH_H * scale, 2 * scale);
    for (y, text) in pixel_legend(h, log_axis, sample_rate, scale) {
        let top = y.saturating_sub(glyph_h / 2).min(h.saturating_sub(glyph_h));
        draw_label(plot, 0, top, &text, scale);
        for tx in strip_w.saturating_sub(tick)..strip_w {
            plot(tx, y);
        }
    }
}

/// Bottom of the log frequency axis. Below this the FFT has no resolution to
/// speak of and the rows would all collapse onto bin 0.
const SPECTRO_LOG_F_MIN: f32 = 30.0;

/// Map a display row to an FFT bin, logarithmic in Hz over [F_MIN, Nyquist].
fn analysis_row_to_bin_log(row: usize, rows: usize, nbins: usize, sample_rate: f32) -> usize {
    if rows <= 1 || nbins == 0 { return 0; }
    let bin_hz = (sample_rate * 0.5) / (nbins - 1).max(1) as f32;
    let f = analysis_row_freq(row, rows, true, sample_rate);
    ((f / bin_hz).round() as usize).min(nbins - 1)
}

// --- frequency gutter --------------------------------------------------------
//
// A vertical axis legend down the left edge of the analysis spectrogram. The
// image rows carry frequency, so the labels have to sit on the row the renderer
// actually drew that frequency on: `analysis_row_freq` is the exact inverse of
// the two row->bin maps above, pinned by a test that round-trips both.
//
// Which ladder gets used follows the axis, because the axes differ in kind, not
// just scale. Log spacing makes octaves evenly spaced, so it carries note names
// (C1, C2, ... - what the {B} "Dots" axis is *for*). Linear spacing crushes
// every musical pitch into the bottom rows, so it carries plain Hz instead.

/// Narrowest the gutter ever gets: a 3-char label plus the rule glyph. Every
/// label fits 3 chars at ordinary rates; only the linear ladder above 100 kHz
/// (192/384 kHz hardware) needs a fourth, and then the field widens to suit.
const GUTTER_W: usize = 4;
/// Terminal width below which the gutter is dropped, so narrow windows spend
/// their columns on the image. Matches the Minimal theme's panel breakpoints.
const GUTTER_MIN_TERM_W: usize = 60;

/// Image cells at this terminal width, once the 2-cell left indent, the right
/// margin and the frequency gutter are taken out. The block must never exceed
/// the window: a sixel image overflowing its reserved cells triggers auto-scroll
/// on every frame, so the gutter narrows the image rather than pushing it out.
fn analysis_cols_for(term_w: usize, gutter_w: usize) -> usize {
    term_w.saturating_sub(4 + gutter_w).clamp(8, 320)
}

/// Frequency drawn on display row `row` (0 = top = highest). Exact inverse of
/// `analysis_row_to_bin_linear` / `analysis_row_to_bin_log`.
fn analysis_row_freq(row: usize, rows: usize, log_axis: bool, sample_rate: f32) -> f32 {
    let nyquist = sample_rate * 0.5;
    if rows <= 1 { return nyquist; }
    let frac = (rows - 1 - row) as f32 / (rows - 1) as f32; // 0 bottom, 1 top
    if log_axis {
        SPECTRO_LOG_F_MIN * (nyquist / SPECTRO_LOG_F_MIN).powf(frac)
    } else {
        frac * nyquist
    }
}

/// Frequency of C in octave `n`, scientific pitch notation (A4 = 440 Hz).
/// C4 is middle C at 261.63 Hz; MIDI note number for Cn is 12*(n+1).
fn c_octave_hz(n: i32) -> f32 {
    440.0 * 2f32.powf(((12 * (n + 1)) as f32 - 69.0) / 12.0)
}

/// Round `target` up or down to the nearest 1/2/5 x 10^n value, so a linear
/// ladder lands on readable numbers instead of 4800 Hz steps.
fn nice_step(target: f32) -> f32 {
    if target <= 0.0 { return 1.0; }
    let base = 10f32.powf(target.log10().floor());
    let m = target / base;
    let mult = if m <= 1.5 { 1.0 } else if m <= 3.5 { 2.0 } else if m <= 7.5 { 5.0 } else { 10.0 };
    mult * base
}

/// Format a frequency for the gutter, 3 chars max: "500", "2k", "20k".
fn fmt_gutter_hz(f: f32) -> String {
    let hz = f.round() as i64;
    if hz < 1000 { format!("{hz}") } else { format!("{}k", hz / 1000) }
}

/// Candidate `(frequency, label)` marks for the axis, ascending.
fn gutter_ladder(log_axis: bool, sample_rate: f32) -> Vec<(f32, String)> {
    let nyquist = sample_rate * 0.5;
    if nyquist < SPECTRO_LOG_F_MIN {
        return Vec::new();
    }
    if log_axis {
        // Octave anchors. C0 (16.35 Hz) sits below the axis floor, so the
        // ladder naturally starts at C1 and runs until it passes Nyquist.
        (0..12)
            .map(|n| (c_octave_hz(n), format!("C{n}")))
            .filter(|(f, _)| *f >= SPECTRO_LOG_F_MIN && *f <= nyquist)
            .collect()
    } else {
        // Aim for roughly six marks across the axis, snapped to round numbers.
        let step = nice_step(nyquist / 5.0);
        let mut out = Vec::new();
        let mut f = 0.0;
        while f <= nyquist {
            out.push((f, fmt_gutter_hz(f)));
            f += step;
        }
        out
    }
}

/// Place the ladder's marks on rows: one label per row, each on the row whose
/// drawn frequency is nearest. A mark whose row is already taken is dropped
/// rather than overwriting - at 16 rows the ladder can out-number the rows.
fn gutter_labels(rows: usize, log_axis: bool, sample_rate: f32) -> Vec<Option<String>> {
    let mut out = vec![None; rows];
    if rows == 0 {
        return out;
    }
    // Distance in the axis's own metric, so "nearest" means nearest as drawn.
    let pos = |f: f32| if log_axis { f.max(1e-6).log2() } else { f };
    for (f, text) in gutter_ladder(log_axis, sample_rate) {
        let target = pos(f);
        let best = (0..rows).min_by(|&a, &b| {
            let da = (pos(analysis_row_freq(a, rows, log_axis, sample_rate)) - target).abs();
            let db = (pos(analysis_row_freq(b, rows, log_axis, sample_rate)) - target).abs();
            da.total_cmp(&db)
        });
        if let Some(row) = best {
            if out[row].is_none() {
                out[row] = Some(text);
            }
        }
    }
    out
}

/// Build the gutter for a block: one dim string per row, plus the visible cell
/// width they all share. Returns `(vec![], 0)` when the terminal is too narrow
/// to spend the columns. The label field sizes itself to its widest member so
/// a 4-char label (150k, on 384 kHz hardware) widens the column instead of
/// overflowing it; the width is returned rather than assumed because the image
/// geometry and the sixel cursor arithmetic both have to agree with it.
///
/// Labelled rows get a tick on the rule so the eye can follow the label across.
fn analysis_gutter(
    term_w: usize,
    rows: usize,
    log_axis: bool,
    sample_rate: f32,
) -> (Vec<String>, usize) {
    if term_w < GUTTER_MIN_TERM_W {
        return (Vec::new(), 0);
    }
    let labels = gutter_labels(rows, log_axis, sample_rate);
    let label_w = labels
        .iter()
        .flatten()
        .map(|t| t.chars().count())
        .max()
        .unwrap_or(0)
        .max(GUTTER_W - 1);
    let lines = labels
        .into_iter()
        .map(|label| match label {
            Some(text) => format!("{C_DIM}{text:>label_w$}\u{2524}{C_RESET}"),
            None => format!("{C_DIM}{:label_w$}\u{2502}{C_RESET}", ""),
        })
        .collect();
    (lines, label_w + 1)
}

/// Perceptual "magma"-ish ramp: black -> purple -> red -> orange -> yellow -> white.
fn analysis_colormap(t: f32) -> (u8, u8, u8) {
    const STOPS: [(f32, f32, f32, f32); 6] = [
        (0.0,   0.0,   0.0,   0.0),
        (0.2,  40.0,  11.0,  84.0),
        (0.4, 121.0,  28.0, 109.0),
        (0.6, 190.0,  54.0,  66.0),
        (0.8, 240.0, 134.0,  29.0),
        (1.0, 252.0, 253.0, 191.0),
    ];
    let t = t.clamp(0.0, 1.0);
    let mut i = 0;
    while i + 1 < STOPS.len() && t > STOPS[i + 1].0 {
        i += 1;
    }
    let (t0, r0, g0, b0) = STOPS[i];
    let (t1, r1, g1, b1) = STOPS[(i + 1).min(STOPS.len() - 1)];
    let f = if (t1 - t0).abs() < 1e-6 { 0.0 } else { (t - t0) / (t1 - t0) };
    let lerp = |a: f32, b: f32| (a + (b - a) * f).round().clamp(0.0, 255.0) as u8;
    (lerp(r0, r1), lerp(g0, g1), lerp(b0, b1))
}

#[cfg(test)]
mod analysis_tests {
    use super::*;

    /// Every scaled mode, both styles, swept across window sizes the way a drag
    /// resizes one — one row and one column at a time. Two invariants hold at
    /// every step: the renderer returns EXACTLY the rows it was given (the
    /// frame's line accounting is derived from this, and a renderer that
    /// returns a different count silently shifts everything below it), and no
    /// line is wider than the window (a wrapped line costs a row the layout
    /// never budgeted for, which is what scrolls the frame).
    #[test]
    fn scaled_viz_fill_their_row_budget_at_every_window_size() {
        use crate::ansi::visible_len;
        let state = PlayerState::new();
        let analyser = VizAnalyser::new(48000);
        let modes = [
            VizMode::VuMeter,
            VizMode::SpectrumHorizontal,
            VizMode::SpectrumVertical,
            VizMode::Oscilloscope,
            VizMode::Lissajous,
            VizMode::Spectrogram,
        ];
        for mode in modes {
            for style in [VizStyle::Dots, VizStyle::Bars] {
              for extras in [false, true] {
                // Every width from 1 up to 20 (a drag through a tiny window —
                // the sweep used to start at 20, which hid renderers with a
                // hard minimum width), then a spread of wider ones.
                for term_w in (1usize..=20).chain([40, 62, 80, 100, 140, 200, 320]) {
                    // Step through every height a drag would pass through.
                    for term_h in 0..48usize {
                        let rows = viz_body_rows(mode, term_h, true, extras);
                        let lines = match mode {
                            VizMode::VuMeter => render_vu_meter(&state, style, term_w, rows, extras),
                            VizMode::SpectrumHorizontal =>
                                render_spectrum_horizontal(&state, style, term_w, rows, extras),
                            VizMode::SpectrumVertical =>
                                render_spectrum_vertical(&state, style, term_w, rows, extras),
                            VizMode::Oscilloscope =>
                                render_oscilloscope(&analyser, style, term_w, rows),
                            VizMode::Lissajous =>
                                render_lissajous(&analyser, style, term_w, rows),
                            VizMode::Spectrogram =>
                                render_spectrogram(&analyser, style, term_w, rows),
                            _ => unreachable!(),
                        };
                        assert_eq!(
                            lines.len(), rows,
                            "{mode:?}/{style:?} extras={extras} w={term_w} budget={term_h}: \
                             got {} lines for {rows} rows",
                            lines.len()
                        );
                        for (i, line) in lines.iter().enumerate() {
                            assert!(
                                visible_len(line) <= term_w,
                                "{mode:?}/{style:?} extras={extras} w={term_w} rows={rows} \
                                 line {i} is {} wide",
                                visible_len(line)
                            );
                        }
                    }
                }
              }
            }
        }
    }

    #[test]
    fn a_scaled_viz_never_claims_rows_the_window_does_not_have() {
        // The same trap as the analysis block's old 4-row floor: a minimum
        // applied to a window with no room reserves lines off-screen, and the
        // frame scrolls on every repaint.
        for term_h in 0..60usize {
            for above in 0..40usize {
                let avail = viz_rows_available(term_h, above, 3);
                for mode in [VizMode::VuMeter, VizMode::SpectrumHorizontal,
                             VizMode::SpectrumVertical, VizMode::Oscilloscope,
                             VizMode::Lissajous, VizMode::Spectrogram,
                             VizMode::SpectrogramAnalysis] {
                    {
                        let body = viz_body_rows(mode, avail, true, true);
                        assert!(body <= avail, "{mode:?} claimed {body} of {avail}");
                        assert!(
                            body + above + 3 <= term_h || body == 0,
                            "{mode:?} term_h={term_h} above={above} body={body} overflows"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn analysis_block_fills_its_row_budget_across_window_sizes() {
        // Same contract as the cell-based modes, on a coarser grid: this one
        // encodes an image per call, so sweeping every size costs half a minute.
        use crate::ansi::visible_len;
        let analyser = VizAnalyser::new(48000);
        for term_w in [1usize, 4, 8, 11, 24, 60, 120, 240] {
            for rows in [0usize, 1, 2, 5, 9, 16, 24, 32] {
                let budget = viz_body_rows(VizMode::SpectrogramAnalysis, rows, true, false);
                for log in [true, false] {
                    let lines = render_spectrogram_analysis(
                        &analyser, term_w, log, false, budget, true,
                    );
                    assert_eq!(
                        lines.len(), budget,
                        "w={term_w} budget={budget}: got {} lines", lines.len()
                    );
                    for (i, l) in lines.iter().enumerate() {
                        assert!(
                            visible_len(l) <= term_w,
                            "w={term_w} budget={budget} line {i} is {} wide", visible_len(l)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_band_gets_its_own_label_in_three_columns() {
        // A legend that prints "1k" over 1000, 1250 and 1600 is worse than no
        // legend: it names the wrong bin and looks deliberate.
        let mut seen = std::collections::HashSet::new();
        for &f in ISO_CENTERS.iter() {
            let text = fmt_band_hz(f);
            assert!(text.len() <= 3, "{f}Hz -> {text:?} is too wide");
            assert!(seen.insert(text.clone()), "duplicate label {text:?}");
        }
        assert_eq!(fmt_band_hz(20.0), "20");
        assert_eq!(fmt_band_hz(1000.0), "1k");
        assert_eq!(fmt_band_hz(1250.0), "1k2");
        assert_eq!(fmt_band_hz(6300.0), "6k3");
        assert_eq!(fmt_band_hz(16000.0), "16k");
    }

    #[test]
    fn the_db_scale_runs_loud_to_quiet_and_ends_at_full_scale() {
        for bar in [12usize, 24, 54, 120, 300] {
            let marks = vu_scale_marks(bar);
            assert!(!marks.is_empty(), "bar={bar} got no scale");
            // 0 dB is full scale, so it always survives the thinning and sits
            // hard against the right-hand end.
            let (col, text) = *marks.last().unwrap();
            assert_eq!(text, "0");
            assert_eq!(col, bar - 1, "bar={bar}: 0 dB must end the scale");
            // Quieter marks sit further left, and none overlap.
            for pair in marks.windows(2) {
                assert!(pair[0].0 + pair[0].1.len() < pair[1].0,
                        "bar={bar}: {:?} collides with {:?}", pair[0], pair[1]);
            }
            for (c, t) in &marks {
                assert!(c + t.len() <= bar, "bar={bar}: {t:?} runs off the end");
            }
        }
    }

    #[test]
    fn vectorscope_stats_name_what_the_picture_only_implies() {
        use std::collections::VecDeque;
        let mk = |f: &dyn Fn(usize) -> (f32, f32)| -> VecDeque<(f32, f32)> {
            (0..512).map(f).collect()
        };
        let sine = |i: usize| (i as f32 * 0.1).sin();
        // Identical channels: perfectly correlated, dead centre.
        let (c, b) = lissajous_stats(&mk(&|i| (sine(i), sine(i))));
        assert!((c - 1.0).abs() < 1e-3, "mono corr {c}");
        assert!(b.abs() < 1e-3, "mono bal {b}");
        // Inverted: cancels in mono, which is the thing worth warning about.
        let (c, _) = lissajous_stats(&mk(&|i| (sine(i), -sine(i))));
        assert!((c + 1.0).abs() < 1e-3, "out-of-phase corr {c}");
        // One channel only: hard to that side.
        let (_, b) = lissajous_stats(&mk(&|i| (sine(i), 0.0)));
        assert!((b + 1.0).abs() < 1e-3, "left-only bal {b}");
        let (_, b) = lissajous_stats(&mk(&|i| (0.0, sine(i))));
        assert!((b - 1.0).abs() < 1e-3, "right-only bal {b}");
        // Silence must not divide by zero.
        assert_eq!(lissajous_stats(&mk(&|_| (0.0, 0.0))), (0.0, 0.0));
        assert_eq!(lissajous_stats(&VecDeque::new()), (0.0, 0.0));
    }

    #[test]
    fn full_window_centres_a_block_that_stops_short() {
        for avail in 0..60usize {
            for mode in [VizMode::VuMeter, VizMode::Lissajous, VizMode::SpectrumVertical] {
                for extras in [false, true] {
                    let body = viz_body_rows(mode, avail, true, extras);
                    let pad = viz_top_pad(avail, body, true);
                    // Never claims more than the window has, and the leftover
                    // is split so the block sits in the middle of it.
                    assert!(pad + body <= avail, "{mode:?} avail={avail}");
                    let below = avail - pad - body;
                    assert!(below >= pad && below - pad <= 1,
                            "{mode:?} avail={avail}: {pad} above, {below} below");
                    // A normal frame has chrome under the block; nothing to centre.
                    assert_eq!(viz_top_pad(avail, body, false), 0);
                }
            }
        }
    }

    #[test]
    fn a_resize_grows_the_viz_and_full_window_lifts_the_ceiling() {
        for mode in [VizMode::VuMeter, VizMode::SpectrumHorizontal,
                     VizMode::SpectrumVertical, VizMode::Oscilloscope,
                     VizMode::Lissajous, VizMode::Spectrogram,
                     VizMode::SpectrogramAnalysis] {
            // Growth is responsive on its own: more room, more rows, no key.
            let mut prev = 0;
            for avail in 0..30usize {
                let rows = viz_body_rows(mode, avail, false, true);
                assert!(rows >= prev, "{mode:?} shrank as the window grew");
                assert!(rows <= avail, "{mode:?} claimed rows it wasn't given");
                prev = rows;
            }
            // A normal frame stops short so the rest of the UI survives a tall
            // terminal; full window lifts that ceiling.
            let normal = viz_body_rows(mode, 200, false, true);
            let full = viz_body_rows(mode, 200, true, true);
            assert_eq!(normal, viz_max_rows(mode, false, true));
            assert!(full > normal, "{mode:?}: full window must exceed normal");
            assert_eq!(viz_body_rows(mode, 0, true, true), 0);
        }
        assert_eq!(viz_body_rows(VizMode::None, 40, true, true), 0);
        assert_eq!(viz_body_rows(VizMode::VuMeter, 9999, true, true), VU_MAX_ROWS);
    }

    #[test]
    fn vu_spends_height_on_information_not_on_thicker_bars() {
        for style in [VizStyle::Dots, VizStyle::Bars] {
            for rows in 0..40usize {
                let lay = vu_layout(rows, style, true);
                assert_eq!(lay.total(), rows, "layout must account for every row (rows={rows})");
                assert!(lay.thickness <= VU_MAX_THICKNESS, "rows={rows}");
            }
            // Odd at every height, so `thickness / 2` is always a real middle.
            for rows in 0..40usize {
                let t = vu_layout(rows, style, true).thickness;
                assert!(t == 0 || !t.is_multiple_of(2), "rows={rows}: even thickness {t}");
            }
            // Past the thickness cap the extra rows go to the history strip,
            // which is the only part carrying information a single row cannot.
            let tall = vu_layout(24, style, true);
            assert_eq!(tall.thickness, VU_MAX_THICKNESS);
            assert!(tall.history > 0, "tall meter must show level history");
        }
    }

    #[test]
    fn spectrum_bands_widen_to_use_the_window() {
        // 31 bands is fixed by the state pipeline, so width goes into cell
        // width — but never below 2 (unreadable) or above 6 (a bar chart).
        // Bar weight steps in whole glyphs: 1, then 2, then 3.
        assert_eq!(spectrum_bar_w(0), 1);
        assert_eq!(spectrum_bar_w(64), 1);
        assert_eq!(spectrum_bar_w(120), 2);
        assert_eq!(spectrum_bar_w(200), 3);
        assert_eq!(spectrum_bar_w(4000), 3);
        for w in 0..400usize {
            assert!((1..=3).contains(&spectrum_bar_w(w)), "w={w}");
            assert_eq!(spectrum_cell_w(w), spectrum_bar_w(w) + 1, "w={w}");
        }
    }

    #[test]
    fn pixel_row_for_a_frequency_inverts_the_rows_own_mapping() {
        // The legend and the image must agree exactly; this is the property
        // that made a cell-based legend impossible without knowing cell size.
        let sr = 48000.0;
        for &log in &[true, false] {
            for h in [32usize, 144, 300] {
                for y in [0usize, 1, h / 3, h / 2, h - 2, h - 1] {
                    let f = analysis_row_freq(y, h, log, sr);
                    let back = freq_to_pixel_row(f, h, log, sr);
                    assert!(back.abs_diff(y) <= 1, "log={log} h={h} y={y} -> {f}Hz -> {back}");
                }
            }
        }
    }

    #[test]
    fn legend_ticks_sit_at_their_own_frequency_and_never_overlap() {
        let sr = 48000.0;
        for &log in &[true, false] {
            for h in [48usize, 144, 320] {
                let scale = legend_scale(h, 9);
                let marks = pixel_legend(h, log, sr, scale);
                for (y, text) in &marks {
                    assert!(*y < h, "tick off the image: {text} at {y} of {h}");
                }
                for pair in marks.windows(2) {
                    let gap = pair[1].0.abs_diff(pair[0].0);
                    assert!(gap >= GLYPH_H * scale, "labels collide: {pair:?} h={h}");
                }
            }
        }
    }

    #[test]
    fn legend_strip_leaves_room_for_its_widest_label() {
        let sr = 96000.0;
        for &log in &[true, false] {
            let h = 200;
            let scale = legend_scale(h, 10);
            let strip = legend_strip_w(h, log, sr, scale);
            for (_, text) in pixel_legend(h, log, sr, scale) {
                assert!(label_px_w(&text, scale) <= strip, "{text} wider than {strip}px");
            }
        }
    }

    #[test]
    fn legend_is_drawn_inside_its_strip_and_never_over_the_spectrogram() {
        let (w, h) = (200usize, 144usize);
        let scale = legend_scale(h, 9);
        let strip = legend_strip_w(h, true, 48000.0, scale);
        let mut buf = vec![0u8; w * h];
        draw_pixel_legend(&mut |x: usize, y: usize| { if x < w && y < h { buf[y * w + x] = 200; } },
                          h, strip, true, 48000.0, scale);
        assert!(buf.contains(&200), "legend drew nothing");
        for y in 0..h {
            for x in strip..w {
                assert_eq!(buf[y * w + x], 0, "legend bled into the image at ({x},{y})");
            }
        }
    }

    #[test]
    fn image_legend_protocols_match_the_ones_that_render_images() {
        use crate::cover::GraphicsProtocol as GP;
        // If these disagree the legend is either stranded outside the image or
        // cells are reserved that nothing draws into.
        for p in [GP::Kitty, GP::Sixel, GP::Iterm2, GP::HalfBlock] {
            assert_eq!(
                protocol_draws_image_legend(p),
                analysis_image_geometry_with(p, 120, 16, SIXEL_CELL_FLOOR).is_some(),
                "{p:?}"
            );
        }
    }

    #[test]
    fn octaves_are_evenly_spaced_in_pixels_unlike_snapped_cell_rows() {
        // The Kitty bug this fixes: at ~1.1 cell rows per octave the cell
        // gutter rounds several octaves onto adjacent rows and then skips one,
        // so labels clump (C1/C2/C3 together, gap, C4/C5...). In pixel space
        // every octave gets the same distance.
        let (h, sr) = (144usize, 48000.0f32);
        let scale = legend_scale(h, 9);
        let marks = pixel_legend(h, true, sr, scale);
        let gaps: Vec<usize> = marks.windows(2).map(|p| p[0].0.abs_diff(p[1].0)).collect();
        assert!(gaps.len() >= 8, "expected most octaves to be labelled, got {marks:?}");
        let (lo, hi) = (*gaps.iter().min().unwrap(), *gaps.iter().max().unwrap());
        assert!(hi - lo <= 1, "octave spacing varies by more than rounding: {gaps:?}");
    }

    #[test]
    fn label_glyphs_land_where_they_are_placed_and_clip_at_the_edges() {
        let (w, h) = (24usize, 8usize);
        let mut buf = vec![0u8; w * h];
        draw_label(&mut |x: usize, y: usize| { if x < w && y < h { buf[y * w + x] = 9; } },
                   1, 1, "1", 1);
        // '1' is [010,110,010,010,111]: row 0 of the glyph sets only the middle
        // column, which at x=1 scale=1 is pixel x=2.
        assert_eq!(buf[w + 2], 9, "glyph pixel set");
        assert_eq!(buf[w + 1], 0, "left column of that row stays clear");
        assert_eq!(buf[0], 0, "nothing drawn above the origin");

        // Running off the right edge must clip, never wrap to the next row.
        let mut edge = vec![0u8; w * h];
        draw_label(&mut |x: usize, y: usize| { if x < w && y < h { edge[y * w + x] = 7; } },
                   w - 1, 0, "8", 1);
        for y in 0..h {
            assert_eq!(edge[y * w], 0, "row {y} column 0 untouched by clipping");
        }
    }

    #[test]
    fn label_width_accounts_for_scale_and_gaps() {
        assert_eq!(label_px_w("", 2), 0);
        assert_eq!(label_px_w("C", 1), GLYPH_W);
        // Two glyphs plus one 1px gap, doubled.
        assert_eq!(label_px_w("C1", 2), (2 * GLYPH_W + 1) * 2);
        assert_eq!(label_px_w("10k", 1), 3 * GLYPH_W + 2);
    }

    #[test]
    fn every_legend_character_has_a_glyph() {
        // The ladders only ever emit digits, 'C' and 'k'; a missing glyph would
        // silently drop a character and misreport a frequency.
        for c in "0123456789Ck".chars() {
            assert!(glyph(c).is_some(), "no glyph for {c:?}");
        }
        for (_, text) in gutter_ladder(true, 48000.0) {
            assert!(text.chars().all(|c| glyph(c).is_some()), "log label {text:?}");
        }
        for (_, text) in gutter_ladder(false, 48000.0) {
            assert!(text.chars().all(|c| glyph(c).is_some()), "linear label {text:?}");
        }
    }

    #[test]
    fn intensity_maps_window_to_unit_range() {
        assert!((analysis_intensity(-70.0, -70.0, -10.0) - 0.0).abs() < 1e-6);
        assert!((analysis_intensity(-10.0, -70.0, -10.0) - 1.0).abs() < 1e-6);
        assert!((analysis_intensity(-40.0, -70.0, -10.0) - 0.5).abs() < 1e-6);
        assert_eq!(analysis_intensity(-90.0, -70.0, -10.0), 0.0);
        assert_eq!(analysis_intensity(0.0, -70.0, -10.0), 1.0);
    }

    #[test]
    fn analysis_row_budget_respects_what_sits_below() {
        // Window chosen so the SPECTRO_ANALYSIS_ROWS cap doesn't bind and the
        // reserve is what's actually being measured.
        // Default (3 rows reserved) is for a footer under the image.
        let with_footer = viz_body_rows(VizMode::SpectrogramAnalysis, viz_rows_available(24, 10, 3), false, false);
        // Minimal draws its tray above, so only the bottom-slack row is kept —
        // same window, same content above, but two more rows of image.
        let tray_above = viz_body_rows(VizMode::SpectrogramAnalysis, viz_rows_available(24, 10, 1), false, false);
        assert_eq!(with_footer, 24 - 10 - 3);
        assert_eq!(tray_above, 24 - 10 - 1);
        assert!(tray_above > with_footer);

        // The frame must still fit: rows_above + image + reserve <= term_h.
        for term_h in [12usize, 20, 24, 40, 60] {
            for rows_above in [5usize, 10, 18] {
                let r = viz_body_rows(VizMode::SpectrogramAnalysis,
                                      viz_rows_available(term_h, rows_above, 1), false, false);
                assert!(
                    rows_above + r < term_h || r == 0,
                    "term_h={term_h} above={rows_above} rows={r} overflows"
                );
            }
        }
        // Never grows past the cap however tall the window is.
        assert_eq!(viz_body_rows(VizMode::SpectrogramAnalysis, viz_rows_available(200, 0, 1), false, false),
                   viz_body_rows(VizMode::SpectrogramAnalysis, viz_rows_available(200, 0, 3), false, false));
    }

    #[test]
    fn linear_freq_map_spans_bins_endpoints() {
        let nbins = 2049;
        let h = 16;
        assert_eq!(analysis_row_to_bin_linear(0, h, nbins), nbins - 1);
        assert_eq!(analysis_row_to_bin_linear(h - 1, h, nbins), 0);
    }

    #[test]
    fn analysis_skip_allows_unchanged_sixel_frames() {
        let key = (7u64, 120usize, false, 14usize);
        assert!(analysis_can_skip_emit(true, false, key, key));
    }

    #[test]
    fn analysis_skip_never_when_forced_or_changed_or_not_sixel() {
        let key = (7u64, 120usize, false, 14usize);
        // Forced (full repaint / block painted over): must re-emit.
        assert!(!analysis_can_skip_emit(true, true, key, key));
        // New hop landed (gen bump): must re-emit.
        assert!(!analysis_can_skip_emit(true, false, (8, 120, false, 14), key));
        // Geometry changed: must re-emit.
        assert!(!analysis_can_skip_emit(true, false, (7, 120, false, 12), key));
        // Non-sixel protocols always re-emit.
        assert!(!analysis_can_skip_emit(false, false, key, key));
    }

    #[test]
    fn analysis_rows_keep_full_height_in_tall_windows() {
        assert_eq!(viz_body_rows(VizMode::SpectrogramAnalysis, viz_rows_available(50, 15, 3), false, false), SPECTRO_ANALYSIS_ROWS);
    }

    #[test]
    fn analysis_rows_shed_to_fit_short_windows() {
        // 32-row window, 15 rows above the viz block, 3 reserved (separator +
        // transient status + slack) → only 14 spectrogram rows fit.
        assert_eq!(viz_body_rows(VizMode::SpectrogramAnalysis, viz_rows_available(32, 15, 3), false, false), 14);
    }

    #[test]
    fn analysis_rows_yield_nothing_when_the_window_has_no_room() {
        // 15 rows of content above a 12-row window: there is no space at all,
        // and reserving a minimum anyway put the block past the screen bottom,
        // which on Sixel is the auto-scroll storm.
        assert_eq!(viz_body_rows(VizMode::SpectrogramAnalysis, viz_rows_available(12, 15, 3), false, false), 0);
        for term_h in 0..40usize {
            for above in 0..40usize {
                let r = viz_body_rows(VizMode::SpectrogramAnalysis,
                                      viz_rows_available(term_h, above, 3), false, false);
                assert!(r + above + 3 <= term_h || r == 0, "term_h={term_h} above={above} r={r}");
            }
        }
    }

    #[test]
    fn analysis_levels_stretch_history_across_wider_images() {
        // Sixel images are wider in pixels than the history is deep (512
        // hops); each logical slot must stretch across the width instead of
        // mapping 1:1 right-anchored, which left the image's left half
        // permanently black on Windows Terminal.
        let mut a = VizAnalyser::new(48000);
        a.spectro_raw_history.push_back(vec![-10.0]); // hot column (older)
        a.spectro_raw_history.push_back(vec![-70.0]); // floor column (newer)
        let mut lv = Vec::new();
        // 4 px wide, full history (n == logical == 2): each slot covers 2 px.
        analysis_levels_into(&mut lv, &a, 4, 1, false, 2, 0);
        assert_eq!(lv[0], lv[1], "first history column must cover px 0..2");
        assert_eq!(lv[2], lv[3], "second history column must cover px 2..4");
        assert_ne!(lv[0], lv[2]);
        assert_ne!(lv[0], 0, "no blank padding when history is full");
    }

    #[test]
    fn analysis_levels_keep_one_to_one_most_recent_window_for_half_block() {
        let mut a = VizAnalyser::new(48000);
        for i in 0..4 {
            let db = if i == 3 { -10.0 } else { -70.0 };
            a.spectro_raw_history.push_back(vec![db]);
        }
        let mut lv = Vec::new();
        // logical == width: a 2-px window shows the most recent 2 hops 1:1.
        analysis_levels_into(&mut lv, &a, 2, 1, false, 2, 0);
        assert_ne!(lv[0], lv[1]);
        assert_ne!(lv[1], 0, "newest (hot) hop lands at the right edge");
    }

    #[test]
    fn analysis_geometry_kitty_matches_the_cell_box_aspect() {
        use crate::cover::GraphicsProtocol as GP;
        // Sized to the assumed cell box, so the terminal's upscale is the same
        // in both axes; a fixed-width image stretched the legend glyphs wide.
        let (cw, ch) = SIXEL_CELL_FLOOR;
        assert_eq!(
            analysis_image_geometry(GP::Kitty, 120, 16),
            Some((120 * cw, 16 * ch))
        );
        // Whatever the real cell turns out to be, the scale factor the terminal
        // applies is uniform: image aspect == cell-box aspect.
        for (cols, rows) in [(40usize, 6usize), (120, 16), (300, 20)] {
            let (w, h) = analysis_image_geometry(GP::Kitty, cols, rows).unwrap();
            assert_eq!(w * rows * ch, h * cols * cw, "cols={cols} rows={rows}");
        }
    }

    #[test]
    fn analysis_geometry_sixel_undershoots_cell_box() {
        // Sixel renders 1:1 pixels with no scaling, and an image whose bottom
        // edge passes the screen bottom makes the terminal scroll — every
        // frame. Cell metrics are unknowable without querying, so size at a
        // conservative 8×16 px per cell: must underfill, never overflow.
        use crate::cover::GraphicsProtocol as GP;
        assert_eq!(
            analysis_image_geometry(GP::Sixel, 120, 16),
            Some((960, 256))
        );
    }

    #[test]
    fn analysis_geometry_sixel_keeps_uniform_hops_at_any_cell_size() {
        // With probed 9×19 px cells, the image fills the block exactly
        // (modulo the px-per-hop width trim) instead of the 8×16 floor.
        use crate::cover::GraphicsProtocol as GP;
        // raw_w = 120*9 = 1080, k = ceil(1080/512) = 3, trimmed = 1080.
        assert_eq!(
            analysis_image_geometry_with(GP::Sixel, 120, 14, (9, 19)),
            Some((1080, 14 * 19))
        );
        // Probed metrics must keep the uniform px-per-hop invariant.
        for cols in 8..=320 {
            let (w, _) = analysis_image_geometry_with(GP::Sixel, cols, 16, (9, 19)).unwrap();
            let k = w.div_ceil(SPECTRO_ANALYSIS_COLS).max(1);
            assert_eq!(w % k, 0, "cols={}: w={} not a multiple of k={}", cols, w, k);
        }
    }

    #[test]
    fn analysis_geometry_sixel_width_is_multiple_of_px_per_hop() {
        // Every displayed hop must be exactly k px wide: with a fractional
        // ratio (e.g. 1120px / 512 slots), some hops render 1px and some 2px
        // in a fixed screen pattern, so scrolling features alternate fat/thin
        // — visible shimmer. Width must be trimmed to a multiple of k.
        use crate::cover::GraphicsProtocol as GP;
        for cols in 8..=320 {
            let (w, _) = analysis_image_geometry(GP::Sixel, cols, 16).unwrap();
            let k = w.div_ceil(SPECTRO_ANALYSIS_COLS).max(1);
            assert_eq!(w % k, 0, "cols={}: w={} not a multiple of k={}", cols, w, k);
        }
    }

    #[test]
    fn analysis_geometry_iterm2_and_plain_use_half_block() {
        use crate::cover::GraphicsProtocol as GP;
        assert_eq!(analysis_image_geometry(GP::Iterm2, 120, 16), None);
        assert_eq!(analysis_image_geometry(GP::HalfBlock, 120, 16), None);
    }

    #[test]
    fn log_freq_map_is_monotonic_and_bounded() {
        let nbins = 2049;
        let h = 32;
        let sr = 44100.0;
        let mut prev = usize::MAX;
        for y in 0..h {
            let b = analysis_row_to_bin_log(y, h, nbins, sr);
            assert!(b < nbins);
            if prev != usize::MAX { assert!(b <= prev); }
            prev = b;
        }
    }

    #[test]
    fn colormap_dark_at_zero_bright_at_one() {
        let lo = analysis_colormap(0.0);
        let hi = analysis_colormap(1.0);
        let sum = |c: (u8, u8, u8)| c.0 as u32 + c.1 as u32 + c.2 as u32;
        assert!(sum(lo) < 60);
        assert!(sum(hi) > 600);
        let _ = analysis_colormap(-1.0);
        let _ = analysis_colormap(2.0);
    }

    // --- frequency gutter -------------------------------------------------

    #[test]
    fn row_freq_spans_the_axis_endpoints() {
        let sr = 48000.0;
        // Bottom row is the axis floor, top row is Nyquist, on both axes.
        assert!((analysis_row_freq(15, 16, true, sr) - SPECTRO_LOG_F_MIN).abs() < 0.01);
        assert!((analysis_row_freq(0, 16, true, sr) - 24000.0).abs() < 1.0);
        assert!((analysis_row_freq(15, 16, false, sr) - 0.0).abs() < 0.01);
        assert!((analysis_row_freq(0, 16, false, sr) - 24000.0).abs() < 1.0);
    }

    #[test]
    fn row_freq_rises_from_bottom_row_to_top_row() {
        for log_axis in [true, false] {
            let mut prev = f32::INFINITY;
            for row in 0..16 {
                let f = analysis_row_freq(row, 16, log_axis, 48000.0);
                assert!(f < prev, "row {row} freq {f} not below row {}", row - 1);
                prev = f;
            }
        }
    }

    #[test]
    fn row_freq_agrees_with_the_bin_map_it_inverts() {
        // The gutter labels a row with the frequency the image actually drew
        // there, so the inverse must land on the same bin the renderer picked.
        let (nbins, rows, sr) = (2049usize, 16usize, 48000.0f32);
        let bin_hz = (sr * 0.5) / (nbins - 1) as f32;
        for row in 0..rows {
            for log_axis in [true, false] {
                let f = analysis_row_freq(row, rows, log_axis, sr);
                let bin = if log_axis {
                    analysis_row_to_bin_log(row, rows, nbins, sr)
                } else {
                    analysis_row_to_bin_linear(row, rows, nbins)
                };
                assert_eq!(bin, ((f / bin_hz).round() as usize).min(nbins - 1));
            }
        }
    }

    #[test]
    fn c_octaves_match_scientific_pitch() {
        assert!((c_octave_hz(4) - 261.626).abs() < 0.01);
        assert!((c_octave_hz(0) - 16.352).abs() < 0.01);
        assert!((c_octave_hz(8) - 4186.01).abs() < 0.1);
    }

    #[test]
    fn log_axis_gutter_labels_octaves_bottom_up() {
        let labels = gutter_labels(16, true, 48000.0);
        assert_eq!(labels.len(), 16);
        let placed: Vec<(usize, String)> = labels
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.clone().map(|t| (i, t)))
            .collect();
        assert!(placed.len() >= 6, "expected several octave marks, got {placed:?}");
        // Every label is a C octave, and octave number falls as the row index
        // rises (row 0 = top = highest frequency).
        let mut prev_octave = i32::MAX;
        for (_, text) in &placed {
            let n: i32 = text.trim_start_matches('C').parse().expect("Cn label");
            assert!(n < prev_octave, "octaves out of order: {placed:?}");
            prev_octave = n;
        }
        // C4 (middle C, 262 Hz) must be somewhere on a 30 Hz..24 kHz axis.
        assert!(placed.iter().any(|(_, t)| t == "C4"), "{placed:?}");
    }

    #[test]
    fn linear_axis_gutter_labels_hz_not_notes() {
        let labels = gutter_labels(16, false, 48000.0);
        let placed: Vec<String> = labels.iter().flatten().cloned().collect();
        assert!(placed.len() >= 4, "{placed:?}");
        assert!(placed.iter().all(|t| !t.starts_with('C')), "{placed:?}");
        // Bottom row is DC on the linear axis.
        assert_eq!(labels[15].as_deref(), Some("0"));
        assert!(placed.iter().any(|t| t.ends_with('k')), "{placed:?}");
    }

    #[test]
    fn gutter_rows_all_match_the_reported_width() {
        // The reported width drives the image geometry and the sixel cursor
        // arithmetic, so a row that disagrees with it shoves the image sideways.
        for sr in [8000.0, 44100.0, 48000.0, 96000.0, 192000.0, 384000.0] {
            for log_axis in [true, false] {
                for rows in 4..=16 {
                    let (lines, w) = analysis_gutter(120, rows, log_axis, sr);
                    assert_eq!(lines.len(), rows);
                    assert!(w >= GUTTER_W, "gutter narrower than the floor at sr {sr}");
                    for line in &lines {
                        assert_eq!(
                            crate::ansi::visible_len(line),
                            w,
                            "row width != reported (sr {sr}, log {log_axis}): {line:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn gutter_widens_for_labels_that_outgrow_the_field() {
        // Ordinary rates stay at the 4-cell floor...
        assert_eq!(analysis_gutter(120, 16, false, 48000.0).1, GUTTER_W);
        assert_eq!(analysis_gutter(120, 16, true, 192000.0).1, GUTTER_W);
        // ...while the linear ladder past 100 kHz needs one cell more.
        let (_, w) = analysis_gutter(120, 16, false, 384000.0);
        assert_eq!(w, GUTTER_W + 1);
    }

    #[test]
    fn gutter_labels_stay_inside_the_displayed_range() {
        for sr in [44100.0, 48000.0, 192000.0] {
            for log_axis in [true, false] {
                let rows = 16;
                let labels = gutter_labels(rows, log_axis, sr);
                for (row, _) in labels.iter().enumerate().filter(|(_, l)| l.is_some()) {
                    let f = analysis_row_freq(row, rows, log_axis, sr);
                    assert!(f <= sr * 0.5 + 1.0, "label above Nyquist at row {row}");
                }
            }
        }
    }

    #[test]
    fn gutter_labels_never_collide_on_a_row() {
        // One label per row; a crowded ladder drops candidates rather than
        // overwriting, so the count can never exceed the row count.
        for rows in 1..=16 {
            let labels = gutter_labels(rows, true, 192000.0);
            assert_eq!(labels.len(), rows);
            assert!(labels.iter().flatten().count() <= rows);
        }
    }

    #[test]
    fn gutter_narrows_the_image_instead_of_overflowing_the_window() {
        // Indent + gutter + image + right margin must fit the terminal at every
        // width the gutter is shown at. Overflow is the sixel auto-scroll storm.
        for term_w in 12..=300 {
            let (_, gutter_w) = analysis_gutter(term_w, 16, true, 48000.0);
            let cols = analysis_cols_for(term_w, gutter_w);
            assert!(
                2 + gutter_w + cols + 2 <= term_w,
                "block overflows at term_w {term_w}: gutter {gutter_w} + image {cols}"
            );
        }
    }

    #[test]
    fn gutter_costs_exactly_its_width_in_image_cells() {
        // Turning the legend on must take cells from the image, not add them.
        let bare = analysis_cols_for(120, 0);
        let (_, gutter_w) = analysis_gutter(120, 16, true, 48000.0);
        assert_eq!(analysis_cols_for(120, gutter_w), bare - gutter_w);
    }

    #[test]
    fn gutter_is_dropped_in_narrow_terminals() {
        assert_eq!(analysis_gutter(120, 16, true, 48000.0).1, GUTTER_W);
        assert_eq!(analysis_gutter(GUTTER_MIN_TERM_W, 16, true, 48000.0).1, GUTTER_W);
        let (lines, w) = analysis_gutter(GUTTER_MIN_TERM_W - 1, 16, true, 48000.0);
        assert_eq!(w, 0);
        assert!(lines.is_empty());
        assert_eq!(analysis_gutter(20, 16, true, 48000.0).1, 0);
    }

    #[test]
    fn gutter_survives_a_silent_analyser() {
        // Before the first packet the sample rate is 0: keep the column's
        // width stable (layout must not shift) but print no bogus labels.
        let (lines, w) = analysis_gutter(120, 16, true, 0.0);
        assert_eq!(lines.len(), 16);
        assert_eq!(w, GUTTER_W);
        for line in &lines {
            assert_eq!(crate::ansi::visible_len(line), GUTTER_W);
        }
        assert!(gutter_labels(16, true, 0.0).iter().all(|l| l.is_none()));
        assert!(gutter_labels(16, false, 0.0).iter().all(|l| l.is_none()));
    }

    #[test]
    fn nice_step_snaps_to_the_one_two_five_ladder() {
        assert_eq!(nice_step(4800.0), 5000.0);
        assert_eq!(nice_step(19200.0), 20000.0);
        assert_eq!(nice_step(1.2), 1.0);
        assert_eq!(nice_step(3.0), 2.0);
        assert_eq!(nice_step(8.0), 10.0);
    }
}
