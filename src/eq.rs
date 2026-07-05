use serde::Deserialize;

/// Flush near-zero values to exactly zero. IIR feedback paths (biquads, comb
/// filters) decay into denormal range during silence, and denormal arithmetic
/// is 10-100x slower on x86 — a CPU spike right when the player goes quiet.
/// 1e-30 is ~-600 dB, far below audibility, while still comfortably above the
/// f32 denormal threshold (~1.2e-38).
#[inline]
pub(crate) fn flush_denormal(x: f32) -> f32 {
    if x.abs() < 1e-30 { 0.0 } else { x }
}

/// Single biquad filter state (2nd-order IIR) per channel
#[derive(Clone)]
struct BiquadState {
    x1: f32, x2: f32,
    y1: f32, y2: f32,
}

impl BiquadState {
    fn new() -> Self {
        Self { x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    fn reset(&mut self) {
        self.x1 = 0.0; self.x2 = 0.0;
        self.y1 = 0.0; self.y2 = 0.0;
    }
}

/// Biquad filter coefficients (normalized, a0 = 1.0)
#[derive(Clone)]
struct BiquadCoeffs {
    b0: f32, b1: f32, b2: f32,
    a1: f32, a2: f32,
}

impl BiquadCoeffs {
    /// Peaking EQ filter from Audio EQ Cookbook (Robert Bristow-Johnson)
    fn peaking_eq(freq: f32, gain_db: f32, q: f32, sample_rate: f32) -> Self {
        if gain_db.abs() < 0.01 {
            return Self { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };
        }

        let a = 10.0f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let sin_w0 = w0.sin();
        let cos_w0 = w0.cos();
        let alpha = sin_w0 / (2.0 * q);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha / a;

        Self {
            b0: b0 / a0, b1: b1 / a0, b2: b2 / a0,
            a1: a1 / a0, a2: a2 / a0,
        }
    }
}

/// Fixed 10-band graphic-EQ centres (ISO octave, Hz). Each band is a peaking
/// filter at its centre with a fixed ~1-octave Q; only the gain is adjustable.
pub const EQ_FREQS: [f32; 10] =
    [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
/// Band count.
pub const EQ_BANDS: usize = 10;
/// Fixed Q per band (~1 octave bandwidth).
pub const EQ_Q: f32 = 1.41;
/// Gain limit (±dB) — the graphic-EQ hardware standard.
pub const EQ_GAIN_LIMIT: f32 = 12.0;

/// Short freq labels for the editor / curve (aligned with `EQ_FREQS`).
pub const EQ_FREQ_LABELS: [&str; 10] =
    ["31", "62", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];

/// An EQ preset: a name + one gain (dB) per band. Loaded from JSON or built-in.
#[derive(Deserialize, Clone)]
pub struct EqPreset {
    pub name: String,
    #[serde(default)]
    pub gains: Vec<f32>,
}

impl EqPreset {
    /// Gains normalised to exactly `EQ_BANDS` values — short lists zero-padded,
    /// long lists truncated — and clamped to the ±limit.
    pub fn gains_10(&self) -> [f32; EQ_BANDS] {
        let mut g = [0.0f32; EQ_BANDS];
        for (i, slot) in g.iter_mut().enumerate() {
            *slot = self
                .gains
                .get(i)
                .copied()
                .unwrap_or(0.0)
                .clamp(-EQ_GAIN_LIMIT, EQ_GAIN_LIMIT);
        }
        g
    }
}

/// One filter per band per channel (stereo)
struct FilterBand {
    coeffs: BiquadCoeffs,
    state_l: BiquadState,
    state_r: BiquadState,
}

/// The runtime EQ processor
pub struct EqChain {
    filters: Vec<FilterBand>,
    active: bool,
}

impl EqChain {
    pub fn new() -> Self {
        Self { filters: Vec::new(), active: false }
    }

    pub fn load_preset(&mut self, preset: &EqPreset, sample_rate: f32) {
        self.load_gains(&preset.gains_10(), sample_rate);
    }

    /// Build the fixed-band peaking filters from a gain-per-band array — the
    /// graphic EQ's single source of truth (presets and the live editor both
    /// feed this).
    pub fn load_gains(&mut self, gains: &[f32; EQ_BANDS], sample_rate: f32) {
        self.filters.clear();
        let mut has_nonzero = false;
        for (i, &gain) in gains.iter().enumerate() {
            if gain.abs() >= 0.01 {
                has_nonzero = true;
            }
            self.filters.push(FilterBand {
                coeffs: BiquadCoeffs::peaking_eq(EQ_FREQS[i], gain, EQ_Q, sample_rate),
                state_l: BiquadState::new(),
                state_r: BiquadState::new(),
            });
        }
        self.active = has_nonzero;
    }

    pub fn reset(&mut self) {
        for f in &mut self.filters {
            f.state_l.reset();
            f.state_r.reset();
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Process interleaved stereo samples in-place
    pub fn process_stereo(&mut self, samples: &mut [f32]) {
        if !self.active || self.filters.is_empty() {
            return;
        }

        let frames = samples.len() / 2;
        for frame in 0..frames {
            let li = frame * 2;
            let ri = frame * 2 + 1;
            let mut left = samples[li];
            let mut right = samples[ri];

            for f in &mut self.filters {
                let out_l = f.coeffs.b0 * left
                          + f.coeffs.b1 * f.state_l.x1
                          + f.coeffs.b2 * f.state_l.x2
                          - f.coeffs.a1 * f.state_l.y1
                          - f.coeffs.a2 * f.state_l.y2;
                f.state_l.x2 = f.state_l.x1;
                f.state_l.x1 = left;
                f.state_l.y2 = f.state_l.y1;
                // Flush the feedback state: during silence y decays into
                // denormal range, where x86 float ops are 10-100x slower.
                f.state_l.y1 = flush_denormal(out_l);
                left = out_l;

                let out_r = f.coeffs.b0 * right
                          + f.coeffs.b1 * f.state_r.x1
                          + f.coeffs.b2 * f.state_r.x2
                          - f.coeffs.a1 * f.state_r.y1
                          - f.coeffs.a2 * f.state_r.y2;
                f.state_r.x2 = f.state_r.x1;
                f.state_r.x1 = right;
                f.state_r.y2 = f.state_r.y1;
                f.state_r.y1 = flush_denormal(out_r);
                right = out_r;
            }

            samples[li] = left;
            samples[ri] = right;
        }
    }
}

/// Render a compact EQ curve visualization for the status line (Classic banner).
/// Shows gain per band using block characters: ▁▂▃▄▅▆▇█ for boost, `·` for flat.
/// Draws from the live 10-band gains, so it reflects preset changes AND edits.
pub fn render_eq_curve(gains: &[f32; EQ_BANDS]) -> String {
    use crate::state::{C_RESET, C_DIM, C_CYAN, C_GREEN, C_YELLOW, C_RED};

    if gains.iter().all(|g| g.abs() < 0.01) {
        return String::new();
    }

    // Display 20 log-spaced points across 20Hz-20kHz, interpolating gain from the
    // 10 fixed bands with a ~1-octave bell (matches the graphic EQ's Q).
    let n_points = 20;
    let bars: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    let mut result = format!("  {C_DIM}EQ:{C_RESET} ");

    for i in 0..n_points {
        // Log-spaced frequency from 20Hz to 20kHz
        let t = i as f32 / (n_points - 1) as f32;
        let freq = 20.0 * (1000.0f32).powf(t); // 20 * 10^(t*3) = 20..20000

        // Sum contributions from all bands using a bell curve (peaking response).
        let mut gain = 0.0f32;
        for (b, &band_gain) in gains.iter().enumerate() {
            let octaves = (freq / EQ_FREQS[b]).log2();
            let weight = (-octaves * octaves * EQ_Q * EQ_Q * 2.0).exp();
            gain += band_gain * weight;
        }

        let (ch, color) = if gain > 0.1 {
            let idx = ((gain / 8.0) * 8.0).clamp(1.0, 8.0) as usize;
            let color = if gain > 5.0 { C_RED } else if gain > 3.0 { C_YELLOW } else { C_GREEN };
            (bars[idx], color)
        } else if gain < -0.1 {
            let idx = ((-gain / 8.0) * 8.0).clamp(1.0, 8.0) as usize;
            (bars[idx], C_CYAN)
        } else {
            ('·', C_DIM)
        };

        result.push_str(&format!("{}{}", color, ch));
    }
    result.push_str(C_RESET);
    result
}

/// Built-in presets as 10-band gain shapes (dB). Bands: 31 62 125 250 500 1k 2k
/// 4k 8k 16k — see `EQ_FREQS`. These approximate the old curated peaking curves.
pub fn builtin_presets() -> Vec<EqPreset> {
    let p = |name: &str, gains: [f32; EQ_BANDS]| EqPreset {
        name: name.to_string(),
        gains: gains.to_vec(),
    };
    vec![
        p("Flat", [0.0; EQ_BANDS]),
        p("Bass Boost", [6.0, 5.0, 3.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        p("Treble Boost", [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 4.0, 5.0]),
        p("Vocal", [0.0, 0.0, -2.0, 0.0, 0.0, 3.0, 4.0, 3.0, 1.0, 0.0]),
        p("Loudness", [4.0, 3.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 2.0, 3.0]),
    ]
}

/// Load custom presets from ~/.config/keet/eq/*.json (or %APPDATA%\keet\eq\ on Windows)
pub fn load_custom_presets() -> Vec<EqPreset> {
    let dir = if cfg!(target_os = "windows") {
        std::env::var("APPDATA").ok().map(|p| std::path::PathBuf::from(p).join("keet").join("eq"))
    } else {
        std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".config").join("keet").join("eq"))
    };

    let dir = match dir {
        Some(d) if d.is_dir() => d,
        _ => return Vec::new(),
    };

    let mut presets = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Ok(preset) = serde_json::from_str::<EqPreset>(&contents) {
                        presets.push(preset);
                    }
                }
            }
        }
    }
    presets.sort_by(|a, b| a.name.cmp(&b.name));
    presets
}

#[cfg(test)]
mod denormal_tests {
    use super::*;

