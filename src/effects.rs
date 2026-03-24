use serde::Deserialize;

// --- Comb Filter (used by Freeverb) ---

struct CombFilter {
    buffer: Vec<f32>,
    index: usize,
    filter_store: f32,
    feedback: f32,
    damp1: f32,
    damp2: f32,
}

impl CombFilter {
    fn new(size: usize) -> Self {
        Self {
            buffer: vec![0.0; size],
            index: 0,
            filter_store: 0.0,
            feedback: 0.0,
            damp1: 0.0,
            damp2: 0.0,
        }
    }

    fn set_feedback(&mut self, val: f32) { self.feedback = val; }

    fn set_damp(&mut self, val: f32) {
        self.damp1 = val;
        self.damp2 = 1.0 - val;
    }

    fn process(&mut self, input: f32) -> f32 {
        let output = self.buffer[self.index];
        self.filter_store = output * self.damp2 + self.filter_store * self.damp1;
        self.buffer[self.index] = input + self.filter_store * self.feedback;
        self.index = (self.index + 1) % self.buffer.len();
        output
    }

    fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.filter_store = 0.0;
        self.index = 0;
    }
}

// --- Allpass Filter (used by Freeverb) ---

struct AllpassFilter {
    buffer: Vec<f32>,
    index: usize,
}

impl AllpassFilter {
    fn new(size: usize) -> Self {
        Self { buffer: vec![0.0; size], index: 0 }
    }

    fn process(&mut self, input: f32) -> f32 {
        let buffered = self.buffer[self.index];
        let output = -input + buffered;
        self.buffer[self.index] = input + buffered * 0.5;
        self.index = (self.index + 1) % self.buffer.len();
        output
    }

    fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.index = 0;
    }
}

// --- Freeverb ---

// Comb filter tunings (at 44100Hz, scaled to actual sample rate)
const COMB_TUNINGS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_TUNINGS: [usize; 4] = [556, 441, 341, 225];
const STEREO_SPREAD: usize = 23;

pub struct Freeverb {
    combs_l: Vec<CombFilter>,
    combs_r: Vec<CombFilter>,
    allpasses_l: Vec<AllpassFilter>,
    allpasses_r: Vec<AllpassFilter>,
    wet: f32,
    dry: f32,
    width: f32,
}

impl Freeverb {
    fn new(sample_rate: f32) -> Self {
        let scale = sample_rate / 44100.0;
        let combs_l: Vec<_> = COMB_TUNINGS.iter()
            .map(|&t| CombFilter::new(((t as f32) * scale) as usize))
            .collect();
        let combs_r: Vec<_> = COMB_TUNINGS.iter()
            .map(|&t| CombFilter::new(((t as f32) * scale) as usize + STEREO_SPREAD))
            .collect();
        let allpasses_l: Vec<_> = ALLPASS_TUNINGS.iter()
            .map(|&t| AllpassFilter::new(((t as f32) * scale) as usize))
            .collect();
        let allpasses_r: Vec<_> = ALLPASS_TUNINGS.iter()
            .map(|&t| AllpassFilter::new(((t as f32) * scale) as usize + STEREO_SPREAD))
            .collect();

        Self { combs_l, combs_r, allpasses_l, allpasses_r, wet: 0.0, dry: 1.0, width: 1.0 }
    }

