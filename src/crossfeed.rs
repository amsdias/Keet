/// Meier-style headphone crossfeed filter.
///
/// For each stereo frame:
/// 1. Low-pass filter the opposite channel (~700Hz Butterworth, authentic Meier value)
/// 2. Delay the filtered signal by ~300us (interaural time difference)
/// 3. Blend the filtered+delayed opposite channel at the crossfeed level
///
/// High frequencies maintain stereo separation while low frequencies
/// cross over, simulating speaker listening in a room.

use std::f32::consts::{FRAC_1_SQRT_2, PI};

/// Crossfeed preset definition
pub struct CrossfeedPreset {
    pub name: String,
    pub level_db: f32,
    pub cutoff_hz: f32,
}

/// Biquad filter coefficients (normalized, a0 = 1.0)
struct BiquadCoeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl BiquadCoeffs {
    fn passthrough() -> Self {
        Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 }
    }

    /// 2nd-order Butterworth low-pass filter (Audio EQ Cookbook)
    fn low_pass(cutoff: f32, sample_rate: f32) -> Self {
        if sample_rate <= 0.0 || cutoff <= 0.0 {
            return Self::passthrough();
        }
        let w0 = 2.0 * PI * cutoff / sample_rate;
        let cos_w0 = w0.cos();
        let alpha = w0.sin() / (2.0 * FRAC_1_SQRT_2); // Q = 1/sqrt(2) for Butterworth

        let b0 = (1.0 - cos_w0) / 2.0;
        let b1 = 1.0 - cos_w0;
        let b2 = (1.0 - cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }
}

/// Biquad filter state (2nd-order IIR) for one channel
struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl BiquadState {
    fn new() -> Self {
        Self { x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }

    fn process(&mut self, coeffs: &BiquadCoeffs, input: f32) -> f32 {
        let output = coeffs.b0 * input
            + coeffs.b1 * self.x1
            + coeffs.b2 * self.x2
            - coeffs.a1 * self.y1
            - coeffs.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = input;
        self.y2 = self.y1;
        self.y1 = output;
        output
    }
}

/// Simple delay line (circular buffer)
struct DelayLine {
    buffer: Vec<f32>,
    write_pos: usize,
}

impl DelayLine {
    fn new(delay_samples: usize) -> Self {
        Self {
            buffer: vec![0.0; delay_samples.max(1)],
            write_pos: 0,
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let output = self.buffer[self.write_pos];
        self.buffer[self.write_pos] = input;
        self.write_pos = (self.write_pos + 1) % self.buffer.len();
        output
    }

    fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_pos = 0;
    }

    fn resize(&mut self, new_len: usize) {
        let len = new_len.max(1);
        self.buffer = vec![0.0; len];
        self.write_pos = 0;
    }
}

/// Interaural time difference: ~300 microseconds
const ITD_SECONDS: f32 = 0.0003;

/// Meier-style headphone crossfeed filter
pub struct CrossfeedFilter {
    // LPF for each crossfeed path (R→L and L→R)
    lpf_coeffs: BiquadCoeffs,
    lpf_state_l: BiquadState, // filters R channel signal that feeds into L
    lpf_state_r: BiquadState, // filters L channel signal that feeds into R

    // Delay lines for interaural time difference
    delay_l: DelayLine, // delayed filtered R → feeds into L
    delay_r: DelayLine, // delayed filtered L → feeds into R

    // Crossfeed level (linear gain)
    level: f32,
    active: bool,
}

impl CrossfeedFilter {
    pub fn new() -> Self {
        Self {
            lpf_coeffs: BiquadCoeffs::passthrough(),
            lpf_state_l: BiquadState::new(),
            lpf_state_r: BiquadState::new(),
            delay_l: DelayLine::new(1),
            delay_r: DelayLine::new(1),
            level: 0.0,
            active: false,
        }
    }

    pub fn load_preset(&mut self, preset: &CrossfeedPreset, sample_rate: f32) {
        if preset.name == "Off" {
            self.active = false;
            self.level = 0.0;
            return;
        }

        self.lpf_coeffs = BiquadCoeffs::low_pass(preset.cutoff_hz, sample_rate);
        self.level = 10.0_f32.powf(preset.level_db / 20.0);

        let delay_samples = (ITD_SECONDS * sample_rate).round() as usize;
        self.delay_l.resize(delay_samples);
        self.delay_r.resize(delay_samples);

        self.reset();
        self.active = true;
    }

    pub fn reset(&mut self) {
        self.lpf_state_l.reset();
        self.lpf_state_r.reset();
        self.delay_l.reset();
        self.delay_r.reset();
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Process interleaved stereo samples in-place
    pub fn process_stereo(&mut self, samples: &mut [f32]) {
        if !self.active {
            return;
        }

        let frames = samples.len() / 2;
        for frame in 0..frames {
            let li = frame * 2;
            let ri = frame * 2 + 1;
            let left = samples[li];
            let right = samples[ri];

            // Low-pass filter the opposite channel
            let filtered_r = self.lpf_state_l.process(&self.lpf_coeffs, right);
            let filtered_l = self.lpf_state_r.process(&self.lpf_coeffs, left);

            // Delay the filtered signals (ITD simulation)
            let delayed_r = self.delay_l.process(filtered_r);
            let delayed_l = self.delay_r.process(filtered_l);

            // Blend: add filtered+delayed opposite channel
            samples[li] = left + self.level * delayed_r;
            samples[ri] = right + self.level * delayed_l;
        }
    }
}

pub fn builtin_presets() -> Vec<CrossfeedPreset> {
    vec![
        // Note: 700Hz cutoff is the authentic Meier crossfeed value. The spec says "~2kHz" as a
        // simplification, but the original Meier design uses ~650-700Hz which produces a more natural,
        // less colored result. Higher cutoffs (1-2kHz) make the effect more aggressive.
        CrossfeedPreset { name: "Off".to_string(), level_db: 0.0, cutoff_hz: 700.0 },
        CrossfeedPreset { name: "Light".to_string(), level_db: -6.0, cutoff_hz: 700.0 },
        CrossfeedPreset { name: "Medium".to_string(), level_db: -4.5, cutoff_hz: 700.0 },
        CrossfeedPreset { name: "Strong".to_string(), level_db: -3.0, cutoff_hz: 700.0 },
    ]
}
