//! Click-free preset changes for the DSP stages (EQ, effects, crossfeed).
//!
//! Swapping a stage's settings outright is a step in the output: a crossfeed
//! or effects preset rebuilt its filters and delay lines from silence (cutting
//! a reverb tail dead), and an EQ band switched on started from zero filter
//! state, a thump on a loud signal. A stage that changes keeps a copy of its
//! old self here; for a short while both run on the same input and the output
//! moves linearly from the old one to the new one.

/// The retired copies of a stage and how far the fade has got.
pub(crate) struct Crossfade<T> {
    /// What is being faded away from: one copy, or — after a change during a
    /// fade — several, each with the weight it had in the blend at that
    /// moment (the weights sum to 1).
    old: Vec<(Box<T>, f32)>,
    /// Whether the stage has processed audio since its last reset. Only a
    /// running signal has a step to hide: a fresh stage (a new producer, or
    /// one just reset by a seek) takes new settings at once.
    live: bool,
    /// Frames faded so far, and the fade's length in frames.
    pos: usize,
    len: usize,
    scratch: Vec<f32>,
    mix: Vec<f32>,
}

/// Retired copies kept at most. Deliberate, both halves of it:
/// - up to this many copies plus the live stage run at once, i.e. five
///   reverbs during a burst of effects-preset presses. That lasts one fade
///   (300 ms after the last press) on the producer thread, which runs ~4 s
///   ahead of playback, so the extra work never reaches the audio callback.
/// - a press beyond this drops the copy with the smallest weight, and its
///   share of the blend vanishes in one sample: a step of at most that weight
///   times its difference from the rest. With five presses inside one fade
///   the faintest share is small, and the alternative (an unbounded list)
///   lets a held key grow the work without limit.
const MAX_RETIRED: usize = 4;

impl<T> Default for Crossfade<T> {
    fn default() -> Self {
        Self { old: Vec::new(), live: false, pos: 0, len: 0, scratch: Vec::new(), mix: Vec::new() }
    }
}

/// A stage's clone is its settings and state, never a fade in progress.
impl<T> Clone for Crossfade<T> {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl<T: Clone> Crossfade<T> {
    /// Start fading away from `current` (the stage as it is, BEFORE the change)
    /// over `frames`. A change during a fade continues from the blend being
    /// heard at that moment: the copies already fading keep their share of it
    /// and `current` takes the rest. Restarting from the OLDEST copy would
    /// jump back to it — a click on two quick preset presses.
    pub(crate) fn retire(&mut self, current: T, frames: usize) {
        if !self.live {
            return;
        }
        if self.old.is_empty() {
            self.old.push((Box::new(current), 1.0));
        } else {
            let t = self.progress();
            for (_, w) in &mut self.old {
                *w *= 1.0 - t;
            }
            self.old.push((Box::new(current), t));
            self.old.retain(|(_, w)| *w > 1e-4);
            while self.old.len() > MAX_RETIRED {
                let faintest = (0..self.old.len())
                    .min_by(|&a, &b| self.old[a].1.total_cmp(&self.old[b].1))
                    .expect("not empty");
                self.old.remove(faintest);
            }
            let total: f32 = self.old.iter().map(|(_, w)| w).sum();
            for (_, w) in &mut self.old {
                *w /= total;
            }
        }
        self.pos = 0;
        self.len = frames.max(1);
    }

    /// How far the fade has got, 0..=1.
    fn progress(&self) -> f32 {
        (self.pos as f32 / self.len.max(1) as f32).min(1.0)
    }

    /// The stage was skipped this chunk (it is off) but audio flowed past it:
    /// it counts as live, so turning it ON fades in. Without this a stage
    /// that was off never ran, never became live, and switched on with a step.
    pub(crate) fn idle(&mut self) {
        self.live = true;
    }

    pub(crate) fn running(&self) -> bool {
        !self.old.is_empty()
    }

    /// Drop the fade (the audio jumped: a seek, a skip); the stage is fresh.
    pub(crate) fn clear(&mut self) {
        self.old.clear();
        self.live = false;
    }

    /// Forget a fade just started without marking the stage fresh (the change
    /// turned out to be no change).
    pub(crate) fn cancel(&mut self) {
        self.old.clear();
    }

