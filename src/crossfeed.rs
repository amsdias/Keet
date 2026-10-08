//! Meier-style headphone crossfeed filter.
//!
//! For each stereo frame:
//!
//! 1. Low-pass filter the opposite channel (~700Hz Butterworth, authentic Meier value)
//! 2. Delay the filtered signal by the interaural time difference (~300us)
//! 3. Blend the filtered+delayed opposite channel at the crossfeed level
//!
//! High frequencies maintain stereo separation while low frequencies
//! cross over, simulating speaker listening in a room.
//!
//! All three parameters — level, cutoff and ITD — are per-preset, so custom
//! JSON presets can dial the effect from a hint of blend to a wide, soft image
//! without touching code.

use std::f64::consts::FRAC_1_SQRT_2;

use serde::Deserialize;

/// Classic Meier corner frequency. The spec says "~2kHz" as a simplification,
/// but the original design uses ~650-700Hz, which is more natural and less
/// colored. Higher cutoffs (1-2kHz) make the effect more aggressive.
pub const DEFAULT_CUTOFF_HZ: f32 = 700.0;
/// Interaural time difference: ~300 microseconds.
pub const DEFAULT_DELAY_US: f32 = 300.0;

fn default_cutoff() -> f32 {
    DEFAULT_CUTOFF_HZ
}

fn default_delay() -> f32 {
    DEFAULT_DELAY_US
}

/// Crossfeed preset definition. Loaded from JSON or built-in.
#[derive(Deserialize, Clone)]
pub struct CrossfeedPreset {
    pub name: String,
    /// Blend level of the crossfed channel, in dB (negative = quieter).
    pub level_db: f32,
    /// Corner frequency of the crossfeed low-pass, in Hz.
    #[serde(default = "default_cutoff")]
    pub cutoff_hz: f32,
    /// Inter-channel delay (interaural time difference), in microseconds.
    #[serde(default = "default_delay")]
    pub delay_us: f32,
}

use crate::eq::{BiquadCoeffs, BiquadState};

/// Simple delay line (circular buffer)
struct DelayLine {
    buffer: Vec<f64>,
    write_pos: usize,
}

impl DelayLine {
    fn new(delay_samples: usize) -> Self {
        Self {
            buffer: vec![0.0; delay_samples.max(1)],
            write_pos: 0,
        }
    }

    fn process(&mut self, input: f64) -> f64 {
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
    level: f64,
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

        // Custom presets are user-editable JSON, so every value is clamped. A
        // cutoff at or above half the sample rate (12 kHz at a 22.05 kHz
        // source in exclusive mode) made the low-pass unstable: inf, then NaN,
        // which the limiter turns into silence for good.
        let cutoff = preset.cutoff_hz.min(sample_rate * 0.45).max(20.0);
        let level_db = preset.level_db.clamp(-40.0, 0.0);
        let delay_us = preset.delay_us.clamp(0.0, 2_000.0);
        self.lpf_coeffs = BiquadCoeffs::low_pass(cutoff as f64, FRAC_1_SQRT_2, sample_rate.max(1.0) as f64);
        self.level = 10.0_f64.powf(level_db as f64 / 20.0);

        let delay_samples = (delay_us / 1_000_000.0 * sample_rate).round() as usize;
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
            let left = samples[li] as f64;
            let right = samples[ri] as f64;

            // Low-pass filter the opposite channel
            let filtered_r = self.lpf_state_l.process(&self.lpf_coeffs, right);
            let filtered_l = self.lpf_state_r.process(&self.lpf_coeffs, left);

            // Delay the filtered signals (ITD simulation)
            let delayed_r = self.delay_l.process(filtered_r);
            let delayed_l = self.delay_r.process(filtered_l);

            // Blend the DIFFERENCE, not the opposite channel: each side gets
            // the low-passed, delayed (opposite - own). Content common to both
            // channels cancels, so centred bass and vocals come out exactly as
            // they went in; adding the opposite channel outright raised them by
            // 20·log10(1 + level) — up to +4.6 dB of bass at "Strong", straight
            // into the limiter. Panned content still crosses to the other ear.
            let cross = self.level * (delayed_r - delayed_l);
            samples[li] = (left + cross) as f32;
            samples[ri] = (right - cross) as f32;
        }
    }
}

pub fn builtin_presets() -> Vec<CrossfeedPreset> {
    let p = |name: &str, level_db: f32| CrossfeedPreset {
        name: name.to_string(),
        level_db,
        cutoff_hz: DEFAULT_CUTOFF_HZ,
        delay_us: DEFAULT_DELAY_US,
    };
    vec![
        p("Off", 0.0),
        p("Light", -6.0),
        p("Medium", -4.5),
        p("Strong", -3.0),
    ]
}

/// Load custom crossfeed presets from `~/.config/keet/crossfeed/*.json`
/// (or `%APPDATA%\keet\crossfeed\` on Windows), mirroring the EQ and effects
/// preset folders. This is where the depth lives: a preset may set any of
/// level, cutoff and ITD.
pub fn load_custom_presets() -> Vec<CrossfeedPreset> {
    crate::config::load_presets("crossfeed", |p: &mut CrossfeedPreset| &mut p.name)
}

