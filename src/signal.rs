//! The signal path verdict: does exclusive mode deliver the file bit-perfect,
//! and if not, which stage changes the samples. The conditions are the ones
//! the `chain_tests::bit_perfect_*` tests pin (see CLAUDE.md, "Bit-Perfect
//! Output"); every theme shows the same verdict.

use std::sync::atomic::Ordering;

use crate::state::PlayerState;

/// Everything on the path from the file to the DAC that can alter a sample.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PathInputs {
    pub exclusive: bool,
    pub src_rate: u32,
    pub out_rate: u32,
    pub src_bits: u32,
    /// The DAC format's exact bits (0 = unknown).
    pub out_bits: u32,
    pub src_channels: u32,
    pub volume: u32,
    pub eq: bool,
    pub fx: bool,
    pub crossfeed: bool,
    /// The ReplayGain actually applied to this track (0 = none).
    pub rg_db: f32,
    pub balance: i32,
    pub crossfade_secs: u32,
}

impl PathInputs {
    /// Read the live path. `fx_name`/`cf_name` are the active preset names
    /// ("None"/"Off" = bypassed), as every renderer already holds them.
    pub fn from_state(state: &PlayerState, fx_name: &str, cf_name: &str) -> Self {
        let eq = state.eq_preamp_db().abs() >= 0.01
            || state.eq_bands_array().iter().any(|b| b.is_effective());
        Self {
            exclusive: state.exclusive.load(Ordering::Relaxed),
            src_rate: state.sample_rate.load(Ordering::Relaxed) as u32,
            out_rate: state.output_rate.load(Ordering::Relaxed) as u32,
            src_bits: state.bits_per_sample.load(Ordering::Relaxed) as u32,
            out_bits: state.output_bits.load(Ordering::Relaxed),
            src_channels: state.channels.load(Ordering::Relaxed) as u32,
            volume: state.volume.load(Ordering::Relaxed),
            eq,
            fx: fx_name != "None",
            crossfeed: cf_name != "Off",
            rg_db: state.rg_gain_db(),
            balance: state.balance_value(),
            crossfade_secs: state.crossfade_secs.load(Ordering::Relaxed),
        }
    }
}

/// What the screen says about the path.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// Shared mode: the system mixer owns the output, so there is nothing to
    /// claim either way.
    Shared,
    BitPerfect,
    /// Exclusive mode, but these stages change the samples (in path order).
    Altered(Vec<String>),
}

fn khz(rate: u32) -> String {
    let k = rate as f32 / 1000.0;
    if k.fract() == 0.0 { format!("{k:.0}k") } else { format!("{k:.1}k") }
}

pub fn verdict(i: &PathInputs) -> Verdict {
    if !i.exclusive {
        return Verdict::Shared;
    }
    let mut why = Vec::new();
    if i.src_channels > 2 {
        why.push("downmixed to stereo".to_string());
    }
    if i.src_rate != i.out_rate {
        why.push(format!("resampled {}→{}", khz(i.src_rate), khz(i.out_rate)));
    }
    if i.eq {
        why.push("EQ".to_string());
    }
    if i.fx {
        why.push("effects".to_string());
    }
    if i.rg_db.abs() >= 0.01 {
        why.push(format!("ReplayGain {:+.1} dB", i.rg_db));
    }
    if i.crossfeed {
        why.push("crossfeed".to_string());
    }
    if i.balance != 0 {
        why.push("balance".to_string());
    }
    if i.crossfade_secs > 0 {
        why.push(format!("crossfade {} s", i.crossfade_secs));
    }
    if i.volume != 100 {
        why.push(format!("volume {}%", i.volume));
    }
    // The f32 path is exact up to 24 bits; a 32-bit integer source loses its
    // low bits, and a DAC format narrower than the file drops them too.
    //
    // A 32-bit FLOAT source (float WAV) is also reported, deliberately: it
    // passes the f32 path untouched, but its values are not on any integer
    // grid (a float carries 24 significant bits at every level, so quiet
    // samples hold detail below a 24-bit DAC's last step), and every integer
    // DAC format rounds it. Only a float source into a float device format is
    // bit-perfect, and nothing here knows either side is float: `src_bits`
    // and `out_bits` are 32 and 24 for both kinds. Saying "bit-perfect" for
    // it would be wrong on every integer DAC, so the verdict stays
    // conservative until both formats are tracked.
    if i.src_bits > 24 {
        why.push(format!("{}-bit source", i.src_bits));
    } else if i.out_bits != 0 && i.out_bits < i.src_bits {
        why.push(format!("DAC {}-bit", i.out_bits));
    }
    if why.is_empty() { Verdict::BitPerfect } else { Verdict::Altered(why) }
}

