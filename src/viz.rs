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
pub const SPECTROGRAM_COLS: usize = 240;
// Each column averages this many FFT hops, dilating the time axis so the display
// scrolls slower and smoother. At ~43 ms/hop: 1 = ~2.6 s window (was), 4 = ~10 s.
pub const SPECTROGRAM_HOPS_PER_COL: usize = 4;

// Analysis spectrogram: history depth (time window, columns ≈ FFT hops),
// vertical rows, and dB contrast window.
const SPECTRO_ANALYSIS_COLS: usize = 512;
const SPECTRO_ANALYSIS_ROWS: usize = 16;
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

        const ISO_CENTERS: [f32; 31] = [
            20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0,
            200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0,
            2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0, 10000.0, 12500.0, 16000.0,
            20000.0,
        ];
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

pub fn render_vu_meter(state: &PlayerState, style: VizStyle, width: usize) -> Vec<String> {
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

    let mut lines = vec![
        make_bar(left, dot_l, "L", bar_width, style, &vp),
    ];
    if matches!(style, VizStyle::Bars) {
        lines.push(String::new()); // minimal empty line gap
    }
    lines.push(make_bar(right, dot_r, "R", bar_width, style, &vp));
    lines
}

// The horizontal spectrum is stacked over SPECTRUM_H_ROWS braille rows per channel
// (was a single row) for more height. Per-row partial fills index by quarters filled
// (1..4): up fills from the bottom (L channel), down fills from the top (R channel).
const SPECTRUM_H_ROWS: usize = 3;
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

pub fn render_spectrum_horizontal(state: &PlayerState, style: VizStyle) -> Vec<String> {
    let spec_l = state.get_spectrum();
    let spec_r = state.get_spectrum_r();
    let kind = state.theme_kind();
    let vp = viz_palette(kind);
    let n = SPECTRUM_H_ROWS;
    let mut lines: Vec<String> = Vec::with_capacity(n * 2);

    // L channel: bars grow upward. Rows print top→bottom, so row 0 covers the
    // highest magnitude slice [(n-1)/n, 1.0] and the last row the base [0, 1/n].
    for r in 0..n {
        let lo = (n - 1 - r) as f32 / n as f32;
        let hi = (n - r) as f32 / n as f32;
        let mut line = String::from("  ");
        for (i, &level) in spec_l.iter().enumerate() {
            let color = band_color(i, &vp, kind);
            line.push_str(&h_cell(level, lo, hi, style, color, true));
        }
        line.push_str(vp.reset);
        lines.push(line);
    }

    // R channel: bars grow downward. Rows print top→bottom, so row 0 is the base
    // [0, 1/n] just under the L bars and the last row the deepest [(n-1)/n, 1.0].
    for r in 0..n {
        let lo = r as f32 / n as f32;
        let hi = (r + 1) as f32 / n as f32;
        let mut line = String::from("  ");
        for (i, &level) in spec_r.iter().enumerate() {
            let color = band_color(i, &vp, kind);
            line.push_str(&h_cell(level, lo, hi, style, color, false));
        }
        line.push_str(vp.reset);
        lines.push(line);
    }

    lines
}

/// Render one 2-char-wide spectrum cell for a horizontal-spectrum row spanning the
/// magnitude range `[lo, hi)`. `up` selects bottom-up fill (L channel) vs top-down
/// fill (R channel).
fn h_cell(level: f32, lo: f32, hi: f32, style: VizStyle, color: &str, up: bool) -> String {
    if level >= hi {
        let full = match style { VizStyle::Bars => '█', VizStyle::Dots => '⣿' };
        return format!("{}{} ", color, full);
    }
    if level <= lo {
        return String::from("  ");
    }
    let frac = (level - lo) / (hi - lo); // fraction of this row that's filled
    match style {
        VizStyle::Dots => {
            let idx = ((frac * 4.0).ceil() as usize).clamp(1, 4);
            let ch = if up { H_UP_BRAILLE[idx] } else { H_DN_BRAILLE[idx] };
            format!("{}{} ", color, ch)
        }
        VizStyle::Bars => {
            let idx = ((frac * 8.0).ceil() as usize).clamp(1, 8);
            if up {
                format!("{}{} ", color, SPECTRUM_H_CHARS[idx])
            } else if idx >= 8 {
                format!("{}█ ", color)
            } else {
                // Reverse video: FG becomes BG and vice versa, so the block's
                // "empty" part uses the terminal's real background (invisible),
                // making the block fill from the top of the cell.
                format!("{}\x1B[7m{}\x1B[27m{C_RESET} ", color, SPECTRUM_H_BLOCKS_DN[idx])
            }
        }
    }
}