    fn set_params(&mut self, room_size: f32, damping: f32, wet: f32, dry: f32, width: f32) {
        let feedback = room_size * 0.28 + 0.7; // scale to 0.7-0.98 range
        for comb in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) {
            comb.set_feedback(feedback);
            comb.set_damp(damping);
        }
        self.wet = wet;
        self.dry = dry;
        self.width = width;
    }

    fn reset(&mut self) {
        for c in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) { c.reset(); }
        for a in self.allpasses_l.iter_mut().chain(self.allpasses_r.iter_mut()) { a.reset(); }
    }

    fn process_stereo(&mut self, samples: &mut [f32]) {
        let wet1 = self.wet * (1.0 + self.width) / 2.0;
        let wet2 = self.wet * (1.0 - self.width) / 2.0;

        let frames = samples.len() / 2;
        for frame in 0..frames {
            let li = frame * 2;
            let ri = frame * 2 + 1;
            let input = (samples[li] + samples[ri]) * 0.5; // mono input to reverb

            let mut out_l = 0.0f32;
            let mut out_r = 0.0f32;

            for comb in &mut self.combs_l { out_l += comb.process(input); }
            for comb in &mut self.combs_r { out_r += comb.process(input); }

            // Scale comb sum (8 filters) to prevent amplification
            const FIXED_GAIN: f32 = 0.015;
            out_l *= FIXED_GAIN;
            out_r *= FIXED_GAIN;

            for ap in &mut self.allpasses_l { out_l = ap.process(out_l); }
            for ap in &mut self.allpasses_r { out_r = ap.process(out_r); }

            samples[li] = samples[li] * self.dry + out_l * wet1 + out_r * wet2;
            samples[ri] = samples[ri] * self.dry + out_r * wet1 + out_l * wet2;
        }
    }
}

// --- Chorus ---

pub struct Chorus {
    delay_l: Vec<f32>,
    delay_r: Vec<f32>,
    write_idx: usize,
    phase: f32,
    rate: f32,         // LFO Hz
    depth: f32,        // modulation depth in samples
    wet: f32,
    sample_rate: f32,
}

impl Chorus {
    fn new(sample_rate: f32) -> Self {
        let max_delay = (sample_rate * 0.05) as usize; // 50ms max
        Self {
            delay_l: vec![0.0; max_delay],
            delay_r: vec![0.0; max_delay],
            write_idx: 0,
            phase: 0.0,
            rate: 1.0,
            depth: 0.0,
            wet: 0.0,
            sample_rate,
        }
    }

    fn set_params(&mut self, rate: f32, depth: f32, wet: f32) {
        self.rate = rate;
        self.depth = depth * self.sample_rate * 0.001; // convert ms to samples
        self.wet = wet;
    }

    fn reset(&mut self) {
        self.delay_l.fill(0.0);
        self.delay_r.fill(0.0);
        self.write_idx = 0;
        self.phase = 0.0;
    }

    fn process_stereo(&mut self, samples: &mut [f32]) {
        let buf_len = self.delay_l.len();
        let phase_inc = self.rate / self.sample_rate;
        let base_delay = buf_len as f32 / 2.0;

        let frames = samples.len() / 2;
        for frame in 0..frames {
            let li = frame * 2;
            let ri = frame * 2 + 1;

            // Write to delay buffer
            self.delay_l[self.write_idx] = samples[li];
            self.delay_r[self.write_idx] = samples[ri];

            // LFO (sine) - right channel offset by 90 degrees
            let lfo_l = (self.phase * 2.0 * std::f32::consts::PI).sin();
            let lfo_r = ((self.phase + 0.25) * 2.0 * std::f32::consts::PI).sin();

            // Read with modulated delay (linear interpolation)
            let delay_l = base_delay + lfo_l * self.depth;
            let delay_r = base_delay + lfo_r * self.depth;

            let read_l = self.read_interpolated(&self.delay_l, delay_l);
            let read_r = self.read_interpolated(&self.delay_r, delay_r);

            samples[li] = samples[li] * (1.0 - self.wet) + read_l * self.wet;
            samples[ri] = samples[ri] * (1.0 - self.wet) + read_r * self.wet;

            self.write_idx = (self.write_idx + 1) % buf_len;
            self.phase = (self.phase + phase_inc) % 1.0;
        }
    }

    fn read_interpolated(&self, buf: &[f32], delay: f32) -> f32 {
        let buf_len = buf.len() as f32;
        let read_pos = self.write_idx as f32 - delay;
        let read_pos = if read_pos < 0.0 { read_pos + buf_len } else { read_pos };
        let idx0 = read_pos as usize % buf.len();
        let idx1 = (idx0 + 1) % buf.len();
        let frac = read_pos.fract();
        buf[idx0] * (1.0 - frac) + buf[idx1] * frac
    }
}

// --- Delay ---

