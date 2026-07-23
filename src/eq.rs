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

    /// Low shelf (RBJ cookbook): boost/cut everything below `freq`.
    fn low_shelf(freq: f32, gain_db: f32, q: f32, sample_rate: f32) -> Self {
        let a = 10.0f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        let b0 = a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha);
        let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0);
        let a2 = (a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha;

        Self {
            b0: b0 / a0, b1: b1 / a0, b2: b2 / a0,
            a1: a1 / a0, a2: a2 / a0,
        }
    }

    /// High shelf (RBJ cookbook): boost/cut everything above `freq`.
    fn high_shelf(freq: f32, gain_db: f32, q: f32, sample_rate: f32) -> Self {
        let a = 10.0f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        let b0 = a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha);
        let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_w0);
        let a2 = (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha;

        Self {
            b0: b0 / a0, b1: b1 / a0, b2: b2 / a0,
            a1: a1 / a0, a2: a2 / a0,
        }
    }

    /// 2nd-order high-pass (RBJ cookbook) — the "low cut". Q > 0.707 adds a
    /// resonant bump at the corner, like an analog filter's emphasis.
    fn high_pass(freq: f32, q: f32, sample_rate: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let b0 = (1.0 + cos_w0) / 2.0;
        let b1 = -(1.0 + cos_w0);
        let b2 = (1.0 + cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0, b1: b1 / a0, b2: b2 / a0,
            a1: a1 / a0, a2: a2 / a0,
        }
    }

    /// 2nd-order low-pass (RBJ cookbook) — the "high cut".
    fn low_pass(freq: f32, q: f32, sample_rate: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let b0 = (1.0 - cos_w0) / 2.0;
        let b1 = 1.0 - cos_w0;
        let b2 = (1.0 - cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0, b1: b1 / a0, b2: b2 / a0,
            a1: a1 / a0, a2: a2 / a0,
        }
    }

    /// Coefficients for one parametric band, or None when the band is a no-op
    /// (peak/shelf at ~0 dB). Freq is clamped away from DC and Nyquist so the
    /// bilinear designs stay stable at any output rate.
    fn for_band(band: &BandSettings, sample_rate: f32) -> Option<Self> {
        let freq = band.freq.clamp(EQ_FREQ_MIN, (sample_rate * 0.45).min(EQ_FREQ_MAX));
        let q = band.q.clamp(EQ_Q_MIN, EQ_Q_MAX);
        let gain = band.gain.clamp(-EQ_GAIN_LIMIT, EQ_GAIN_LIMIT);
        if band.kind.uses_gain() && gain.abs() < 0.01 {
            return None;
        }
        Some(match band.kind {
            BandType::Peak => Self::peaking_eq(freq, gain, q, sample_rate),
            BandType::LowShelf => Self::low_shelf(freq, gain, q, sample_rate),
            BandType::HighShelf => Self::high_shelf(freq, gain, q, sample_rate),
            BandType::LowCut => Self::high_pass(freq, q, sample_rate),
            BandType::HighCut => Self::low_pass(freq, q, sample_rate),
        })
    }

    /// Exact magnitude response (dB) at `freq`: |H(e^jω)| with
    /// H(z) = (b0 + b1·z⁻¹ + b2·z⁻²) / (1 + a1·z⁻¹ + a2·z⁻²).
    fn response_at(&self, freq: f32, sample_rate: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let nr = self.b0 + self.b1 * c1 + self.b2 * c2;
        let ni = -(self.b1 * s1 + self.b2 * s2);
        let dr = 1.0 + self.a1 * c1 + self.a2 * c2;
        let di = -(self.a1 * s1 + self.a2 * s2);
        let mag2 = (nr * nr + ni * ni) / (dr * dr + di * di).max(1e-20);
        10.0 * mag2.max(1e-20).log10()
    }
}

/// Summed exact response (dB) of a band set at `freq` — drives the curve
/// visualizations, so what's drawn is what the audio actually gets.
pub fn response_db(bands: &[BandSettings], freq: f32, sample_rate: f32) -> f32 {
    bands
        .iter()
        .filter_map(|b| BiquadCoeffs::for_band(b, sample_rate))
        .map(|c| c.response_at(freq, sample_rate))
        .sum()
}

/// Default band centres (ISO octave, Hz) — the graphic-EQ layout every band
/// starts from. Parametric bands may move their frequency freely.
pub const EQ_FREQS: [f32; 10] =
    [31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0];