pub fn render_spectrum_vertical(state: &PlayerState, style: VizStyle) -> Vec<String> {
    const LOWER_BLOCKS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];
    const BRAILLE_V: &[char] = &[' ', '⣀', '⣀', '⣤', '⣤', '⣶', '⣶', '⣿'];
    let spec_l = state.get_spectrum();
    let spec_r = state.get_spectrum_r();
    let spectrum: [f32; SPECTRUM_BANDS] = std::array::from_fn(|i| (spec_l[i] + spec_r[i]) / 2.0);
    let dots = state.get_dots();
    let height = 8;
    let mut lines = vec![String::new(); height];

    let vp = viz_palette(state.theme_kind());
    // Top rows are "hot" (loud), bottom rows are quiet — map onto hot→mid→low.
    let row_colors = [
        vp.hot, vp.hot, vp.mid, vp.mid,
        vp.low, vp.low, vp.low, vp.low,
    ];

    let partials = match style {
        VizStyle::Bars => LOWER_BLOCKS,
        VizStyle::Dots => BRAILLE_V,
    };

    for row in 0..height {
        lines[row].push_str("  ");
        let row_bottom = (height - 1 - row) as f32 / height as f32;
        let row_top = (height - row) as f32 / height as f32;
        let color = row_colors[row];

        for (i, &level) in spectrum.iter().enumerate() {
            let dot = dots[i];
            let dot_in_row = dot >= row_bottom && dot < row_top;
            let bar_partial = level > row_bottom && level < row_top;
            let bar_full = level >= row_top;

            if bar_full {
                let ch = match style { VizStyle::Bars => '█', VizStyle::Dots => '⣿' };
                lines[row].push_str(&format!("{C_RESET}{}{} ", color, ch));
            } else if bar_partial && dot_in_row {
                let frac = (dot - row_bottom) / (row_top - row_bottom);
                let idx = (frac * 7.0).clamp(1.0, 7.0) as usize;
                lines[row].push_str(&format!("{C_RESET}{}{} ", color, partials[idx]));
            } else if dot_in_row {
                let dot_ch = match style {
                    VizStyle::Dots => '⣀',
                    VizStyle::Bars => {
                        let frac = (dot - row_bottom) / (row_top - row_bottom);
                        let idx = (frac * 7.0).clamp(1.0, 7.0) as usize;
                        LOWER_BLOCKS[idx.min(2)]
                    }
                };
                lines[row].push_str(&format!("{C_RESET}{}{} ", color, dot_ch));
            } else if bar_partial {
                let frac = (level - row_bottom) / (row_top - row_bottom);
                let idx = (frac * 7.0).max(1.0) as usize;
                lines[row].push_str(&format!("{C_RESET}{}{} ", color, partials[idx]));
            } else {
                lines[row].push_str(&format!("{}  ", vp.reset));
            }
        }
        lines[row].push_str(vp.reset);
    }
    lines
}

pub fn get_viz_line_count(mode: VizMode, style: VizStyle) -> usize {
    match mode {
        VizMode::None => 0,
        VizMode::VuMeter => if matches!(style, VizStyle::Bars) { 4 } else { 3 },
        VizMode::SpectrumHorizontal => SPECTRUM_H_ROWS * 2 + 1,
        VizMode::SpectrumVertical => 9,
        VizMode::Oscilloscope => OSCILLOSCOPE_ROWS + 1,
        VizMode::Lissajous => LISSAJOUS_ROWS + 1,
        VizMode::Spectrogram => SPECTROGRAM_ROWS + 1,
        VizMode::SpectrogramAnalysis => SPECTRO_ANALYSIS_ROWS + 1,
    }
}