pub struct Delay {
    buffer_l: Vec<f32>,
    buffer_r: Vec<f32>,
    write_idx: usize,
    delay_samples: usize,
    feedback: f32,
    wet: f32,
}

impl Delay {
    fn new(sample_rate: f32) -> Self {
        let max_delay = (sample_rate * 2.0) as usize; // 2 seconds max
        Self {
            buffer_l: vec![0.0; max_delay],
            buffer_r: vec![0.0; max_delay],
            write_idx: 0,
            delay_samples: 0,
            feedback: 0.0,
            wet: 0.0,
        }
    }

    fn set_params(&mut self, delay_ms: f32, feedback: f32, wet: f32, sample_rate: f32) {
        self.delay_samples = ((delay_ms * sample_rate / 1000.0) as usize).min(self.buffer_l.len() - 1);
        self.feedback = feedback.min(0.95);
        self.wet = wet;
    }

    fn reset(&mut self) {
        self.buffer_l.fill(0.0);
        self.buffer_r.fill(0.0);
        self.write_idx = 0;
    }

    fn process_stereo(&mut self, samples: &mut [f32]) {
        if self.delay_samples == 0 { return; }
        let buf_len = self.buffer_l.len();

        let frames = samples.len() / 2;
        for frame in 0..frames {
            let li = frame * 2;
            let ri = frame * 2 + 1;

            let read_idx = (self.write_idx + buf_len - self.delay_samples) % buf_len;
            let delayed_l = self.buffer_l[read_idx];
            let delayed_r = self.buffer_r[read_idx];

            self.buffer_l[self.write_idx] = samples[li] + delayed_l * self.feedback;
            self.buffer_r[self.write_idx] = samples[ri] + delayed_r * self.feedback;

            samples[li] = samples[li] * (1.0 - self.wet) + delayed_l * self.wet;
            samples[ri] = samples[ri] * (1.0 - self.wet) + delayed_r * self.wet;

            self.write_idx = (self.write_idx + 1) % buf_len;
        }
    }
}

// --- Effects Preset Parameters ---

#[derive(Deserialize, Clone, Default)]
pub struct ReverbParams {
    #[serde(default = "default_room")]
    pub room_size: f32,
    #[serde(default)]
    pub damping: f32,
    #[serde(default = "default_wet")]
    pub wet: f32,
    #[serde(default = "default_dry")]
    pub dry: f32,
    #[serde(default = "default_width")]
    pub width: f32,
}

fn default_room() -> f32 { 0.5 }
fn default_wet() -> f32 { 0.5 }
fn default_dry() -> f32 { 0.85 }
fn default_width() -> f32 { 1.0 }

#[derive(Deserialize, Clone, Default)]
pub struct ChorusParams {
    #[serde(default = "default_rate")]
    pub rate: f32,
    #[serde(default = "default_depth")]
    pub depth: f32,
    #[serde(default = "default_chorus_wet")]
    pub wet: f32,
}

fn default_rate() -> f32 { 1.0 }
fn default_depth() -> f32 { 5.0 }
fn default_chorus_wet() -> f32 { 0.3 }

#[derive(Deserialize, Clone, Default)]
pub struct DelayParams {
    #[serde(default)]
    pub delay_ms: f32,
    #[serde(default)]
    pub feedback: f32,
    #[serde(default)]
    pub wet: f32,
}

#[derive(Deserialize, Clone)]
pub struct EffectsPreset {
    pub name: String,
    #[serde(default)]
    pub reverb: Option<ReverbParams>,
    #[serde(default)]
    pub chorus: Option<ChorusParams>,
    #[serde(default)]
    pub delay: Option<DelayParams>,
}

// --- Effects Chain ---

pub struct EffectsChain {
    reverb: Freeverb,
    chorus: Chorus,
    delay: Delay,
    has_reverb: bool,
    has_chorus: bool,
    has_delay: bool,
}