    /// Process interleaved stereo `samples` through every version and blend:
    /// `new` runs the stage as it is now, `old` each retired copy.
    pub(crate) fn run(
        &mut self,
        samples: &mut [f32],
        new: impl FnOnce(&mut [f32]),
        mut old: impl FnMut(&mut T, &mut [f32]),
    ) {
        self.live = true;
        if self.old.is_empty() {
            new(samples);
            return;
        }
        self.mix.clear();
        self.mix.resize(samples.len(), 0.0);
        for (retired, w) in &mut self.old {
            self.scratch.clear();
            self.scratch.extend_from_slice(samples);
            old(retired, &mut self.scratch);
            for (m, s) in self.mix.iter_mut().zip(&self.scratch) {
                *m += *w * s;
            }
        }
        new(samples);
        let frames = samples.len() / 2;
        for f in 0..frames {
            let t = ((self.pos + f) as f32 / self.len as f32).min(1.0);
            for c in 0..2 {
                let i = f * 2 + c;
                samples[i] = self.mix[i] * (1.0 - t) + samples[i] * t;
            }
        }
        self.pos += frames;
        if self.pos >= self.len {
            self.old.clear();
        }
    }
}

/// Fade lengths: long enough to hide the step, short enough to feel instant.
/// Effects get the longest, so a reverb or delay tail fades out instead of
/// being cut.
pub(crate) const EQ_FADE_SECS: f32 = 0.02;
pub(crate) const CROSSFEED_FADE_SECS: f32 = 0.03;
pub(crate) const EFFECTS_FADE_SECS: f32 = 0.3;

pub(crate) fn frames(secs: f32, sample_rate: f32) -> usize {
    (secs * sample_rate.max(1.0)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Gain(f32);

    /// A fade on a stage that has already processed audio.
    fn live() -> Crossfade<Gain> {
        let mut fade = Crossfade::<Gain>::default();
        fade.run(&mut [0.0, 0.0], |_| {}, |_, _| {});
        fade
    }

    #[test]
    fn a_fresh_stage_takes_new_settings_at_once() {
        let mut fade = Crossfade::<Gain>::default();
        fade.retire(Gain(1.0), 10);
        assert!(!fade.running(), "nothing has played yet: no step to hide");
        let mut fade = live();
        fade.clear();
        fade.retire(Gain(1.0), 10);
        assert!(!fade.running(), "a reset (seek, skip) makes the stage fresh again");
    }

    #[test]
    fn the_output_moves_linearly_from_old_to_new_then_the_old_is_dropped() {
        let mut fade = live();
        fade.retire(Gain(1.0), 4);
        let new = Gain(0.0);
        let mut buf = vec![1.0f32; 2 * 6];
        fade.run(&mut buf, |s| s.iter_mut().for_each(|x| *x *= new.0), |g, s| s.iter_mut().for_each(|x| *x *= g.0));
        let left: Vec<f32> = buf.iter().step_by(2).copied().collect();
        assert_eq!(left, [1.0, 0.75, 0.5, 0.25, 0.0, 0.0]);
        assert!(!fade.running(), "done after its length");
    }

    #[test]
    fn a_second_change_continues_from_the_blend_being_heard() {
        // Fade from 1.0 toward 0.0; halfway through (output 0.5), change
        // again to 0.25. The output must continue from 0.5, not jump back to
        // the oldest setting's 1.0 (a click on two quick preset presses).
        let mut fade = live();
        fade.retire(Gain(1.0), 4);
        let mut buf = vec![1.0f32; 2 * 2];
        let zero = Gain(0.0);
        fade.run(&mut buf, |s| s.iter_mut().for_each(|x| *x *= zero.0), |g, s| s.iter_mut().for_each(|x| *x *= g.0));
        assert_eq!(buf[2], 0.75, "two frames into the first fade");
        fade.retire(Gain(0.0), 4); // the stage as it was: 0.0, now changing to 0.25
        let quarter = Gain(0.25);
        let mut next = vec![1.0f32; 2];
        fade.run(&mut next, |s| s.iter_mut().for_each(|x| *x *= quarter.0), |g, s| s.iter_mut().for_each(|x| *x *= g.0));
        assert!((next[0] - 0.5).abs() < 1e-6, "continued from {}, the blend was 0.5", next[0]);
    }

    #[test]
    fn a_clone_never_carries_a_fade() {
        let mut fade = live();
        fade.retire(Gain(1.0), 10);
        assert!(!fade.clone().running());
    }
}
