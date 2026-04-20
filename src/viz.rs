use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use realfft::{RealFftPlanner, RealToComplex};

use crate::state::{
    PlayerState, VizMode, VizStyle, SPECTRUM_BANDS, FFT_SIZE, VIZ_DECAY,
    GRAVITY, DOT_GRAVITY, ATTACK, HOLD_TIME,
    C_RESET, C_DIM, C_CYAN, C_GREEN, C_YELLOW, C_MAGENTA, C_RED,
};

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
// Number of spectrogram columns kept in history (time axis).
pub const SPECTROGRAM_COLS: usize = 60;

pub struct VizAnalyser {
    fft: Arc<dyn RealToComplex<f32>>,
    fft_input: Vec<f32>,
    fft_output: Vec<realfft::num_complex::Complex<f32>>,
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
}

impl VizAnalyser {
    pub fn new(sample_rate: u32) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let fft_input = fft.make_input_vec();
        let fft_output = fft.make_output_vec();
        let window: Vec<f32> = (0..FFT_SIZE)
            .map(|i| 0.5 *(1.0 - (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos()))
            .collect();

        Self {
            fft,
            fft_input,
            fft_output,
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
            self.smoothed_peak_l = self.smoothed_peak_l * DECAY_FACTOR;
        }

        if peak_r > self.smoothed_peak_r {
            self.smoothed_peak_r = self.smoothed_peak_r * ATTACK_FACTOR + peak_r * (1.0 - ATTACK_FACTOR);
        } else {
            self.smoothed_peak_r = self.smoothed_peak_r * DECAY_FACTOR;
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
            let l_bands = Self::run_fft_and_compute(&*self.fft, &mut self.fft_input, &mut self.fft_output, self.sample_rate);

            // Process R channel
            for (i, (&sample, &w)) in self.ch_r.sample_buffer.iter().take(FFT_SIZE).zip(&self.window).enumerate() {
                self.fft_input[i] = sample * w;
            }
            let r_bands = Self::run_fft_and_compute(&*self.fft, &mut self.fft_input, &mut self.fft_output, self.sample_rate);

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

            // Append to spectrogram history (mono = L+R average of the smoothed bands).
            if self.spectrogram_history.len() == SPECTROGRAM_COLS {
                self.spectrogram_history.pop_front();
            }
            self.spectrogram_history.push_back(mono);

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
        sample_rate: u32,
    ) -> [f32; SPECTRUM_BANDS] {
        if fft.process(fft_input, fft_output).is_err() {
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
                heights[i] = (heights[i] - GRAVITY).max(0.0);
            }
            smoothed[i] = smoothed[i] * VIZ_DECAY + heights[i] * (1.0 - VIZ_DECAY);
        }
    }
}

const SPECTRUM_H_CHARS: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn render_vu_meter(state: &PlayerState, style: VizStyle) -> Vec<String> {
    let (left, right) = state.get_peaks();
    let (dot_l, dot_r) = state.get_vu_dots();
    let bar_width = 30;

    fn make_bar(level: f32, dot_val: f32, label: &str, width: usize, style: VizStyle) -> String {
        let full = (level.clamp(0.0, 1.0) * width as f32) as usize;
        let dot_idx = (dot_val.clamp(0.0, 1.0) * width as f32) as usize;

        let yellow_start = width * 6 / 10 + 1;
        let red_start = width * 8 / 10 + 1;

        let mut bar = format!("  {C_DIM}{label}{C_RESET} ");
        let mut last_color = "";
        for i in 0..width {
            let color = if i >= red_start { C_RED }
                        else if i >= yellow_start { C_YELLOW }
                        else { C_GREEN };
            if color != last_color {
                bar.push_str(color);
                last_color = color;
            }

            match style {
                VizStyle::Dots => {
                    if i < full {
                        bar.push('⣿');
                    } else if i == dot_idx && dot_idx > 0 {
                        bar.push_str(C_RESET);
                        bar.push_str(color);
                        last_color = color;
                        bar.push('⠅');
                    } else {
                        if last_color != C_DIM { bar.push_str(C_DIM); last_color = C_DIM; }
                        bar.push('⣀');
                    }
                }
                VizStyle::Bars => {
                    if i < full {
                        bar.push('█');
                    } else if i == dot_idx && dot_idx > 0 {
                        // Bright thin bar as peak dot
                        bar.push_str(C_RESET);
                        bar.push_str(color);
                        last_color = color;
                        bar.push('▏');
                    } else {
                        if last_color != C_DIM { bar.push_str(C_DIM); last_color = C_DIM; }
                        bar.push('▏');
                    }
                }
            }
        }
        bar.push_str(C_RESET);
        bar
    }

    let mut lines = vec![
        make_bar(left, dot_l, "L", bar_width, style),
    ];
    if matches!(style, VizStyle::Bars) {
        lines.push(String::new()); // minimal empty line gap
    }
    lines.push(make_bar(right, dot_r, "R", bar_width, style));
    lines
}