impl EffectsChain {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            reverb: Freeverb::new(sample_rate),
            chorus: Chorus::new(sample_rate),
            delay: Delay::new(sample_rate),
            has_reverb: false,
            has_chorus: false,
            has_delay: false,
        }
    }

    pub fn load_preset(&mut self, preset: &EffectsPreset, sample_rate: f32) {
        self.has_reverb = false;
        self.has_chorus = false;
        self.has_delay = false;

        if let Some(ref r) = preset.reverb {
            self.reverb = Freeverb::new(sample_rate);
            self.reverb.set_params(r.room_size, r.damping, r.wet, r.dry, r.width);
            self.has_reverb = true;
        }
        if let Some(ref c) = preset.chorus {
            self.chorus = Chorus::new(sample_rate);
            self.chorus.set_params(c.rate, c.depth, c.wet);
            self.has_chorus = true;
        }
        if let Some(ref d) = preset.delay {
            if d.delay_ms > 0.0 {
                self.delay = Delay::new(sample_rate);
                self.delay.set_params(d.delay_ms, d.feedback, d.wet, sample_rate);
                self.has_delay = true;
            }
        }
    }

    pub fn reset(&mut self) {
        self.reverb.reset();
        self.chorus.reset();
        self.delay.reset();
    }

    pub fn is_active(&self) -> bool {
        self.has_reverb || self.has_chorus || self.has_delay
    }

    /// Process interleaved stereo samples: chorus -> delay -> reverb
    pub fn process_stereo(&mut self, samples: &mut [f32]) {
        if self.has_chorus { self.chorus.process_stereo(samples); }
        if self.has_delay { self.delay.process_stereo(samples); }
        if self.has_reverb { self.reverb.process_stereo(samples); }

        // Safety limiter: prevent effects from exceeding 0dBFS
        let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        if peak > 1.0 {
            let scale = 1.0 / peak;
            for s in samples.iter_mut() {
                *s *= scale;
            }
        }
    }
}

/// Built-in effects presets
pub fn builtin_presets() -> Vec<EffectsPreset> {
    vec![
        EffectsPreset {
            name: "None".to_string(),
            reverb: None,
            chorus: None,
            delay: None,
        },
        EffectsPreset {
            name: "Small Room".to_string(),
            reverb: Some(ReverbParams { room_size: 0.3, damping: 0.8, wet: 0.4, dry: 0.85, width: 0.8 }),
            chorus: None,
            delay: None,
        },
        EffectsPreset {
            name: "Concert Hall".to_string(),
            reverb: Some(ReverbParams { room_size: 0.85, damping: 0.5, wet: 0.7, dry: 0.75, width: 1.0 }),
            chorus: None,
            delay: None,
        },
        EffectsPreset {
            name: "Cathedral".to_string(),
            reverb: Some(ReverbParams { room_size: 0.95, damping: 0.3, wet: 0.6, dry: 0.7, width: 1.0 }),
            chorus: None,
            delay: Some(DelayParams { delay_ms: 300.0, feedback: 0.2, wet: 0.1 }),
        },
        EffectsPreset {
            name: "Studio".to_string(),
            reverb: Some(ReverbParams { room_size: 0.25, damping: 0.85, wet: 0.35, dry: 0.9, width: 0.6 }),
            chorus: None,
            delay: None,
        },
        EffectsPreset {
            name: "Chorus".to_string(),
            reverb: None,
            chorus: Some(ChorusParams { rate: 1.2, depth: 5.0, wet: 0.35 }),
            delay: None,
        },
        EffectsPreset {
            name: "Echo".to_string(),
            reverb: None,
            chorus: None,
            delay: Some(DelayParams { delay_ms: 400.0, feedback: 0.4, wet: 0.25 }),
        },
    ]
}

/// Load custom effects presets from ~/.config/keet/effects/*.json
pub fn load_custom_presets() -> Vec<EffectsPreset> {
    let dir = if cfg!(target_os = "windows") {
        std::env::var("APPDATA").ok().map(|p| std::path::PathBuf::from(p).join("keet").join("effects"))
    } else {
        std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".config").join("keet").join("effects"))
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
                    if let Ok(preset) = serde_json::from_str::<EffectsPreset>(&contents) {
                        presets.push(preset);
                    }
                }
            }
        }
    }
    presets.sort_by(|a, b| a.name.cmp(&b.name));
    presets
}