/// The two halves of the line every theme draws: the verdict ("bit-perfect" /
/// "not bit-perfect") and the stages responsible. None in shared mode.
pub fn summary(v: &Verdict) -> Option<(bool, &'static str, &[String])> {
    match v {
        Verdict::Shared => None,
        Verdict::BitPerfect => Some((true, "✓ bit-perfect", &[])),
        Verdict::Altered(why) => Some((false, "▲ not bit-perfect", why)),
    }
}

/// The verdict as one line of at most `width` columns: the head, then as many
/// stages as fit, joined with `sep`. `ok` picks the colour. None in shared
/// mode, or when even the head does not fit (half a verdict misleads).
pub fn fitted(v: &Verdict, sep: &str, width: usize) -> Option<(bool, String)> {
    let (ok, head, why) = summary(v)?;
    if crate::ansi::visible_len(head) > width {
        return None;
    }
    let mut segments = vec![head.to_string()];
    segments.extend(why.iter().cloned());
    Some((ok, crate::ansi::fit_segments(&segments, sep, width)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> PathInputs {
        PathInputs {
            exclusive: true,
            src_rate: 96_000,
            out_rate: 96_000,
            src_bits: 24,
            out_bits: 24,
            src_channels: 2,
            volume: 100,
            ..Default::default()
        }
    }

    #[test]
    fn exclusive_with_every_stage_off_is_bit_perfect() {
        assert_eq!(verdict(&clean()), Verdict::BitPerfect);
        // Mono is duplicated, not mixed; a wider DAC format pads exactly.
        let i = PathInputs { src_channels: 1, src_bits: 16, ..clean() };
        assert_eq!(verdict(&i), Verdict::BitPerfect);
    }

    #[test]
    fn shared_mode_claims_nothing() {
        let i = PathInputs { exclusive: false, ..clean() };
        assert_eq!(verdict(&i), Verdict::Shared);
        assert_eq!(summary(&verdict(&i)), None);
    }

    #[test]
    fn every_altering_stage_is_named_in_path_order() {
        let i = PathInputs {
            src_rate: 44_100,
            src_channels: 6,
            eq: true,
            fx: true,
            rg_db: -6.4,
            crossfeed: true,
            balance: -10,
            crossfade_secs: 4,
            volume: 82,
            ..clean()
        };
        let Verdict::Altered(why) = verdict(&i) else { panic!() };
        assert_eq!(
            why,
            [
                "downmixed to stereo", "resampled 44.1k→96k", "EQ", "effects",
                "ReplayGain -6.4 dB", "crossfeed", "balance", "crossfade 4 s", "volume 82%",
            ]
        );
    }

    #[test]
    fn bit_depth_limits_are_reported() {
        let wide = PathInputs { src_bits: 32, out_bits: 24, ..clean() };
        assert_eq!(verdict(&wide), Verdict::Altered(vec!["32-bit source".into()]));
        let narrow_dac = PathInputs { out_bits: 16, ..clean() };
        assert_eq!(verdict(&narrow_dac), Verdict::Altered(vec!["DAC 16-bit".into()]));
        // Unknown DAC depth is not a fault.
        assert_eq!(verdict(&PathInputs { out_bits: 0, ..clean() }), Verdict::BitPerfect);
    }

    #[test]
    fn a_track_without_replaygain_tags_stays_bit_perfect_in_track_mode() {
        // Only the gain actually applied counts, not the mode setting.
        assert_eq!(verdict(&PathInputs { rg_db: 0.0, ..clean() }), Verdict::BitPerfect);
    }

    #[test]
    fn the_line_drops_stages_whole_and_never_half_a_verdict() {
        let v = Verdict::Altered(vec!["EQ".into(), "volume 82%".into()]);
        assert_eq!(fitted(&v, " · ", 80), Some((false, "▲ not bit-perfect · EQ · volume 82%".into())));
        assert_eq!(fitted(&v, " · ", 22), Some((false, "▲ not bit-perfect · EQ".into())));
        assert_eq!(fitted(&v, " · ", 10), None);
        assert_eq!(fitted(&Verdict::BitPerfect, " · ", 20), Some((true, "✓ bit-perfect".into())));
        assert_eq!(fitted(&Verdict::Shared, " · ", 80), None);
    }

    #[test]
    fn the_live_state_reads_as_the_stages_it_runs() {
        let st = PlayerState::new();
        st.exclusive.store(true, Ordering::Relaxed);
        st.sample_rate.store(48_000, Ordering::Relaxed);
        st.output_rate.store(48_000, Ordering::Relaxed);
        let i = PathInputs::from_state(&st, "None", "Off");
        assert!(!i.eq && !i.fx && !i.crossfeed);
        assert_eq!(verdict(&i), Verdict::BitPerfect);
        st.set_eq_preamp_db(-3.0);
        let i = PathInputs::from_state(&st, "Hall", "Light");
        assert!(i.eq && i.fx && i.crossfeed, "a preamp alone runs the EQ stage");
    }
}
