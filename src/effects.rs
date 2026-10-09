use serde::Deserialize;

// --- Comb Filter (used by Freeverb) ---

#[derive(Clone)]
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
        // Flush the feedback paths: during silence these decay into denormal
        // range, where x86 float ops are 10-100x slower (classic freeverb
        // "undenormalise" fix).
        self.filter_store =
            crate::eq::flush_denormal(output * self.damp2 + self.filter_store * self.damp1);
        self.buffer[self.index] =
            crate::eq::flush_denormal(input + self.filter_store * self.feedback);
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

#[derive(Clone)]
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
        self.buffer[self.index] = crate::eq::flush_denormal(input + buffered * 0.5);
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
/// Freeverb's `scaledamp`: the damping parameter is scaled by this before it
/// reaches the combs' low-pass. Applied raw, damping 1.0 freezes the
/// low-pass at zero and cuts the comb feedback entirely — no tail.
const DAMP_SCALE: f32 = 0.4;
/// Limiter release time constant: 2000 samples at 44.1 kHz, in seconds so it
/// is the same at every rate (a per-sample step would recover 4.35x faster at
/// 192 kHz).
const LIMITER_RELEASE_SECS: f32 = 2000.0 / 44_100.0;

#[derive(Clone)]
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
        // The L/R offset is a time like the tunings, so it scales with them.
        let spread = ((STEREO_SPREAD as f32) * scale).round() as usize;
        let combs_l: Vec<_> = COMB_TUNINGS.iter()
            .map(|&t| CombFilter::new(((t as f32) * scale) as usize))
            .collect();
        let combs_r: Vec<_> = COMB_TUNINGS.iter()
            .map(|&t| CombFilter::new(((t as f32) * scale) as usize + spread))
            .collect();
        let allpasses_l: Vec<_> = ALLPASS_TUNINGS.iter()
            .map(|&t| AllpassFilter::new(((t as f32) * scale) as usize))
            .collect();
        let allpasses_r: Vec<_> = ALLPASS_TUNINGS.iter()
            .map(|&t| AllpassFilter::new(((t as f32) * scale) as usize + spread))
            .collect();

        Self { combs_l, combs_r, allpasses_l, allpasses_r, wet: 0.0, dry: 1.0, width: 1.0 }
    }

    // Every parameter is clamped: presets are user-editable JSON, and an
    // out-of-range value (room_size > ~1.07 puts comb feedback at >= 1) makes
    // the filters run away to inf and then NaN (see the test at the bottom).
    fn set_params(&mut self, room_size: f32, damping: f32, wet: f32, dry: f32, width: f32) {
        let feedback = room_size.clamp(0.0, 1.0) * 0.28 + 0.7; // scale to 0.7-0.98 range
        let damping = damping.clamp(0.0, 1.0);
        let (wet, dry, width) = (wet.clamp(0.0, 1.0), dry.clamp(0.0, 1.0), width.clamp(0.0, 1.0));
        for comb in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) {
            comb.set_feedback(feedback);
            comb.set_damp(damping * DAMP_SCALE);
        }
        self.wet = wet;
        self.dry = dry;
        self.width = width;
    }

    fn reset(&mut self) {
        for c in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) { c.reset(); }
        for a in self.allpasses_l.iter_mut().chain(self.allpasses_r.iter_mut()) { a.reset(); }
    }

    // Level calibration, deliberately not Jezar's: the reference feeds the
    // combs (L + R)·0.015 and scales `wet` by 3; this takes the stereo AVERAGE
    // and no 3× — a given `wet` is about a sixth as loud as in the reference.
    // The built-in presets are tuned by ear on this scale, and existing custom
    // presets were written against it, so matching the reference would mean
    // rescaling every preset value to sound the same. Values copied from
    // another Freeverb need `wet` ×6 here for the same level.
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

#[derive(Clone)]
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
        self.rate = rate.clamp(0.0, 20.0);
        // Depth must keep the modulated read inside the delay line, which is
        // centred on half its length.
        let max_depth = (self.delay_l.len() as f32 / 2.0 - 1.0).max(0.0);
        self.depth = (depth.max(0.0) * self.sample_rate * 0.001).min(max_depth); // ms -> samples
        self.wet = wet.clamp(0.0, 1.0);
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

#[derive(Clone)]
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
        self.delay_samples = ((delay_ms.max(0.0) * sample_rate / 1000.0) as usize).min(self.buffer_l.len() - 1);
        // Both signs: feedback below -1 grows just as surely as above +1.
        self.feedback = feedback.clamp(-0.95, 0.95);
        self.wet = wet.clamp(0.0, 1.0);
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

            self.buffer_l[self.write_idx] =
                crate::eq::flush_denormal(samples[li] + delayed_l * self.feedback);
            self.buffer_r[self.write_idx] =
                crate::eq::flush_denormal(samples[ri] + delayed_r * self.feedback);

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