const SPECTRUM_H_BRAILLE: &[char] = &[' ', '⣀', '⣀', '⣤', '⣤', '⣶', '⣶', '⣿', '⣿'];
// Braille chars filling from top down (for R channel going down)
const SPECTRUM_H_BRAILLE_DN: &[char] = &[' ', '⠉', '⠉', '⠛', '⠛', '⠿', '⠿', '⣿', '⣿'];
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

    let chars_up = match style {
        VizStyle::Bars => SPECTRUM_H_CHARS,
        VizStyle::Dots => SPECTRUM_H_BRAILLE,
    };

    // L channel (bars going up) — same as before
    let mut line_l = String::from("  ");
    for (i, &level) in spec_l.iter().enumerate() {
        let char_idx = (level * 8.0).min(8.0) as usize;
        let color = BAND_COLORS.get(i).unwrap_or(&C_YELLOW);
        line_l.push_str(&format!("{}{} ", color, chars_up[char_idx]));
    }
    line_l.push_str(C_RESET);

    // R channel (bars going down)
    let mut line_r = String::from("  ");
    for (i, &level) in spec_r.iter().enumerate() {
        let char_idx = (level * 8.0).min(8.0) as usize;
        let color = BAND_COLORS.get(i).unwrap_or(&C_YELLOW);
        match style {
            VizStyle::Dots => {
                line_r.push_str(&format!("{}{} ", color, SPECTRUM_H_BRAILLE_DN[char_idx]));
            }
            VizStyle::Bars => {
                if char_idx == 0 {
                    line_r.push_str("  ");
                } else if char_idx == 8 {
                    line_r.push_str(&format!("{}█ ", color));
                } else {
                    // Reverse video: FG becomes BG and vice versa, so the block's
                    // "empty" part uses the terminal's real background (invisible)
                    line_r.push_str(&format!("{}\x1B[7m{}\x1B[27m{C_RESET} ", color, SPECTRUM_H_BLOCKS_DN[char_idx]));
                }
            }
        }
    }
    line_r.push_str(C_RESET);

    vec![line_l, line_r]
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

    let row_colors = [
        C_RED, C_RED, C_YELLOW, C_YELLOW,
        C_GREEN, C_GREEN, C_GREEN, C_GREEN,
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
                let idx = (frac * 7.0).max(1.0).min(7.0) as usize;
                lines[row].push_str(&format!("{C_RESET}{}{} ", color, partials[idx]));
            } else if dot_in_row {
                let dot_ch = match style {
                    VizStyle::Dots => '⣀',
                    VizStyle::Bars => {
                        let frac = (dot - row_bottom) / (row_top - row_bottom);
                        let idx = (frac * 7.0).max(1.0).min(7.0) as usize;
                        LOWER_BLOCKS[idx.min(2)]
                    }
                };
                lines[row].push_str(&format!("{C_RESET}{}{} ", color, dot_ch));
            } else if bar_partial {
                let frac = (level - row_bottom) / (row_top - row_bottom);
                let idx = (frac * 7.0).max(1.0) as usize;
                lines[row].push_str(&format!("{C_RESET}{}{} ", color, partials[idx]));
            } else {
                lines[row].push_str(&format!("{C_RESET}  "));
            }
        }
        lines[row].push_str(C_RESET);
    }
    lines
}