/// Band count (fixed: the lock-free shared state is sized by it).
pub const EQ_BANDS: usize = 10;
/// Default Q (~1 octave bandwidth).
pub const EQ_Q: f32 = 1.41;
/// Gain limit (±dB) — the graphic-EQ hardware standard.
pub const EQ_GAIN_LIMIT: f32 = 12.0;
/// Band frequency range (Hz).
pub const EQ_FREQ_MIN: f32 = 20.0;
pub const EQ_FREQ_MAX: f32 = 20000.0;
/// Q range: 0.3 (very broad) to 10 (surgical notch).
pub const EQ_Q_MIN: f32 = 0.3;
pub const EQ_Q_MAX: f32 = 10.0;
/// Editor key-press step ratios: freq moves ⅓ octave, Q steps by √2.
pub const EQ_FREQ_STEP: f32 = 1.259_921;
pub const EQ_Q_STEP: f32 = std::f32::consts::SQRT_2;

/// Filter shape of one parametric band (RBJ Audio EQ Cookbook designs).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandType {
    #[default]
    Peak = 0,
    LowShelf = 1,
    HighShelf = 2,
    /// High-pass: removes everything below the band frequency.
    LowCut = 3,
    /// Low-pass: removes everything above the band frequency.
    HighCut = 4,
}

impl BandType {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Total: any byte maps to a valid type (unknown → Peak).
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => BandType::LowShelf,
            2 => BandType::HighShelf,
            3 => BandType::LowCut,
            4 => BandType::HighCut,
            _ => BandType::Peak,
        }
    }

    /// Step `dir` (+1/-1) through the five types, wrapping.
    pub fn cycled(self, dir: i32) -> Self {
        Self::from_u8((self.as_u8() as i32 + dir).rem_euclid(5) as u8)
    }

    /// Cuts filter by frequency alone; gain applies to peak and shelves only.
    pub fn uses_gain(self) -> bool {
        matches!(self, BandType::Peak | BandType::LowShelf | BandType::HighShelf)
    }

    /// Stable snake_case name — matches the preset JSON spelling.
    pub fn name(self) -> &'static str {
        match self {
            BandType::Peak => "peak",
            BandType::LowShelf => "low_shelf",
            BandType::HighShelf => "high_shelf",
            BandType::LowCut => "low_cut",
            BandType::HighCut => "high_cut",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        [
            BandType::Peak,
            BandType::LowShelf,
            BandType::HighShelf,
            BandType::LowCut,
            BandType::HighCut,
        ]
        .into_iter()
        .find(|k| k.name() == name)
    }

    /// Short label for the editor's type row.
    pub fn short_label(self) -> &'static str {
        match self {
            BandType::Peak => "PK",
            BandType::LowShelf => "LS",
            BandType::HighShelf => "HS",
            BandType::LowCut => "LC",
            BandType::HighCut => "HC",
        }
    }
}

/// One parametric band: filter type, centre/corner frequency, gain, Q.
/// This is the universal EQ currency — presets, the live shared state, and the
/// filter chain all speak it.
#[derive(Clone, Copy, PartialEq, Debug, Deserialize)]
pub struct BandSettings {
    #[serde(rename = "type", default)]
    pub kind: BandType,
    pub freq: f32,
    #[serde(default)]
    pub gain: f32,
    #[serde(default = "default_band_q")]
    pub q: f32,
}

fn default_band_q() -> f32 {
    EQ_Q
}

impl BandSettings {
    /// The no-op state of slot `i`: a 0 dB peak at the slot's ISO centre.
    pub fn inert(slot: usize) -> Self {
        Self {
            kind: BandType::Peak,
            freq: EQ_FREQS[slot.min(EQ_BANDS - 1)],
            gain: 0.0,
            q: EQ_Q,
        }
    }

    /// Copy with every field clamped to its legal range.
    pub fn clamped(self) -> Self {
        Self {
            kind: self.kind,
            freq: self.freq.clamp(EQ_FREQ_MIN, EQ_FREQ_MAX),
            gain: self.gain.clamp(-EQ_GAIN_LIMIT, EQ_GAIN_LIMIT),
            q: self.q.clamp(EQ_Q_MIN, EQ_Q_MAX),
        }
    }

    /// Whether the band changes the signal (cuts always do; peak/shelf need gain).
    pub fn is_effective(&self) -> bool {
        !self.kind.uses_gain() || self.gain.abs() >= 0.01
    }
}