// --- Oscilloscope -----------------------------------------------------------

const OSCILLOSCOPE_ROWS: usize = 8;    // terminal cells tall (width is responsive)
const OSCILLOSCOPE_DOTS_H: usize = OSCILLOSCOPE_ROWS * 4;

// Bit offsets within a braille cell for dot (px, py) where px∈0..2, py∈0..4.
const BRAILLE_BITS: [[u32; 4]; 2] = [
    [0x01, 0x02, 0x04, 0x40],
    [0x08, 0x10, 0x20, 0x80],
];

pub fn render_oscilloscope(analyser: &VizAnalyser, style: VizStyle, width: usize) -> Vec<String> {
    // Fill the terminal width (2-space pad + 2-col safety margin), with a sane cap.
    let cols = width.saturating_sub(4).clamp(8, 240);
    match style {
        VizStyle::Dots => render_oscilloscope_dots(analyser, cols),
        VizStyle::Bars => render_oscilloscope_bars(analyser, cols),
    }
}

fn render_oscilloscope_bars(analyser: &VizAnalyser, cols: usize) -> Vec<String> {
    let buf = &analyser.waveform_buf;
    // 2× horizontal resolution via quadrant blocks: sample at 2× cell width.
    let sub_cols = cols * 2;
    const SUB_ROWS: usize = OSCILLOSCOPE_ROWS * 2;
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
    let mid_sub = SUB_ROWS as f32 / 2.0;
    let mut sub_grid = vec![false; SUB_ROWS * sub_cols];
    for (x, &v) in col_values.iter().enumerate() {
        let wave_sub = mid_sub - v * mid_sub;
        let (lo, hi) = if wave_sub < mid_sub { (wave_sub, mid_sub) } else { (mid_sub, wave_sub) };
        let lo_i = lo.floor() as usize;
        let hi_i = (hi.ceil() as usize).min(SUB_ROWS);
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
    let mut lines = Vec::with_capacity(OSCILLOSCOPE_ROWS);
    for cy in 0..OSCILLOSCOPE_ROWS {
        let from_edge = cy.min(OSCILLOSCOPE_ROWS - 1 - cy);
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

fn render_oscilloscope_dots(analyser: &VizAnalyser, cols: usize) -> Vec<String> {
    let buf = &analyser.waveform_buf;
    let dots_w = cols * 2; // braille 2 dots/cell
    let mut grid = vec![0u32; dots_w * OSCILLOSCOPE_DOTS_H];
    let set = |g: &mut [u32], x: usize, y: usize| {
        if x < dots_w && y < OSCILLOSCOPE_DOTS_H {
            g[y * dots_w + x] = 1;
        }
    };

    if !buf.is_empty() {
        let n = buf.len();
        let mut prev_y: Option<i32> = None;
        let mid = (OSCILLOSCOPE_DOTS_H / 2) as i32;
        for x in 0..dots_w {
            // Map column to sample index (newest on right).
            let idx = x * (n - 1) / dots_w.max(1);
            let (l, r) = buf[idx];
            let mono = (l + r) * 0.5;
            let y = mid - (mono.clamp(-1.0, 1.0) * mid as f32) as i32;
            let y = y.clamp(0, (OSCILLOSCOPE_DOTS_H - 1) as i32);
            // Connect previous sample's y to current y so the trace is continuous.
            let y0 = prev_y.unwrap_or(y);
            let (lo, hi) = if y0 < y { (y0, y) } else { (y, y0) };
            for yi in lo..=hi {
                set(&mut grid, x, yi as usize);
            }
            prev_y = Some(y);
        }
    }

    // Render grid row-by-row. Color by distance from center (green → yellow → red).
    let mut lines = Vec::with_capacity(OSCILLOSCOPE_ROWS);
    for cy in 0..OSCILLOSCOPE_ROWS {
        let mut line = String::from("  ");
        let mut last_color = "";
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
            // Color by row — rows near the edges are louder, so redder.
            let from_edge = cy.min(OSCILLOSCOPE_ROWS - 1 - cy);
            let color = match from_edge {
                0 => C_RED,
                1 => C_YELLOW,
                _ => C_GREEN,
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

const LISSAJOUS_COLS: usize = 16;
const LISSAJOUS_ROWS: usize = 8;
const LISSAJOUS_DOTS_W: usize = LISSAJOUS_COLS * 2;
const LISSAJOUS_DOTS_H: usize = LISSAJOUS_ROWS * 4;

pub fn render_lissajous(analyser: &VizAnalyser, style: VizStyle, width: usize) -> Vec<String> {
    // The vectorscope box must stay ~square (16 cols × 8 rows ≈ square at the
    // typical 1:2 cell aspect), so unlike the other modes it can't stretch to
    // the terminal width — center it instead.
    let pad = (width.saturating_sub(LISSAJOUS_COLS) / 2).max(2);
    match style {
        VizStyle::Dots => render_lissajous_dots(analyser, pad),
        VizStyle::Bars => render_lissajous_bars(analyser, pad),
    }
}

fn render_lissajous_bars(analyser: &VizAnalyser, pad: usize) -> Vec<String> {
    let buf = &analyser.waveform_buf;
    let mut counts = vec![0u32; LISSAJOUS_COLS * LISSAJOUS_ROWS];
    let inv_sqrt2 = std::f32::consts::FRAC_1_SQRT_2;
    let w_half = LISSAJOUS_COLS as f32 / 2.0;
    let h_half = LISSAJOUS_ROWS as f32 / 2.0;
    for &(l, r) in buf.iter() {
        let side = (l - r) * inv_sqrt2;
        let mid = (l + r) * inv_sqrt2;
        let x = (w_half + side.clamp(-1.0, 1.0) * (w_half - 0.5)) as i32;
        let y = (h_half - mid.clamp(-1.0, 1.0) * (h_half - 0.5)) as i32;
        if x >= 0 && (x as usize) < LISSAJOUS_COLS && y >= 0 && (y as usize) < LISSAJOUS_ROWS {
            counts[y as usize * LISSAJOUS_COLS + x as usize] += 1;
        }
    }
    let max = counts.iter().copied().max().unwrap_or(1).max(1) as f32;

    let mut lines = Vec::with_capacity(LISSAJOUS_ROWS);
    for cy in 0..LISSAJOUS_ROWS {
        let mut line = " ".repeat(pad);
        line.push_str(C_CYAN);
        for cx in 0..LISSAJOUS_COLS {
            let f = counts[cy * LISSAJOUS_COLS + cx] as f32 / max;
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

fn render_lissajous_dots(analyser: &VizAnalyser, pad: usize) -> Vec<String> {
    let buf = &analyser.waveform_buf;
    let mut grid = vec![0u32; LISSAJOUS_DOTS_W * LISSAJOUS_DOTS_H];

    // Rotated 45° (mid/side): mono signals appear as a vertical line.
    // X = side = (L - R) / sqrt(2); Y = mid = (L + R) / sqrt(2). Terminal Y grows down.
    let inv_sqrt2 = std::f32::consts::FRAC_1_SQRT_2;
    let w_half = (LISSAJOUS_DOTS_W / 2) as f32;
    let h_half = (LISSAJOUS_DOTS_H / 2) as f32;
    for &(l, r) in buf.iter() {
        let side = (l - r) * inv_sqrt2;
        let mid = (l + r) * inv_sqrt2;
        let x = (w_half + side.clamp(-1.0, 1.0) * (w_half - 1.0)) as i32;
        let y = (h_half - mid.clamp(-1.0, 1.0) * (h_half - 1.0)) as i32;
        if x >= 0 && (x as usize) < LISSAJOUS_DOTS_W && y >= 0 && (y as usize) < LISSAJOUS_DOTS_H {
            grid[y as usize * LISSAJOUS_DOTS_W + x as usize] = 1;
        }
    }

    let mut lines = Vec::with_capacity(LISSAJOUS_ROWS);
    for cy in 0..LISSAJOUS_ROWS {
        let mut line = " ".repeat(pad);
        line.push_str(C_CYAN);
        for cx in 0..LISSAJOUS_COLS {
            let mut bits: u32 = 0;
            for py in 0..4 {
                for px in 0..2 {
                    let gx = cx * 2 + px;
                    let gy = cy * 4 + py;
                    if grid[gy * LISSAJOUS_DOTS_W + gx] != 0 {
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

pub fn render_spectrogram(analyser: &VizAnalyser, style: VizStyle, width: usize) -> Vec<String> {
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
    let band_groups: [(usize, usize); SPECTROGRAM_ROWS] = [
        (27, 31), // 10k-20k air
        (24, 27), // 5k-8k brilliance
        (21, 24), // 2.5k-4k presence
        (18, 21), // 1.25k-2k upper-mid
        (15, 18), // 630-1k mid
        (12, 15), // 315-500 low-mid
        (9, 12),  // 160-250 upper bass
        (6, 9),   // 80-125 bass
        (3, 6),   // 40-63 low bass
        (0, 3),   // 20-31.5 sub
    ];

    let mut lines = Vec::with_capacity(SPECTROGRAM_ROWS);
    for (row, &(lo, hi)) in band_groups.iter().enumerate() {
        let mut line = String::from("  ");
        let color = SPECTROGRAM_ROW_COLORS[row];
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
fn analysis_levels_into(out: &mut Vec<u8>, analyser: &VizAnalyser, width_px: usize, height_px: usize, log_axis: bool, logical_cols: usize) {
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
    for x in 0..width_px {
        let slot = x * logical_cols / width_px.max(1);
        if slot < blank_slots { continue; }
        let col = &hist[oldest_shown + (slot - blank_slots)];
        for (y, &bin) in row_bin.iter().enumerate() {
            let db = col.get(bin).copied().unwrap_or(SPECTRO_ANALYSIS_FLOOR_DB);
            let t = analysis_intensity(db, SPECTRO_ANALYSIS_FLOOR_DB, SPECTRO_ANALYSIS_CEIL_DB);
            out[y * width_px + x] = (t * 255.0).round() as u8;
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
fn analysis_sixel_palette() -> &'static [(u8, u8, u8)] {
    static PALETTE: OnceLock<Vec<(u8, u8, u8)>> = OnceLock::new();
    PALETTE.get_or_init(|| (0..128).map(|i| analysis_colormap(i as f32 / 127.0)).collect())
}

pub fn render_spectrogram_analysis(analyser: &VizAnalyser, width: usize, log_axis: bool, paused: bool, rows: usize, force: bool) -> Vec<String> {
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
    let cols = width.saturating_sub(4).clamp(8, 320);
    let gen = analyser.spectro_last_gen;
    let key = (gen, width, log_axis, rows);
    let sixel = analysis_needs_raw_lines();

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
    let image_geom = analysis_image_geometry(crate::cover::detect_protocol(), cols, rows);
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
        let logical_cols = if image_geom.is_some() {
            if sixel {
                let k = w.div_ceil(SPECTRO_ANALYSIS_COLS).max(1);
                w / k
            } else {
                SPECTRO_ANALYSIS_COLS
            }
        } else {
            w
        };
        analysis_levels_into(levels, analyser, w, h, log_axis, logical_cols);
        let mut lines = if image_geom.is_some() && sixel {
            // Fixed-palette indexed sixel — no quantizer, no shimmer.
            for lv in levels.iter_mut() { *lv >>= 1; }
            crate::cover::render_viz_sixel_indexed(levels, analysis_sixel_palette(), w, h, cols as u32, rows as u32)
        } else {
            RGB_BUF.with(|cell| {
                let mut guard = cell.borrow_mut();
                let rgb: &mut Vec<u8> = &mut guard;
                colorize_levels(levels, rgb);
                if image_geom.is_some() {
                    crate::cover::render_image_block(rgb.as_slice(), w as u32, h as u32, cols as u32, rows as u32)
                } else {
                    crate::cover::render_half_block_public(w as u32, h as u32, rgb.as_slice())
                }
            })
        };
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

/// Clamp the analysis-spectrogram row count so the whole frame fits the
/// window. If the frame is even 1-2 rows taller than the terminal, every full
/// repaint (viz switch, track skip) scrolls the banner's top rows into
/// scrollback — on ConPTY that litters one UI fragment per song. `rows_above`
/// = banner + status lines above the viz block; 3 more rows are reserved for
/// the viz separator line, the transient status line, and one row of slack.
pub fn analysis_rows_for_window(term_h: usize, rows_above: usize) -> usize {
    SPECTRO_ANALYSIS_ROWS.min(4.max(term_h.saturating_sub(rows_above + 3)))
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
fn analysis_image_geometry(
    protocol: crate::cover::GraphicsProtocol,
    cols: usize,
    rows: usize,
) -> Option<(usize, usize)> {
    let (cw, ch) = crate::cover::cell_metrics().unwrap_or((8, 16));
    analysis_image_geometry_with(protocol, cols, rows, (cw as usize, ch as usize))
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
        GP::Kitty => Some((SPECTRO_ANALYSIS_COLS, rows * 16)),
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

/// Map a display row to an FFT bin, logarithmic in Hz over [F_MIN, Nyquist].
fn analysis_row_to_bin_log(row: usize, rows: usize, nbins: usize, sample_rate: f32) -> usize {
    const F_MIN: f32 = 30.0;
    if rows <= 1 || nbins == 0 { return 0; }
    let nyquist = sample_rate * 0.5;
    let bin_hz = nyquist / (nbins - 1).max(1) as f32;
    let frac = (rows - 1 - row) as f32 / (rows - 1) as f32; // 0 bottom, 1 top
    let f = F_MIN * (nyquist / F_MIN).powf(frac);
    ((f / bin_hz).round() as usize).min(nbins - 1)
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

    #[test]
    fn intensity_maps_window_to_unit_range() {
        assert!((analysis_intensity(-70.0, -70.0, -10.0) - 0.0).abs() < 1e-6);
        assert!((analysis_intensity(-10.0, -70.0, -10.0) - 1.0).abs() < 1e-6);
        assert!((analysis_intensity(-40.0, -70.0, -10.0) - 0.5).abs() < 1e-6);
        assert_eq!(analysis_intensity(-90.0, -70.0, -10.0), 0.0);
        assert_eq!(analysis_intensity(0.0, -70.0, -10.0), 1.0);
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
        assert_eq!(analysis_rows_for_window(50, 15), SPECTRO_ANALYSIS_ROWS);
    }

    #[test]
    fn analysis_rows_shed_to_fit_short_windows() {
        // 32-row window, 15 rows above the viz block, 3 reserved (separator +
        // transient status + slack) → only 14 spectrogram rows fit.
        assert_eq!(analysis_rows_for_window(32, 15), 14);
    }

    #[test]
    fn analysis_rows_floor_at_four_for_tiny_windows() {
        assert_eq!(analysis_rows_for_window(12, 15), 4);
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
        analysis_levels_into(&mut lv, &a, 4, 1, false, 2);
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
        analysis_levels_into(&mut lv, &a, 2, 1, false, 2);
        assert_ne!(lv[0], lv[1]);
        assert_ne!(lv[1], 0, "newest (hot) hop lands at the right edge");
    }

    #[test]
    fn analysis_geometry_kitty_renders_history_depth() {
        use crate::cover::GraphicsProtocol as GP;
        assert_eq!(
            analysis_image_geometry(GP::Kitty, 120, 16),
            Some((SPECTRO_ANALYSIS_COLS, 16 * 16))
        );
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
    fn analysis_geometry_sixel_uses_probed_cell_metrics() {
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
}