pub fn get_viz_line_count(mode: VizMode, style: VizStyle) -> usize {
    match mode {
        VizMode::None => 0,
        VizMode::VuMeter => if matches!(style, VizStyle::Bars) { 4 } else { 3 },
        VizMode::SpectrumHorizontal => 3,
        VizMode::SpectrumVertical => 9,
        VizMode::Oscilloscope => OSCILLOSCOPE_ROWS + 1,
        VizMode::Lissajous => LISSAJOUS_ROWS + 1,
        VizMode::Spectrogram => SPECTROGRAM_ROWS + 1,
    }
}

// --- Oscilloscope -----------------------------------------------------------

const OSCILLOSCOPE_COLS: usize = 60;   // terminal cells wide
const OSCILLOSCOPE_ROWS: usize = 8;    // terminal cells tall
const OSCILLOSCOPE_DOTS_W: usize = OSCILLOSCOPE_COLS * 2; // braille 2 dots/cell
const OSCILLOSCOPE_DOTS_H: usize = OSCILLOSCOPE_ROWS * 4;

// Bit offsets within a braille cell for dot (px, py) where px∈0..2, py∈0..4.
const BRAILLE_BITS: [[u32; 4]; 2] = [
    [0x01, 0x02, 0x04, 0x40],
    [0x08, 0x10, 0x20, 0x80],
];

pub fn render_oscilloscope(analyser: &VizAnalyser, style: VizStyle) -> Vec<String> {
    match style {
        VizStyle::Dots => render_oscilloscope_dots(analyser),
        VizStyle::Bars => render_oscilloscope_bars(analyser),
    }
}