    #[test]
    fn flush_denormal_zeroes_tiny_values_keeps_audio() {
        assert_eq!(flush_denormal(1e-32), 0.0);
        assert_eq!(flush_denormal(-1e-35), 0.0);
        assert_eq!(flush_denormal(0.5), 0.5);
        assert_eq!(flush_denormal(-0.2), -0.2);
        assert_eq!(flush_denormal(0.0), 0.0);
    }

    #[test]
    fn gains_10_pads_truncates_and_clamps() {
        // Short list zero-pads to 10.
        let p = EqPreset { name: "x".into(), gains: vec![3.0, -2.0] };
        assert_eq!(p.gains_10(), [3.0, -2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        // Long list truncates to 10.
        let p = EqPreset { name: "x".into(), gains: vec![1.0; 12] };
        assert_eq!(p.gains_10().len(), 10);
        // Out-of-range clamps to ±limit.
        let p = EqPreset { name: "x".into(), gains: vec![99.0, -99.0] };
        assert_eq!(p.gains_10()[0], EQ_GAIN_LIMIT);
        assert_eq!(p.gains_10()[1], -EQ_GAIN_LIMIT);
    }

    #[test]
    fn builtin_presets_are_ten_band_shapes() {
        let presets = builtin_presets();
        let by = |n: &str| presets.iter().find(|p| p.name == n).unwrap().gains_10();
        // Flat is silent.
        assert_eq!(by("Flat"), [0.0; 10]);
        // Bass Boost lifts the low bands, leaves the top flat.
        let bass = by("Bass Boost");
        assert!(bass[0] > 0.0 && bass[1] > 0.0);
        assert_eq!(bass[9], 0.0);
        // Treble Boost lifts the top, leaves the bottom flat.
        let treble = by("Treble Boost");
        assert!(treble[8] > 0.0 && treble[9] > 0.0);
        assert_eq!(treble[0], 0.0);
    }

    #[test]
    fn load_gains_active_only_when_a_band_is_nonzero() {
        let mut eq = EqChain::new();
        eq.load_gains(&[0.0; 10], 48000.0);
        assert!(!eq.is_active());
        let mut g = [0.0f32; 10];
        g[3] = 6.0;
        eq.load_gains(&g, 48000.0);
        assert!(eq.is_active());
    }
}