/// Compact frequency label for the editor: "31", "250", "1k", "3.3k", "16k".
pub fn format_freq(freq: f32) -> String {
    if freq >= 10000.0 {
        format!("{:.0}k", freq / 1000.0)
    } else if freq >= 1000.0 {
        let k = freq / 1000.0;
        if (k - k.round()).abs() < 0.05 {
            format!("{:.0}k", k)
        } else {
            format!("{:.1}k", k)
        }
    } else {
        format!("{:.0}", freq)
    }
}

/// An EQ preset loaded from JSON or built-in. Two forms:
/// - legacy graphic: `gains` = one dB value per fixed ISO band
/// - parametric: `bands` = up to 10 of `{type, freq, gain, q}` (AutoEq-style);
///   takes precedence over `gains` when present
#[derive(Deserialize, Clone)]
pub struct EqPreset {
    pub name: String,
    #[serde(default)]
    pub gains: Vec<f32>,
    #[serde(default)]
    pub bands: Vec<BandSettings>,
    /// Flat gain (dB) applied before the filters — AutoEq presets ship a
    /// negative preamp so boosted bands can't clip. 0 = none; clamped ±12.
    #[serde(default)]
    pub preamp: f32,
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

    /// The preset as exactly `EQ_BANDS` parametric bands: the `bands` form
    /// clamped and padded with inert slots, or the legacy `gains` mapped onto
    /// peaking filters at the ISO centres.
    pub fn bands_10(&self) -> [BandSettings; EQ_BANDS] {
        if self.bands.is_empty() {
            let gains = self.gains_10();
            return std::array::from_fn(|i| BandSettings {
                kind: BandType::Peak,
                freq: EQ_FREQS[i],
                gain: gains[i],
                q: EQ_Q,
            });
        }
        std::array::from_fn(|i| {
            self.bands
                .get(i)
                .map(|b| b.clamped())
                .unwrap_or_else(|| BandSettings::inert(i))
        })
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
    /// Linear pre-filter gain (from the preset's preamp dB; 1.0 = none).
    pre: f32,
    active: bool,
}

impl EqChain {
    pub fn new() -> Self {
        Self { filters: Vec::new(), pre: 1.0, active: false }
    }

    pub fn load_preset(&mut self, preset: &EqPreset, sample_rate: f32) {
        self.load_bands(&preset.bands_10(), preset.preamp, sample_rate);
    }