fn render_oscilloscope_bars(analyser: &VizAnalyser) -> Vec<String> {
    let buf = &analyser.waveform_buf;
    // 2× horizontal resolution via quadrant blocks: sample at 2× cell width.
    const SUB_COLS: usize = OSCILLOSCOPE_COLS * 2;
    const SUB_ROWS: usize = OSCILLOSCOPE_ROWS * 2;
    let mut col_values = vec![0.0f32; SUB_COLS];
    if !buf.is_empty() {
        let n = buf.len();
        for x in 0..SUB_COLS {
            let idx = x * (n - 1) / SUB_COLS.max(1);
            let (l, r) = buf[idx];
            col_values[x] = ((l + r) * 0.5).clamp(-1.0, 1.0);
        }
    }
    // Mark filled sub-cells (2 sub-cols × 2 sub-rows per terminal cell).
    let mid_sub = SUB_ROWS as f32 / 2.0;
    let mut sub_grid = vec![false; SUB_ROWS * SUB_COLS];
    for (x, &v) in col_values.iter().enumerate() {
        let wave_sub = mid_sub - v * mid_sub;
        let (lo, hi) = if wave_sub < mid_sub { (wave_sub, mid_sub) } else { (mid_sub, wave_sub) };
        let lo_i = lo.floor() as usize;
        let hi_i = (hi.ceil() as usize).min(SUB_ROWS);
        for sy in lo_i..hi_i {
            sub_grid[sy * SUB_COLS + x] = true;
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
        for cx in 0..OSCILLOSCOPE_COLS {
            let lx = cx * 2;
            let rx = cx * 2 + 1;
            let tl = sub_grid[top_row * SUB_COLS + lx] as u8;
            let tr = sub_grid[top_row * SUB_COLS + rx] as u8;
            let bl = sub_grid[bot_row * SUB_COLS + lx] as u8;
            let br = sub_grid[bot_row * SUB_COLS + rx] as u8;
            let idx = (tl) | (tr << 1) | (bl << 2) | (br << 3);
            line.push(QUAD[idx as usize]);
        }
        line.push_str(C_RESET);
        lines.push(line);
    }
    lines
}

fn render_oscilloscope_dots(analyser: &VizAnalyser) -> Vec<String> {
    let buf = &analyser.waveform_buf;
    let mut grid = vec![0u32; OSCILLOSCOPE_DOTS_W * OSCILLOSCOPE_DOTS_H];
    let set = |g: &mut [u32], x: usize, y: usize| {
        if x < OSCILLOSCOPE_DOTS_W && y < OSCILLOSCOPE_DOTS_H {
            g[y * OSCILLOSCOPE_DOTS_W + x] = 1;
        }
    };

    if !buf.is_empty() {
        let n = buf.len();
        let mut prev_y: Option<i32> = None;
        let mid = (OSCILLOSCOPE_DOTS_H / 2) as i32;
        for x in 0..OSCILLOSCOPE_DOTS_W {
            // Map column to sample index (newest on right).
            let idx = x * (n - 1) / OSCILLOSCOPE_DOTS_W.max(1);
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
        for cx in 0..OSCILLOSCOPE_COLS {
            let mut bits: u32 = 0;
            for py in 0..4 {
                for px in 0..2 {
                    let gx = cx * 2 + px;
                    let gy = cy * 4 + py;
                    if grid[gy * OSCILLOSCOPE_DOTS_W + gx] != 0 {
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

pub fn render_lissajous(analyser: &VizAnalyser, style: VizStyle) -> Vec<String> {
    match style {
        VizStyle::Dots => render_lissajous_dots(analyser),
        VizStyle::Bars => render_lissajous_bars(analyser),
    }
}

fn render_lissajous_bars(analyser: &VizAnalyser) -> Vec<String> {
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
        let mut line = String::from("  ");
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

fn render_lissajous_dots(analyser: &VizAnalyser) -> Vec<String> {
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

    // Center the box horizontally-ish — align with the rest of the viz (2-space pad).
    let mut lines = Vec::with_capacity(LISSAJOUS_ROWS);
    for cy in 0..LISSAJOUS_ROWS {
        let mut line = String::from("  ");
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

const SPECTROGRAM_ROWS: usize = 8;

// 31 bands → 8 rows (top = highest freq). Colors mirror BAND_COLORS by region.
const SPECTROGRAM_ROW_COLORS: [&str; SPECTROGRAM_ROWS] = [
    C_MAGENTA, C_RED, C_RED, C_YELLOW,
    C_YELLOW, C_GREEN, C_GREEN, C_CYAN,
];

// 9-level braille fill, one extra dot per step so each magnitude maps to a
// visibly distinct glyph (the shared SPECTRUM_H_BRAILLE table has duplicates).
const SPECTROGRAM_DOTS: &[char] = &[' ', '⡀', '⣀', '⣄', '⣤', '⣦', '⣶', '⣷', '⣿'];

pub fn render_spectrogram(analyser: &VizAnalyser, style: VizStyle) -> Vec<String> {
    let hist = &analyser.spectrogram_history;
    let cols = SPECTROGRAM_COLS;
    let chars: &[char] = match style {
        VizStyle::Bars => SPECTRUM_H_CHARS,
        VizStyle::Dots => SPECTROGRAM_DOTS,
    };
    // Per-band magnitudes after FFT post-processing rarely exceed ~0.7; with
    // a linear mapping the upper glyphs are essentially unreachable. Sqrt
    // pulls mid-range values upward so the dynamic range actually spans
    // the palette. Applied for dots only — bars already look fine linearly.
    let boost = matches!(style, VizStyle::Dots);

    // Group 31 bands into 8 rows, top-to-bottom = highest-to-lowest freq.
    // Row i pulls the max over its band group for snappier high-freq response.
    let band_groups: [(usize, usize); SPECTROGRAM_ROWS] = [
        (27, 31), // 10k-20k air
        (23, 27), // 4k-8k brilliance
        (19, 23), // 2k-3.15k presence
        (15, 19), // 630-1.25k upper-mid
        (11, 15), // 200-500 low-mid
        (7, 11),  // 100-160 bass
        (4, 7),   // 50-80 low bass
        (0, 4),   // 20-40 sub
    ];

    let mut lines = Vec::with_capacity(SPECTROGRAM_ROWS);
    for (row, &(lo, hi)) in band_groups.iter().enumerate() {
        let mut line = String::from("  ");
        let color = SPECTROGRAM_ROW_COLORS[row];
        line.push_str(color);
        // Oldest column on the left, newest on the right. Pad with spaces when history
        // hasn't filled up yet.
        let n = hist.len();
        let pad = cols.saturating_sub(n);
        for _ in 0..pad {
            line.push(' ');
        }
        for col in 0..n {
            let frame = &hist[col];
            let mut v: f32 = 0.0;
            for b in lo..hi {
                v = v.max(frame[b]);
            }
            let v_mapped = if boost { v.sqrt() } else { v };
            let idx = (v_mapped * 8.0).clamp(0.0, 8.0) as usize;
            line.push(chars[idx]);
        }
        line.push_str(C_RESET);
        lines.push(line);
    }
    lines
}