#[derive(Clone)]
pub struct EffectsChain {
    /// The preset being faded away from after a change (see fade.rs): its
    /// reverb or delay tail fades out instead of being cut.
    xfade: crate::fade::Crossfade<EffectsChain>,
    reverb: Option<Freeverb>,
    chorus: Option<Chorus>,
    delay: Option<Delay>,
    /// Smoothed safety-limiter gain (1.0 = no reduction). Persisted across buffers
    /// so the gain doesn't jump discontinuously between `process_stereo` calls.
    limiter_gain: f32,
    /// Per-frame release step for this sample rate (see LIMITER_RELEASE_SECS).
    limiter_release: f32,
}

impl EffectsChain {
    pub fn new(sample_rate: f32) -> Self {
        // Lazy: skip the ~44KB reverb/chorus/delay allocations until a preset that
        // needs them is loaded. The "None" preset keeps this at zero overhead.
        let limiter_release = 1.0 - (-1.0 / (LIMITER_RELEASE_SECS * sample_rate.max(1.0))).exp();
        Self { xfade: Default::default(), reverb: None, chorus: None, delay: None, limiter_gain: 1.0, limiter_release }
    }

    pub fn load_preset(&mut self, preset: &EffectsPreset, sample_rate: f32) {
        let before = self.clone();
        let was_active = before.is_active();
        self.xfade.retire(before, crate::fade::frames(crate::fade::EFFECTS_FADE_SECS, sample_rate));
        self.reverb = None;
        self.chorus = None;
        self.delay = None;

        if let Some(ref r) = preset.reverb {
            let mut rv = Freeverb::new(sample_rate);
            rv.set_params(r.room_size, r.damping, r.wet, r.dry, r.width);
            self.reverb = Some(rv);
        }
        if let Some(ref c) = preset.chorus {
            let mut ch = Chorus::new(sample_rate);
            ch.set_params(c.rate, c.depth, c.wet);
            self.chorus = Some(ch);
        }
        if let Some(ref d) = preset.delay {
            if d.delay_ms > 0.0 {
                let mut dl = Delay::new(sample_rate);
                dl.set_params(d.delay_ms, d.feedback, d.wet, sample_rate);
                self.delay = Some(dl);
            }
        }
        // None to None is no change: no fade (see EqChain::load_bands).
        if !was_active && self.reverb.is_none() && self.chorus.is_none() && self.delay.is_none() {
            self.xfade.cancel();
        }
    }

    pub fn reset(&mut self) {
        self.xfade.clear();
        self.limiter_gain = 1.0;
        if let Some(r) = self.reverb.as_mut() { r.reset(); }
        if let Some(c) = self.chorus.as_mut() { c.reset(); }
        if let Some(d) = self.delay.as_mut() { d.reset(); }
    }

    /// Audio passed this stage while it was off (see `fade::Crossfade::idle`).
    pub fn idle(&mut self) {
        self.xfade.idle();
    }

    pub fn is_active(&self) -> bool {
        self.reverb.is_some() || self.chorus.is_some() || self.delay.is_some() || self.xfade.running()
    }

    /// Process interleaved stereo samples: chorus -> delay -> reverb
    pub fn process_stereo(&mut self, samples: &mut [f32]) {
        // Always through the fade: it also records that audio has flowed
        // (a fresh stage takes new settings without one), and with no fade
        // running it just calls `process_preset`.
        let mut xf = std::mem::take(&mut self.xfade);
        xf.run(samples, |s| self.process_preset(s), |old, s| old.process_preset(s));
        self.xfade = xf;
    }

    fn process_preset(&mut self, samples: &mut [f32]) {
        if let Some(c) = self.chorus.as_mut() { c.process_stereo(samples); }
        if let Some(d) = self.delay.as_mut() { d.process_stereo(samples); }
        if let Some(r) = self.reverb.as_mut() { r.process_stereo(samples); }
    }

    /// Whether the output limiter has work to do for a block peaking at
    /// `peak`: something is over full scale, or the gain is still recovering.
    pub fn limiter_engaged(&self, peak: f32) -> bool {
        peak > 1.0 || self.limiter_gain < 1.0 || !peak.is_finite()
    }