#[cfg(test)]
mod crossfeed_tests {
    use super::*;

    #[test]
    fn preset_json_parses_all_three_parameters_with_defaults() {
        let full = r#"{"name":"Wide","level_db":-5.0,"cutoff_hz":900.0,"delay_us":420.0}"#;
        let p: CrossfeedPreset = serde_json::from_str(full).unwrap();
        assert_eq!(p.name, "Wide");
        assert_eq!(p.level_db, -5.0);
        assert_eq!(p.cutoff_hz, 900.0);
        assert_eq!(p.delay_us, 420.0);

        // Omitted fields fall back to the classic Meier values, so a minimal
        // preset is still a valid one.
        let minimal = r#"{"name":"Just Level","level_db":-6.0}"#;
        let p: CrossfeedPreset = serde_json::from_str(minimal).unwrap();
        assert_eq!(p.cutoff_hz, DEFAULT_CUTOFF_HZ);
        assert_eq!(p.delay_us, DEFAULT_DELAY_US);
    }

    #[test]
    fn itd_parameter_sets_the_actual_inter_channel_delay() {
        // 1000 us at 48 kHz = 48 samples of delay before the crossfed signal
        // reaches the opposite channel.
        let sr = 48000.0f32;
        let mut cf = CrossfeedFilter::new();
        cf.load_preset(
            &CrossfeedPreset {
                name: "test".into(),
                level_db: 0.0,
                cutoff_hz: 700.0,
                delay_us: 1000.0,
            },
            sr,
        );

        // Impulse in L only; R must stay silent until the delay elapses.
        let frames = 200;
        let mut buf = vec![0.0f32; frames * 2];
        buf[0] = 1.0;
        cf.process_stereo(&mut buf);

        let right: Vec<f32> = (0..frames).map(|i| buf[i * 2 + 1]).collect();
        assert!(
            right[..48].iter().all(|s| s.abs() < 1e-9),
            "R leaked before the 48-sample ITD"
        );
        assert!(right[48].abs() > 1e-6, "R silent after the ITD elapsed");
    }

    fn run(preset: CrossfeedPreset, sr: f32, l: impl Fn(usize) -> f32, r: impl Fn(usize) -> f32, frames: usize) -> Vec<f32> {
        let mut cf = CrossfeedFilter::new();
        cf.load_preset(&preset, sr);
        let mut buf: Vec<f32> = (0..frames).flat_map(|i| [l(i), r(i)]).collect();
        cf.process_stereo(&mut buf);
        buf
    }

    fn strong() -> CrossfeedPreset {
        builtin_presets().into_iter().find(|p| p.name == "Strong").unwrap()
    }

    #[test]
    fn centred_content_passes_at_its_own_level() {
        // Crossfeed used to ADD the filtered opposite channel to each side, so
        // anything centred (bass, vocals) rose by 20·log10(1 + level): +4.6 dB
        // of bass at "Strong", which then fed the limiter. Mixing only the
        // stereo difference leaves a mono signal exactly as it came in.
        let sr = 48000.0;
        let tone = |i: usize| 0.5 * (2.0 * std::f32::consts::PI * 100.0 * i as f32 / sr).sin();
        let out = run(strong(), sr, tone, tone, 9600);
        let worst = (0..9600).map(|i| (out[i * 2] - tone(i)).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "mono changed by up to {worst}");
    }

    #[test]
    fn hard_panned_bass_still_crosses_to_the_other_ear() {
        let sr = 48000.0;
        let tone = |i: usize| 0.5 * (2.0 * std::f32::consts::PI * 100.0 * i as f32 / sr).sin();
        let out = run(strong(), sr, tone, |_| 0.0, 9600);
        let peak_r = (4800..9600).map(|i| out[i * 2 + 1].abs()).fold(0.0f32, f32::max);
        // Strong is -3 dB: about 0.7 of the low-passed left reaches the right.
        assert!(peak_r > 0.25 && peak_r < 0.4, "right peak {peak_r}");
    }

    #[test]
    fn out_of_range_preset_values_cannot_destabilise_the_filter() {
        // A cutoff above half the sample rate made the low-pass unstable:
        // the output ran to inf, then NaN, and the limiter turned NaN into
        // silence for good. Custom presets are user-editable JSON.
        let sr = 22050.0;
        let wild = CrossfeedPreset { name: "wild".into(), level_db: 40.0, cutoff_hz: 12000.0, delay_us: 300.0 };
        let noise = |i: usize| (((i * 7919) % 1000) as f32 / 500.0 - 1.0) * 0.5;
        let out = run(wild, sr, noise, |i| -noise(i), 44100);
        assert!(out.iter().all(|s| s.is_finite() && s.abs() < 4.0), "filter ran away");
    }

    #[test]
    fn builtin_presets_keep_their_classic_meier_values() {
        let p = builtin_presets();
        assert_eq!(p[0].name, "Off");
        for preset in p.iter().skip(1) {
            assert_eq!(preset.cutoff_hz, DEFAULT_CUTOFF_HZ);
            assert_eq!(preset.delay_us, DEFAULT_DELAY_US);
            assert!(preset.level_db < 0.0);
        }
    }
}
