//! Click-free preset changes for the DSP stages (EQ, effects, crossfeed).
//!
//! Swapping a stage's settings outright is a step in the output: a crossfeed
//! or effects preset rebuilt its filters and delay lines from silence (cutting
//! a reverb tail dead), and an EQ band switched on started from zero filter
//! state, a thump on a loud signal. A stage that changes keeps a copy of its
//! old self here; for a short while both run on the same input and the output
//! moves linearly from the old one to the new one.

/// The retired copy of a stage and how far its fade has got.
pub(crate) struct Crossfade<T> {
    old: Option<Box<T>>,
    /// Whether the stage has processed audio since its last reset. Only a
    /// running signal has a step to hide: a fresh stage (a new producer, or
    /// one just reset by a seek) takes new settings at once.
    live: bool,
    /// Frames faded so far, and the fade's length in frames.
    pos: usize,
    len: usize,
    scratch: Vec<f32>,
}

impl<T> Default for Crossfade<T> {
    fn default() -> Self {
        Self { old: None, live: false, pos: 0, len: 0, scratch: Vec::new() }
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
    /// over `frames`. A change during a fade keeps the oldest copy and starts
    /// the fade again, so a burst of edits is one smooth move.
    pub(crate) fn retire(&mut self, current: T, frames: usize) {
        if !self.live {
            return;
        }
        if self.old.is_none() {
            self.old = Some(Box::new(current));
        }
        self.pos = 0;
        self.len = frames.max(1);
    }

    pub(crate) fn running(&self) -> bool {
        self.old.is_some()
    }

    /// Drop the fade (the audio jumped: a seek, a skip); the stage is fresh.
    pub(crate) fn clear(&mut self) {
        self.old = None;
        self.live = false;
    }

    /// Forget a fade just started without marking the stage fresh (the change
    /// turned out to be no change).
    pub(crate) fn cancel(&mut self) {
        self.old = None;
    }

    /// Process interleaved stereo `samples` through both versions and blend:
    /// `new` runs the stage as it is now, `old` the retired copy.
    pub(crate) fn run(
        &mut self,
        samples: &mut [f32],
        new: impl FnOnce(&mut [f32]),
        old: impl FnOnce(&mut T, &mut [f32]),
    ) {
        self.live = true;
        let Some(retired) = self.old.as_mut() else {
            new(samples);
            return;
        };
        self.scratch.clear();
        self.scratch.extend_from_slice(samples);
        old(retired, &mut self.scratch);
        new(samples);
        let frames = samples.len() / 2;
        for f in 0..frames {
            let t = ((self.pos + f) as f32 / self.len as f32).min(1.0);
            for c in 0..2 {
                let i = f * 2 + c;
                samples[i] = self.scratch[i] * (1.0 - t) + samples[i] * t;
            }
        }
        self.pos += frames;
        if self.pos >= self.len {
            self.old = None;
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
    fn a_change_during_a_fade_keeps_the_oldest_copy_and_restarts() {
        let mut fade = live();
        fade.retire(Gain(1.0), 10);
        fade.retire(Gain(0.5), 10);
        let mut buf = vec![1.0f32; 2];
        fade.run(&mut buf, |s| s.iter_mut().for_each(|x| *x = 0.0), |g, s| s.iter_mut().for_each(|x| *x *= g.0));
        assert_eq!(buf[0], 1.0, "starts from the first, oldest state");
    }

    #[test]
    fn a_clone_never_carries_a_fade() {
        let mut fade = live();
        fade.retire(Gain(1.0), 10);
        assert!(!fade.clone().running());
    }
}
