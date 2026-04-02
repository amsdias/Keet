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
    struct MachTaskBasicInfo {
        virtual_size: u64,
        resident_size: u64,
        resident_size_max: u64,
        user_time: TimeValue,
        system_time: TimeValue,
        policy: i32,
        suspend_count: i32,
    }
    extern "C" {
        fn mach_task_self() -> u32;
        fn task_info(target: u32, flavor: u32, info: *mut i32, count: *mut u32) -> i32;
    }
    const TASK_THREAD_TIMES_INFO: u32 = 3;
    const MACH_TASK_BASIC_INFO: u32 = 20;
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

        // Memory via MACH_TASK_BASIC_INFO (flavor 20)
        let mut info: MachTaskBasicInfo = std::mem::zeroed();
        count = (std::mem::size_of::<MachTaskBasicInfo>() / 4) as u32;
        let mem = if task_info(task, MACH_TASK_BASIC_INFO,
                               &mut info as *mut _ as *mut i32, &mut count) == 0 {
            info.resident_size
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
            .find(|l| l.starts_with("VmRSS:"))
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
    sample_buffer: Vec<f32>,
    smoothed: [f32; SPECTRUM_BANDS],
    heights: [f32; SPECTRUM_BANDS],
}

impl ChannelBands {
    fn new() -> Self {
        Self {
            sample_buffer: Vec::with_capacity(FFT_SIZE * 2),
            smoothed: [0.0; SPECTRUM_BANDS],
            heights: [0.0; SPECTRUM_BANDS],
        }
    }
}

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
            let l = samples[f * channels].abs();
            peak_l = peak_l.max(l);
            if channels >= 2 {
                let r = samples[f * channels + 1].abs();
                peak_r = peak_r.max(r);
                self.ch_l.sample_buffer.push(samples[f * channels]);
                self.ch_r.sample_buffer.push(samples[f * channels + 1]);
            } else {
                peak_r = peak_l;
                self.ch_l.sample_buffer.push(samples[f * channels]);
                self.ch_r.sample_buffer.push(samples[f * channels]);
            }
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
            let l_bands = self.run_fft_and_compute(&self.ch_l.sample_buffer[..FFT_SIZE].to_vec());
            // Process R channel
            let r_bands = self.run_fft_and_compute(&self.ch_r.sample_buffer[..FFT_SIZE].to_vec());

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

            // 50% overlap
            self.ch_l.sample_buffer.drain(..FFT_SIZE / 2);
            self.ch_r.sample_buffer.drain(..FFT_SIZE / 2);
        }
    }

    /// Run FFT on samples and return raw band values (no ballistics)
    fn run_fft_and_compute(&mut self, samples: &[f32]) -> [f32; SPECTRUM_BANDS] {
        for (i, (&sample, &w)) in samples.iter().zip(&self.window).enumerate() {
            self.fft_input[i] = sample * w;
        }

        if self.fft.process(&mut self.fft_input, &mut self.fft_output).is_err() {
            return [0.0; SPECTRUM_BANDS];
        }

        let nyquist = self.sample_rate as f32 / 2.0;
        let n_bins = self.fft_output.len();
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

                let mag = self.fft_output[bin].norm() * window_correction;
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
    }
}
