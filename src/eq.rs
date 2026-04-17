use serde::Deserialize;

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

/// A single EQ band definition from JSON
#[derive(Deserialize, Clone)]
pub struct EqBand {
    pub freq: f32,
    pub gain: f32,
    #[serde(default = "default_q")]
    pub q: f32,
}

fn default_q() -> f32 { 1.0 }

/// An EQ preset loaded from JSON or built-in
#[derive(Deserialize, Clone)]
pub struct EqPreset {
    pub name: String,
    pub bands: Vec<EqBand>,
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
        self.filters.clear();
        let mut has_nonzero = false;
        for band in &preset.bands {
            if band.gain.abs() >= 0.01 {
                has_nonzero = true;
            }
            self.filters.push(FilterBand {
                coeffs: BiquadCoeffs::peaking_eq(band.freq, band.gain, band.q, sample_rate),
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
                f.state_l.y1 = out_l;
                left = out_l;

                let out_r = f.coeffs.b0 * right
                          + f.coeffs.b1 * f.state_r.x1
                          + f.coeffs.b2 * f.state_r.x2
                          - f.coeffs.a1 * f.state_r.y1
                          - f.coeffs.a2 * f.state_r.y2;
                f.state_r.x2 = f.state_r.x1;
                f.state_r.x1 = right;
                f.state_r.y2 = f.state_r.y1;
                f.state_r.y1 = out_r;
                right = out_r;
            }

            samples[li] = left;
            samples[ri] = right;
        }
    }
}

/// Render a compact EQ curve visualization for the status line
/// Shows gain per band using block characters: ▁▂▃▄▅▆▇█ for boost, underline for cut
pub fn render_eq_curve(preset: &EqPreset) -> String {
    use crate::state::{C_RESET, C_DIM, C_CYAN, C_GREEN, C_YELLOW, C_RED};

    if preset.bands.is_empty() {
        return String::new();
    }

    // Display 20 log-spaced points across 20Hz-20kHz, interpolating gain from all bands
    let n_points = 20;
    let bars: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    let mut result = format!("  {C_DIM}EQ:{C_RESET} ");

    for i in 0..n_points {
        // Log-spaced frequency from 20Hz to 20kHz
        let t = i as f32 / (n_points - 1) as f32;
        let freq = 20.0 * (1000.0f32).powf(t); // 20 * 10^(t*3) = 20..20000

        // Sum contributions from all bands using bell curve (peaking EQ response)
        let mut gain = 0.0f32;
        for band in &preset.bands {
            let q = band.q;
            // Distance in octaves between display freq and band center
            let octaves = (freq / band.freq).log2();
            // Bell curve shaped by Q (higher Q = narrower)
            let weight = (-octaves * octaves * q * q * 2.0).exp();
            gain += band.gain * weight;
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

/// Built-in presets
pub fn builtin_presets() -> Vec<EqPreset> {
    vec![
        EqPreset {
            name: "Flat".to_string(),
            bands: vec![],
        },
        EqPreset {
            name: "Bass Boost".to_string(),
            bands: vec![
                EqBand { freq: 32.0, gain: 6.0, q: 0.8 },
                EqBand { freq: 64.0, gain: 5.0, q: 0.8 },
                EqBand { freq: 125.0, gain: 3.0, q: 1.0 },
                EqBand { freq: 250.0, gain: 1.0, q: 1.0 },
            ],
        },
        EqPreset {
            name: "Treble Boost".to_string(),
            bands: vec![
                EqBand { freq: 4000.0, gain: 2.0, q: 1.0 },
                EqBand { freq: 8000.0, gain: 4.0, q: 1.0 },
                EqBand { freq: 16000.0, gain: 5.0, q: 0.8 },
            ],
        },
        EqPreset {
            name: "Vocal".to_string(),
            bands: vec![
                EqBand { freq: 125.0, gain: -2.0, q: 1.0 },
                EqBand { freq: 1000.0, gain: 3.0, q: 0.8 },
                EqBand { freq: 2000.0, gain: 4.0, q: 0.8 },
                EqBand { freq: 4000.0, gain: 3.0, q: 1.0 },
                EqBand { freq: 8000.0, gain: 1.0, q: 1.0 },
            ],
        },
        EqPreset {
            name: "Loudness".to_string(),
            bands: vec![
                EqBand { freq: 32.0, gain: 4.0, q: 0.8 },
                EqBand { freq: 64.0, gain: 3.0, q: 0.8 },
                EqBand { freq: 125.0, gain: 1.0, q: 1.0 },
                EqBand { freq: 8000.0, gain: 2.0, q: 1.0 },
                EqBand { freq: 16000.0, gain: 3.0, q: 0.8 },
            ],
        },
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