    /// Build the filter chain from a parametric band set — the EQ's single
    /// source of truth (presets and the live editor both feed this). No-op
    /// bands (peak/shelf at 0 dB) are skipped entirely, so a mostly-flat set
    /// only pays for the bands that do something. `preamp_db` is a flat gain
    /// ahead of the filters (clamped ±12 dB) — headroom for boosted bands.
    pub fn load_bands(&mut self, bands: &[BandSettings; EQ_BANDS], preamp_db: f32, sample_rate: f32) {
        self.filters.clear();
        for band in bands {
            if let Some(coeffs) = BiquadCoeffs::for_band(band, sample_rate) {
                self.filters.push(FilterBand {
                    coeffs,
                    state_l: BiquadState::new(),
                    state_r: BiquadState::new(),
                });
            }
        }
        let preamp_db = preamp_db.clamp(-EQ_GAIN_LIMIT, EQ_GAIN_LIMIT);
        self.pre = if preamp_db.abs() < 0.01 {
            1.0
        } else {
            10.0f32.powf(preamp_db / 20.0)
        };
        self.active = !self.filters.is_empty() || self.pre != 1.0;
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
        // Note: active with zero filters is legal — a preamp-only chain.
        if !self.active {
            return;
        }

        let frames = samples.len() / 2;
        for frame in 0..frames {
            let li = frame * 2;
            let ri = frame * 2 + 1;
            let mut left = samples[li] * self.pre;
            let mut right = samples[ri] * self.pre;

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
/// Shows the summed response using block characters: ▁▂▃▄▅▆▇█ for boost, `·`
/// for flat. Evaluates the actual biquad responses (|H(e^jω)|), so shelves and
/// cuts draw truthfully — what's shown is what the audio gets.
pub fn render_eq_curve(bands: &[BandSettings; EQ_BANDS], sample_rate: f32) -> String {
    use crate::state::{C_RESET, C_DIM, C_CYAN, C_GREEN, C_YELLOW, C_RED};

    if bands.iter().all(|b| !b.is_effective()) {
        return String::new();
    }

    // Display 20 log-spaced points across 20Hz-20kHz.
    let n_points = 20;
    let bars: &[char] = &[' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    let mut result = format!("  {C_DIM}EQ:{C_RESET} ");

    for i in 0..n_points {
        // Log-spaced frequency from 20Hz to 20kHz
        let t = i as f32 / (n_points - 1) as f32;
        let freq = 20.0 * (1000.0f32).powf(t); // 20 * 10^(t*3) = 20..20000

        let gain = response_db(bands, freq, sample_rate);

        // One glyph step per dB of summed gain, capped at the 8-step block ramp.
        let (ch, color) = if gain > 0.1 {
            let idx = gain.clamp(1.0, 8.0) as usize;
            let color = if gain > 5.0 { C_RED } else if gain > 3.0 { C_YELLOW } else { C_GREEN };
            (bars[idx], color)
        } else if gain < -0.1 {
            let idx = (-gain).clamp(1.0, 8.0) as usize;
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
        bands: Vec::new(),
        preamp: 0.0,
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

    fn gains_preset(gains: Vec<f32>) -> EqPreset {
        EqPreset { name: "x".into(), gains, bands: Vec::new(), preamp: 0.0 }
    }

    #[test]
    fn gains_10_pads_truncates_and_clamps() {
        // Short list zero-pads to 10.
        let p = gains_preset(vec![3.0, -2.0]);
        assert_eq!(p.gains_10(), [3.0, -2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        // Long list truncates to 10.
        let p = gains_preset(vec![1.0; 12]);
        assert_eq!(p.gains_10().len(), 10);
        // Out-of-range clamps to ±limit.
        let p = gains_preset(vec![99.0, -99.0]);
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
    fn legacy_preset_active_only_when_a_gain_is_nonzero() {
        let mut eq = EqChain::new();
        eq.load_preset(&gains_preset(vec![0.0; 10]), 48000.0);
        assert!(!eq.is_active());
        let mut g = vec![0.0f32; 10];
        g[3] = 6.0;
        eq.load_preset(&gains_preset(g), 48000.0);
        assert!(eq.is_active());
    }
}

#[cfg(test)]
mod parametric_tests {
    use super::*;

    fn band(kind: BandType, freq: f32, gain: f32, q: f32) -> BandSettings {
        BandSettings { kind, freq, gain, q }
    }

    /// One-band response probe.
    fn resp(b: BandSettings, at: f32) -> f32 {
        response_db(&[b], at, 48000.0)
    }

    #[test]
    fn shelf_and_cut_responses_match_rbj_shapes() {
        // Low shelf +6 @ 100 Hz: plateau below, flat above.
        let ls = band(BandType::LowShelf, 100.0, 6.0, 0.9);
        assert!((resp(ls, 25.0) - 6.0).abs() < 0.5, "low shelf plateau: {}", resp(ls, 25.0));
        assert!(resp(ls, 5000.0).abs() < 0.3, "low shelf top: {}", resp(ls, 5000.0));

        // High shelf +6 @ 5 kHz: mirror image.
        let hs = band(BandType::HighShelf, 5000.0, 6.0, 0.9);
        assert!((resp(hs, 16000.0) - 6.0).abs() < 1.0, "high shelf plateau: {}", resp(hs, 16000.0));
        assert!(resp(hs, 100.0).abs() < 0.3, "high shelf bottom: {}", resp(hs, 100.0));

        // Low cut (high-pass) @ 200 Hz: −12 dB/oct below, −3 dB at fc (Q .707).
        let lc = band(BandType::LowCut, 200.0, 0.0, 0.707);
        assert!(resp(lc, 50.0) < -20.0, "low cut slope: {}", resp(lc, 50.0));
        assert!(resp(lc, 2000.0).abs() < 0.5, "low cut passband: {}", resp(lc, 2000.0));
        let at_fc = resp(lc, 200.0);
        assert!(at_fc < -2.0 && at_fc > -4.5, "low cut at fc: {at_fc}");

        // High cut (low-pass) @ 2 kHz.
        let hc = band(BandType::HighCut, 2000.0, 0.0, 0.707);
        assert!(resp(hc, 16000.0) < -30.0, "high cut slope: {}", resp(hc, 16000.0));
        assert!(resp(hc, 200.0).abs() < 0.5, "high cut passband: {}", resp(hc, 200.0));

        // Peak keeps its exact centre gain.
        let pk = band(BandType::Peak, 1000.0, 6.0, EQ_Q);
        assert!((resp(pk, 1000.0) - 6.0).abs() < 0.2, "peak centre: {}", resp(pk, 1000.0));
        assert!(resp(pk, 4000.0).abs() < 1.5, "peak skirt: {}", resp(pk, 4000.0));
    }

    #[test]
    fn band_type_cycles_through_all_five_and_wraps() {
        let mut k = BandType::Peak;
        for _ in 0..5 {
            k = k.cycled(1);
        }
        assert_eq!(k, BandType::Peak, "5 steps must wrap to start");
        assert_eq!(BandType::Peak.cycled(-1), BandType::HighCut);
        for v in 0..=255u8 {
            let _ = BandType::from_u8(v); // total: no panic on any byte
        }
        assert_eq!(BandType::from_u8(BandType::LowShelf.as_u8()), BandType::LowShelf);
        assert_eq!(BandType::from_name(BandType::HighCut.name()), Some(BandType::HighCut));
    }

    #[test]
    fn preset_bands_json_parses_pads_and_clamps() {
        let json = r#"{
            "name": "AutoEq",
            "bands": [
                {"type": "low_shelf", "freq": 105.0, "gain": 5.5, "q": 0.71},
                {"type": "peak", "freq": 3300.0, "gain": -2.0},
                {"type": "high_cut", "freq": 10000.0},
                {"type": "peak", "freq": 5.0, "gain": 99.0, "q": 100.0}
            ]
        }"#;
        let p: EqPreset = serde_json::from_str(json).unwrap();
        let b = p.bands_10();
        assert_eq!(b[0].kind, BandType::LowShelf);
        assert_eq!(b[0].freq, 105.0);
        assert_eq!(b[0].gain, 5.5);
        assert_eq!(b[0].q, 0.71);
        // Omitted q falls back to the graphic default.
        assert_eq!(b[1].kind, BandType::Peak);
        assert_eq!(b[1].q, EQ_Q);
        // Cuts parse with gain defaulting to 0.
        assert_eq!(b[2].kind, BandType::HighCut);
        assert_eq!(b[2].gain, 0.0);
        // Out-of-range values clamp.
        assert_eq!(b[3].freq, EQ_FREQ_MIN);
        assert_eq!(b[3].gain, EQ_GAIN_LIMIT);
        assert_eq!(b[3].q, EQ_Q_MAX);
        // Unused slots load inert: Peak at the slot's ISO centre, 0 dB.
        assert_eq!(b[7].kind, BandType::Peak);
        assert_eq!(b[7].freq, EQ_FREQS[7]);
        assert_eq!(b[7].gain, 0.0);
    }

    #[test]
    fn legacy_gains_preset_maps_to_peak_iso_bands() {
        let json = r#"{"name": "old", "gains": [3.0, -2.0]}"#;
        let p: EqPreset = serde_json::from_str(json).unwrap();
        let b = p.bands_10();
        for (i, band) in b.iter().enumerate() {
            assert_eq!(band.kind, BandType::Peak);
            assert_eq!(band.freq, EQ_FREQS[i]);
            assert_eq!(band.q, EQ_Q);
        }
        assert_eq!(b[0].gain, 3.0);
        assert_eq!(b[1].gain, -2.0);
        assert_eq!(b[2].gain, 0.0);
    }

    #[test]
    fn load_bands_flat_is_inactive_but_a_cut_band_is_always_active() {
        let mut eq = EqChain::new();
        let flat: [BandSettings; EQ_BANDS] =
            std::array::from_fn(BandSettings::inert);
        eq.load_bands(&flat, 0.0, 48000.0);
        assert!(!eq.is_active(), "all-flat peak bands must be inactive");

        let mut with_cut = flat;
        with_cut[0] = band(BandType::LowCut, 100.0, 0.0, 0.707);
        eq.load_bands(&with_cut, 0.0, 48000.0);
        assert!(eq.is_active(), "a cut filters regardless of gain — must be active");
    }

    #[test]
    fn low_cut_attenuates_low_sine_passes_high() {
        let sr = 48000.0f32;
        let mut eq = EqChain::new();
        let mut bands: [BandSettings; EQ_BANDS] = std::array::from_fn(BandSettings::inert);
        bands[0] = band(BandType::LowCut, 500.0, 0.0, 0.707);
        eq.load_bands(&bands, 0.0, sr);

        let peak_after = |freq: f32, eq: &mut EqChain| -> f32 {
            let n = 4800; // 0.1 s
            let mut buf: Vec<f32> = (0..n)
                .flat_map(|i| {
                    let s = 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin();
                    [s, s]
                })
                .collect();
            eq.process_stereo(&mut buf);
            // Skip the first half: filter transient.
            buf[n..].iter().fold(0.0f32, |m, &s| m.max(s.abs()))
        };

        let low = peak_after(50.0, &mut eq);
        eq.reset();
        let high = peak_after(5000.0, &mut eq);
        assert!(low < 0.05, "50 Hz should be gutted by a 500 Hz low cut, peak={low}");
        assert!(high > 0.45, "5 kHz should pass nearly unity, peak={high}");
    }

    #[test]
    fn render_eq_curve_draws_cuts_not_just_gains() {
        let flat: [BandSettings; EQ_BANDS] = std::array::from_fn(BandSettings::inert);
        assert!(render_eq_curve(&flat, 48000.0).is_empty(), "flat set renders nothing");

        // A lone cut band has zero gain everywhere — the old gains-based curve
        // would have skipped it. It must render now.
        let mut cut_only = flat;
        cut_only[0] = band(BandType::LowCut, 200.0, 0.0, 0.707);
        assert!(
            !render_eq_curve(&cut_only, 48000.0).is_empty(),
            "cut-only set must draw a curve"
        );
    }

    #[test]
    fn preamp_parses_from_json_and_scales_the_signal() {
        // AutoEq-style preset: bands + negative preamp for clipping headroom.
        let json = r#"{
            "name": "with preamp",
            "preamp": -6.02,
            "bands": [{"type": "peak", "freq": 1000.0, "gain": 2.0}]
        }"#;
        let p: EqPreset = serde_json::from_str(json).unwrap();
        assert!((p.preamp - -6.02).abs() < 1e-6);

        // Preamp alone (flat bands) must still engage the chain and scale:
        // -6.02 dB is amplitude x0.5.
        let flat = r#"{"name": "pre only", "preamp": -6.02}"#;
        let p: EqPreset = serde_json::from_str(flat).unwrap();
        let mut eq = EqChain::new();
        eq.load_preset(&p, 48000.0);
        assert!(eq.is_active(), "nonzero preamp must activate the chain");
        let mut buf = vec![0.8f32; 32];
        eq.process_stereo(&mut buf);
        for s in &buf {
            assert!((s - 0.4).abs() < 0.002, "expected ~0.4 after -6.02 dB, got {s}");
        }

        // Absent preamp defaults to 0 dB and stays out of the way.
        let none = r#"{"name": "no pre", "gains": [0.0]}"#;
        let p: EqPreset = serde_json::from_str(none).unwrap();
        assert_eq!(p.preamp, 0.0);
        let mut eq = EqChain::new();
        eq.load_preset(&p, 48000.0);
        assert!(!eq.is_active(), "flat preset with no preamp stays inactive");
    }

    #[test]
    fn shipped_eq_example_assets_parse() {
        // The example presets in assets/ are user-facing documentation — they
        // must always load through the real deserializer.
        let load = |f: &str| -> EqPreset {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(f);
            let s = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{f}: {e}"));
            serde_json::from_str(&s).unwrap_or_else(|e| panic!("{f} must parse: {e}"))
        };

        // The graphic example shows a real (non-flat) gain shape.
        let graphic = load("assets/eq-example.json");
        assert!(graphic.bands_10().iter().any(|b| b.is_effective()));

        // The parametric example is a fill-in template: all 10 bands spelled
        // out at their graphic defaults, plus the preamp field.
        let parametric = load("assets/eq-parametric-example.json");
        assert_eq!(parametric.bands.len(), EQ_BANDS, "template lists every slot");
        assert_eq!(parametric.preamp, 0.0, "template shows the preamp field");
        for (i, b) in parametric.bands_10().iter().enumerate() {
            assert_eq!(*b, BandSettings::inert(i), "band {i} at graphic default");
        }
    }

    #[test]
    fn format_freq_is_compact_for_editor_labels() {
        assert_eq!(format_freq(31.0), "31");
        assert_eq!(format_freq(250.0), "250");
        assert_eq!(format_freq(1000.0), "1k");
        assert_eq!(format_freq(3300.0), "3.3k");
        assert_eq!(format_freq(16000.0), "16k");
    }
}