    /// Output safety limiter, run by the producer as the LAST stage of the DSP
    /// chain (the state lives here because the chain already persists across
    /// buffers and tracks). Keeps output at or below 0 dBFS without the
    /// per-buffer gain jumps a brickwall causes (audible zipper/pumping).
    /// Instant attack guarantees no sample exceeds 0 dBFS; slow per-sample
    /// release lets the gain recover smoothly across buffers. A non-finite
    /// sample is replaced with silence — NaN passes straight through `.clamp`
    /// and must never reach the DAC.
    pub fn limit_output(&mut self, samples: &mut [f32]) {
        for frame in samples.chunks_mut(2) {
            for s in frame.iter_mut() {
                if !s.is_finite() {
                    *s = 0.0;
                }
            }
            let peak = frame.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
            let target = if peak > 1.0 { 1.0 / peak } else { 1.0 };
            if target < self.limiter_gain {
                self.limiter_gain = target; // instant attack — never clip
            } else {
                self.limiter_gain += (target - self.limiter_gain) * self.limiter_release;
            }
            for s in frame.iter_mut() {
                *s *= self.limiter_gain;
            }
        }
        // The release approaches 1.0 asymptotically; snap once inaudibly close
        // so the limiter disengages and the chain goes back to zero-copy.
        if self.limiter_gain > 0.9999 {
            self.limiter_gain = 1.0;
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
    crate::config::load_presets("effects", |p: &mut EffectsPreset| &mut p.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_effects_off_fades_the_tail_instead_of_cutting_it() {
        let sr = 48000.0;
        let presets = builtin_presets();
        let hall = presets.iter().find(|p| p.reverb.is_some()).unwrap();
        let mut fx = EffectsChain::new(sr);
        fx.load_preset(hall, sr);
        let mut buf: Vec<f32> = (0..9600).flat_map(|i| { let v = if i < 4800 { ((i % 50) as f32 / 25.0 - 1.0) * 0.5 } else { 0.0 }; [v, v] }).collect();
        fx.process_stereo(&mut buf);
        let last = buf[buf.len() - 2];
        assert!(last.abs() > 1e-4, "the reverb tail should still be ringing");
        fx.load_preset(&presets[0], sr); // None
        let mut silence = vec![0.0f32; 2 * 4800];
        fx.process_stereo(&mut silence);
        assert!((silence[0] - last).abs() < 0.05, "tail cut: {last} then {}", silence[0]);
        assert!(silence.iter().any(|s| s.abs() > 1e-5), "the tail fades out over time");
    }

    fn preset(json: &str) -> EffectsPreset {
        serde_json::from_str(json).expect("preset json")
    }

    #[test]
    fn limiter_recovers_in_the_same_time_at_every_sample_rate() {
        // The release was a fixed per-SAMPLE step, so at 192 kHz the gain
        // came back 4.35x faster than at 44.1 kHz (10 ms instead of 45).
        let gain_after_20ms = |rate: f32| {
            let mut fx = EffectsChain::new(rate);
            let mut hit = vec![2.0f32, 2.0];
            fx.limit_output(&mut hit); // gain drops to 0.5
            let mut quiet = vec![0.1f32; 2 * (rate * 0.02) as usize];
            fx.limit_output(&mut quiet);
            fx.limiter_gain
        };
        let (a, b) = (gain_after_20ms(44_100.0), gain_after_20ms(192_000.0));
        assert!((a - b).abs() < 0.01, "44.1k: {a}, 192k: {b}");
    }

    #[test]
    fn full_damping_still_leaves_a_reverb_tail() {
        // Freeverb scales damping by 0.4 before it reaches the combs. Used
        // raw, damping 1.0 froze each comb's low-pass at zero and the tail
        // died: a "reverb" that only produced its dry signal.
        let p = preset(r#"{"name":"damp","reverb":{"room_size":0.8,"damping":1.0,"wet":1.0,"dry":0.0,"width":1.0}}"#);
        let mut fx = EffectsChain::new(44_100.0);
        fx.load_preset(&p, 44_100.0);
        let mut buf = vec![0.0f32; 2 * 22_050];
        buf[0] = 1.0;
        buf[1] = 1.0;
        fx.process_stereo(&mut buf);
        let tail: f32 = buf[2 * 13_230..].iter().map(|s| s * s).sum(); // from 300 ms
        assert!(tail > 1e-4, "no tail: energy {tail}");
    }

    #[test]
    fn out_of_range_custom_presets_cannot_blow_up() {
        // A hand-edited preset with room_size > ~1.07 pushed comb feedback to
        // >= 1, and a delay feedback below -1 was never clamped: the filters
        // ran away to inf, the limiter's inf * 0 made NaN, and NaN reached the
        // DAC (it sails through `.clamp`). Every parameter must be bounded.
        let p = preset(
            r#"{"name":"broken",
                "reverb":{"room_size":5.0,"damping":-2.0,"wet":3.0,"dry":4.0,"width":9.0},
                "chorus":{"rate":-50.0,"depth":400.0,"wet":7.0},
                "delay":{"delay_ms":300.0,"feedback":-3.0,"wet":5.0}}"#,
        );
        let mut fx = EffectsChain::new(48_000.0);
        fx.load_preset(&p, 48_000.0);
        let mut buf = vec![0.0f32; 4_096];
        for block in 0..600 {
            for (i, s) in buf.iter_mut().enumerate() {
                *s = if block == 0 && i < 2 { 1.0 } else { 0.25 * ((i as f32) * 0.05).sin() };
            }
            fx.process_stereo(&mut buf);
            assert!(buf.iter().all(|s| s.is_finite()), "non-finite output at block {block}");
            fx.limit_output(&mut buf);
            assert!(buf.iter().all(|s| s.abs() <= 1.0 + 1e-6), "limiter let a sample past 0 dBFS");
        }
    }
}
