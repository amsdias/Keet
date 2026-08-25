use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::common::Limit;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use rubato::{Async, FixedAsync, Indexing, SincInterpolationType, SincInterpolationParameters, WindowFunction, Resampler};
use audioadapter_buffers::direct::{InterleavedSlice, SequentialSliceOfVecs};

use rtrb::Producer;

use crate::state::PlayerState;
use crate::state::RgMode;

// NOTE: symphonia 0.5 needed a hand-written planar-to-interleaved walk with an
// arm per sample format (S16/S24/S32/F32/…). 0.6's generic audio buffer does it
// in one SIMD-optimized call — see `copy_to_slice_interleaved` in the decode
// loop — so all of that is gone.

/// Append `input` (interleaved, `channels`-channel) to `out` as interleaved
/// stereo. The ring buffer, DSP chain, and audio callback all assume stereo;
/// every decoded layout must pass through here. Mono is duplicated to L/R,
/// quad and SMPTE-ordered surround layouts are downmixed (center and surrounds
/// at -3 dB, LFE dropped).
fn interleaved_to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    const G: f32 = std::f32::consts::FRAC_1_SQRT_2; // -3 dB
    match channels {
        0 => {}
        2 => out.extend_from_slice(input),
        1 => {
            out.reserve(input.len() * 2);
            for &s in input {
                out.push(s);
                out.push(s);
            }
        }
        4 => {
            // Quad: FL FR BL BR — rears fold into their own sides.
            out.reserve(input.len() / 2);
            for frame in input.as_chunks::<4>().0 {
                out.push(frame[0] + G * frame[2]);
                out.push(frame[1] + G * frame[3]);
            }
        }
        n => {
            // SMPTE order: FL FR FC [LFE] BL BR [SL SR]. Center feeds both
            // sides; LFE (index 3, present from 6ch up) is dropped; remaining
            // surrounds alternate left/right, which matches BL/BR (and SL/SR)
            // pair ordering.
            out.reserve(input.len() / n * 2);
            for frame in input.chunks_exact(n) {
                let mut l = frame[0] + G * frame[2];
                let mut r = frame[1] + G * frame[2];
                let rest = if n >= 6 { &frame[4..] } else { &frame[3..] };
                for (i, &s) in rest.iter().enumerate() {
                    if i % 2 == 0 { l += G * s; } else { r += G * s; }
                }
                out.push(l);
                out.push(r);
            }
        }
    }
}

fn deinterleave_into(samples: &[f32], ch: usize, out: &mut Vec<Vec<f32>>) {
    out.resize_with(ch, Vec::new);
    for plane in out.iter_mut() { plane.clear(); }
    for (i, &s) in samples.iter().enumerate() {
        out[i % ch].push(s);
    }
}

/// True when the producer must stop waiting and unwind: main sets one of these
/// signals and then JOINS the producer thread (quit/shutdown, skip-prev or
/// jump respawn, stream-error recovery). Every producer wait loop must check
/// this — a loop that waits only on ring space deadlocks the join when the
/// audio callback is dead and the ring never drains (frozen UI, raw mode eats
/// Ctrl+C). Add new join-preceding signals HERE, not at individual wait sites.
pub(crate) fn producer_should_unstick(state: &PlayerState) -> bool {
    state.should_quit()
        || state.skip_prev.load(Ordering::Relaxed)
        || state.jump_to_track.load(Ordering::Relaxed) >= 0
}

/// Wait (bounded) until the audio callback has consumed a drain request set via
/// `reset_consumer_counter`. The producer must not push new samples while the
/// flag is pending: the callback drains *everything* in the ring when it sees
/// the flag, so samples pushed in between would be silently discarded —
/// clipping the start of post-seek/skip audio. Bounded so a dead stream
/// (device error) can't hang the producer; 250 ms is many callback periods.
pub(crate) fn await_consumer_drain(state: &PlayerState) {
    for _ in 0..50 {
        if !state.reset_consumer_counter.load(Ordering::Relaxed) || producer_should_unstick(state) {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Push the whole slice into the ring, waiting briefly when full.
/// `push_entire_slice` alone is all-or-nothing — on a full ring it pushes
/// NOTHING and the chunk would be silently dropped (the EOF flush runs without
/// the buffer-space throttle, so it can actually hit that).
///
/// Must also bail on the unstick signals (see `producer_should_unstick`) —
/// dropping the rest of the chunk is fine; the track is being abandoned anyway.
fn push_all(producer: &mut Producer<f32>, state: &PlayerState, mut data: &[f32]) {
    while !data.is_empty() && !producer_should_unstick(state) {
        let n = producer.slots().min(data.len());
        if n > 0 && producer.push_entire_slice(&data[..n]).is_ok() {
            data = &data[n..];
            continue;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Scratch buffers for `apply_chain_and_push`, cleared (capacity retained) on
/// each chunk so the steady-state producer path stays allocation-free.
struct ChainBufs {
    eq_buf: Vec<f32>,
    fx_buf: Vec<f32>,
    rg_buf: Vec<f32>,
    xfeed_buf: Vec<f32>,
    bal_buf: Vec<f32>,
    // Only touched while a crossfade tail is being mixed in; steady-state
    // playback reads the balance stage's output directly and skips this.
    final_buf: Vec<f32>,
}

impl ChainBufs {
    fn with_capacity(cap: usize) -> Self {
        Self {
            eq_buf: Vec::with_capacity(cap),
            fx_buf: Vec::with_capacity(cap),
            rg_buf: Vec::with_capacity(cap),
            xfeed_buf: Vec::with_capacity(cap),
            bal_buf: Vec::with_capacity(cap),
            final_buf: Vec::with_capacity(cap),
        }
    }
}

/// Everything downstream of decode+resample for one stereo chunk:
/// EQ → effects → ReplayGain → crossfeed → balance → crossfade mix →
/// clipping flag → ring push (+ crossfade tail capture).
///
/// Both the packet loop AND the resampler EOF flush MUST route through here.
/// The flush used to push raw resampled samples, so the last ~20 ms of every
/// resampled track skipped the whole chain — an audible ReplayGain/EQ step
/// right at track end, and those samples were missing from the crossfade tail.
#[allow(clippy::too_many_arguments)] // producer-thread chain context; a struct adds no clarity
fn apply_chain_and_push(
    input: &[f32],
    producer: &mut Producer<f32>,
    state: &PlayerState,
    eq: &mut crate::eq::EqChain,
    effects: &mut crate::effects::EffectsChain,
    crossfeed: &mut crate::crossfeed::CrossfeedFilter,
    rg_linear: f32,
    xfade_in: Option<&std::collections::VecDeque<f32>>,
    crossfade_pos: &mut usize,
    crossfade_samples: usize,
    tail_buf: Option<&mut std::collections::VecDeque<f32>>,
    bufs: &mut ChainBufs,
) {
    if input.is_empty() {
        return;
    }

    // EQ processing
    let eq_output = if eq.is_active() {
        bufs.eq_buf.clear();
        bufs.eq_buf.extend_from_slice(input);
        eq.process_stereo(&mut bufs.eq_buf);
        &bufs.eq_buf[..]
    } else {
        input
    };

    // Effects processing
    let fx_output = if effects.is_active() {
        bufs.fx_buf.clear();
        bufs.fx_buf.extend_from_slice(eq_output);
        effects.process_stereo(&mut bufs.fx_buf);
        &bufs.fx_buf[..]
    } else {
        eq_output
    };

    // ReplayGain
    let rg_output = if rg_linear != 1.0 {
        bufs.rg_buf.clear();
        bufs.rg_buf.extend_from_slice(fx_output);
        for sample in bufs.rg_buf.iter_mut() {
            *sample *= rg_linear;
        }
        &bufs.rg_buf[..]
    } else {
        fx_output
    };

    // Crossfeed processing (after RG, before balance)
    let cf_output = if crossfeed.is_active() {
        bufs.xfeed_buf.clear();
        bufs.xfeed_buf.extend_from_slice(rg_output);
        crossfeed.process_stereo(&mut bufs.xfeed_buf);
        &bufs.xfeed_buf[..]
    } else {
        rg_output
    };

    // Balance processing (after crossfeed, before crossfade)
    let balance = state.balance_value();
    let bal_output = if balance != 0 {
        bufs.bal_buf.clear();
        bufs.bal_buf.extend_from_slice(cf_output);
        let left_gain = ((100 - balance) as f32 / 100.0).clamp(0.0, 1.0);
        let right_gain = ((100 + balance) as f32 / 100.0).clamp(0.0, 1.0);
        for i in (0..bufs.bal_buf.len()).step_by(2) {
            bufs.bal_buf[i] *= left_gain;
            if i + 1 < bufs.bal_buf.len() {
                bufs.bal_buf[i + 1] *= right_gain;
            }
        }
        &bufs.bal_buf[..]
    } else {
        cf_output
    };

    // Crossfade mixing with the previous track's tail. Only populate final_buf
    // when we actually need to mutate samples.
    let mut using_final_buf = false;
    if let Some(tail) = xfade_in {
        if *crossfade_pos < crossfade_samples && crossfade_samples > 0 {
            bufs.final_buf.clear();
            bufs.final_buf.extend_from_slice(bal_output);
            for sample in bufs.final_buf.iter_mut() {
                if *crossfade_pos < crossfade_samples {
                    let pos_f = *crossfade_pos as f32 / crossfade_samples as f32;
                    let fade_in = (pos_f * std::f32::consts::FRAC_PI_2).sin();
                    let fade_out = ((1.0 - pos_f) * std::f32::consts::FRAC_PI_2).sin();

                    let tail_sample = if *crossfade_pos < tail.len() { tail[*crossfade_pos] } else { 0.0 };
                    *sample = *sample * fade_in + tail_sample * fade_out;
                    *crossfade_pos += 1;
                }
            }
            using_final_buf = true;
        }
    }

    let out: &[f32] = if using_final_buf { &bufs.final_buf } else { bal_output };

    // Clipping detection only — flag for the UI when peak * volume would
    // exceed 0dBFS at the DAC. Hard prevention happens in the audio callback
    // (clamp post-gain), since volume can change between this scan and
    // consumption (~ring-buffer-depth latency).
    let vol = state.volume.load(Ordering::Relaxed) as f32 / 100.0;
    let peak = out.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    if peak * vol > 1.0 {
        state.clipping.store(true, Ordering::Relaxed);
    }

    // Push to ring buffer, then capture the tail for the crossfade into the
    // next track.
    push_all(producer, state, out);
    if let Some(tb) = tail_buf {
        tb.extend(out.iter().copied());
        if tb.len() > crossfade_samples {
            let excess = tb.len() - crossfade_samples;
            tb.drain(..excess);
        }
    }
}

/// ReplayGain tag values parsed from a single track.
pub struct RgTags {
    pub track_gain: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_gain: Option<f32>,
    pub album_peak: Option<f32>,
}

/// Extract ReplayGain tags from a Symphonia MetadataRevision.
fn extract_rg_from_tags(tags: &[symphonia::core::meta::Tag], rg: &mut RgTags) {
    use symphonia::core::meta::StandardTag;
    for tag in tags {
        // 0.6 maps ReplayGain to standard tags; the raw-key pass below still
        // runs for readers/containers that leave them unmapped.
        match &tag.std {
            Some(StandardTag::ReplayGainTrackGain(s)) if rg.track_gain.is_none() => {
                rg.track_gain = crate::metadata::parse_rg_gain_value(s);
            }
            Some(StandardTag::ReplayGainTrackPeak(s)) if rg.track_peak.is_none() => {
                rg.track_peak = s.trim().parse::<f32>().ok();
            }
            Some(StandardTag::ReplayGainAlbumGain(s)) if rg.album_gain.is_none() => {
                rg.album_gain = crate::metadata::parse_rg_gain_value(s);
            }
            Some(StandardTag::ReplayGainAlbumPeak(s)) if rg.album_peak.is_none() => {
                rg.album_peak = s.trim().parse::<f32>().ok();
            }
            _ => {}
        }
        if let symphonia::core::meta::RawValue::String(ref s) = tag.raw.value {
            let key_lower = tag.raw.key.to_lowercase();
            match key_lower.as_str() {
                "replaygain_track_gain" if rg.track_gain.is_none() => {
                    rg.track_gain = crate::metadata::parse_rg_gain_value(s);
                }
                "replaygain_track_peak" if rg.track_peak.is_none() => {
                    rg.track_peak = s.trim().parse::<f32>().ok();
                }
                "replaygain_album_gain" if rg.album_gain.is_none() => {
                    rg.album_gain = crate::metadata::parse_rg_gain_value(s);
                }
                "replaygain_album_peak" if rg.album_peak.is_none() => {
                    rg.album_peak = s.trim().parse::<f32>().ok();
                }
                _ => {}
            }
        }
    }
}

/// Compute the linear gain multiplier from RG tags and mode.
fn compute_rg_gain(mode: RgMode, tags: &RgTags) -> f32 {
    if mode == RgMode::Off { return 1.0; }

    let (gain_db, peak) = match mode {
        RgMode::Album => {
            let g = tags.album_gain.or(tags.track_gain);
            let p = tags.album_peak.or(tags.track_peak);
            (g, p)
        }
        _ => {
            // ReplayGain spec: fall back to the album values when track tags
            // are missing (mirrors the Album→Track fallback above).
            let g = tags.track_gain.or(tags.album_gain);
            let p = tags.track_peak.or(tags.album_peak);
            (g, p)
        }
    };

    let gain_db = match gain_db {
        Some(db) => db,
        None => return 1.0,
    };

    let mut linear = 10.0_f32.powf(gain_db / 20.0);

    // Peak-based clipping prevention
    if let Some(peak) = peak {
        if peak > 0.0 && linear * peak > 1.0 {
            linear = 1.0 / peak;
        }
    }

    linear
}

#[allow(clippy::too_many_arguments)] // producer thread entry point; args are the full decode context
pub fn decode_playlist(
    playlist: &[PathBuf],
    start_index: usize,
    producer: &mut Producer<f32>,
    state: &PlayerState,
    output_rate: u32,
    hq_resampler: bool,
    eq: &mut crate::eq::EqChain,
    eq_presets: &[crate::eq::EqPreset],
    effects: &mut crate::effects::EffectsChain,
    effects_presets: &[crate::effects::EffectsPreset],
    crossfade_secs: u32,
    crossfeed: &mut crate::crossfeed::CrossfeedFilter,
    crossfeed_presets: &[crate::crossfeed::CrossfeedPreset],
) {
    let crossfade_samples = crossfade_secs as usize * output_rate as usize * 2; // stereo
    let mut crossfade_tail: Option<std::collections::VecDeque<f32>> = None;
    let mut track_index = start_index;
    // True until the first track this producer decodes is set up. Distinct from
    // `track_index == start_index`, which wrongly matches again on repeat-one.
    let mut first_iteration = true;
    // Ring capacity is sized per output rate in main.rs and stored on state before
    // the producer is spawned, so it's stable for the lifetime of this call.
    let ring_capacity = state.ring_capacity.load(Ordering::Relaxed);

    while track_index < playlist.len() {
        // Non-destructive peek: main.rs is the single consumer of skip_prev / jump_to_track
        // (via take_skip_prev / take_jump). If producer consumed these here, a race would
        // let main.rs miss the signal and wrongly advance to the next track.
        if state.should_quit()
            || state.skip_prev.load(Ordering::Relaxed)
            || state.jump_to_track.load(Ordering::Relaxed) >= 0
        {
            break;
        }

        let path = &playlist[track_index];

        // --- Open file and probe format ---
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                if let Ok(mut err) = state.decode_error.lock() {
                    *err = Some(format!("{}: {}", path.display(), e));
                }
                state.signal_next_track(track_index + 1);
                track_index += 1;
                continue;
            }
        };
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        let mut hint = Hint::new();
        if let Some(ext) = path.extension() {
            hint.with_extension(ext.to_str().unwrap_or(""));
        }

        // Skip embedded picture (cover art) reads in the decoder thread —
        // covers are loaded by a dedicated worker (`spawn_cover_worker`) that
        // re-opens the file. Letting symphonia load a 1 MB+ FLAC PICTURE block
        // here is just allocator churn we throw away. `Limit::Maximum(0)`
        // makes the demuxer skip the visual entirely.
        let meta_opts = MetadataOptions::default().limit_visual_bytes(Limit::Maximum(0));
        let mut format = match symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), meta_opts)
        {
            Ok(p) => p,
            Err(e) => {
                if let Ok(mut err) = state.decode_error.lock() {
                    *err = Some(format!("{}: {}", path.display(), e));
                }
                state.signal_next_track(track_index + 1);
                track_index += 1;
                continue;
            }
        };

        let track = match format.default_track(TrackType::Audio) {
            Some(t) => t.clone(),
            None => {
                if let Ok(mut err) = state.decode_error.lock() {
                    *err = Some(format!("{}: No audio track", path.display()));
                }
                state.signal_next_track(track_index + 1);
                track_index += 1;
                continue;
            }
        };

        let track_id = track.id;
        // 0.6: codec_params is Option and splits per media type, so the audio
        // parameters come out of .audio(); track length moved onto Track.
        let audio_params = match track.codec_params.as_ref().and_then(|c| c.audio()) {
            Some(a) => a.clone(),
            None => {
                if let Ok(mut err) = state.decode_error.lock() {
                    *err = Some(format!("{}: No audio codec parameters", path.display()));
                }
                state.signal_next_track(track_index + 1);
                track_index += 1;
                continue;
            }
        };
        let sample_rate = audio_params.sample_rate.unwrap_or(44100);
        // Source layout, kept for the UI. Everything downstream of decode —
        // resampler, DSP chain, ring buffer, audio callback — runs on stereo;
        // interleaved_to_stereo converts right after each packet is decoded.
        let src_channels = audio_params.channels.as_ref().map(|c| c.count()).unwrap_or(2);
        let channels = 2usize;
        let bits_per_sample = audio_params.bits_per_sample.unwrap_or(16);
        let total = track.num_frames.unwrap_or(0);

        let mut decoder = match symphonia::default::get_codecs()
            .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        {
            Ok(d) => d,
            Err(e) => {
                if let Ok(mut err) = state.decode_error.lock() {
                    *err = Some(format!("{}: {}", path.display(), e));
                }
                state.signal_next_track(track_index + 1);
                track_index += 1;
                continue;
            }
        };

        // --- Read ReplayGain tags ---
        // 0.6 unifies probe-side and container metadata into the format reader,
        // so this is one pass instead of two (same as metadata.rs).
        let mut rg_tags = RgTags {
            track_gain: None, track_peak: None,
            album_gain: None, album_peak: None,
        };
        if let Some(rev) = format.metadata().current() {
            extract_rg_from_tags(&rev.media.tags, &mut rg_tags);
        }
        let rg_linear = compute_rg_gain(state.rg_mode(), &rg_tags);

        // --- Create resampler if needed ---
        // Created before the drain wait so a failure skips the track like the
        // decoder-creation failures above. Falling back to "no resampler" here
        // would silently play the track at the wrong pitch.
        let mut resampler: Option<Async<f32>> = if sample_rate != output_rate {
            let params = if hq_resampler {
                SincInterpolationParameters {
                    sinc_len: 256,
                    f_cutoff: Some(0.95),
                    interpolation: SincInterpolationType::Cubic,
                    oversampling_factor: 128,
                    window: WindowFunction::BlackmanHarris2,
                }
            } else {
                SincInterpolationParameters {
                    sinc_len: 64,
                    f_cutoff: Some(0.95),
                    interpolation: SincInterpolationType::Linear,
                    oversampling_factor: 128,
                    window: WindowFunction::BlackmanHarris2,
                }
            };
            match Async::new_sinc(
                output_rate as f64 / sample_rate as f64,
                2.0,
                &params,
                1024,
                channels,
                FixedAsync::Input,
            ) {
                Ok(r) => Some(r),
                Err(e) => {
                    if let Ok(mut err) = state.decode_error.lock() {
                        *err = Some(format!("{}: resampler: {}", path.display(), e));
                    }
                    state.signal_next_track(track_index + 1);
                    track_index += 1;
                    continue;
                }
            }
        } else {
            None
        };

        let mut broke_for_skip = false;
        let mut skipped = false;

        // Wait for buffer to drain so display update matches audio playback.
        // Gated on "not the first decoded track of this producer" rather than
        // track_index != start_index: repeat-one re-enters with the same index,
        // and skipping the wait there reset samples_played up to a full ring
        // (~4 s) before the restart was audible.
        if !first_iteration {
            let drain_threshold = output_rate as usize; // ~0.5s stereo
            loop {
                let buffered = ring_capacity - producer.slots();
                if buffered <= drain_threshold { break; }
                if producer_should_unstick(state) {
                    broke_for_skip = true;
                    break;
                }
                if state.take_skip_next() {
                    state.reset_consumer_counter.store(true, Ordering::Relaxed);
                    await_consumer_drain(state);
                    break;
                }
                if state.is_paused() {
                    thread::sleep(Duration::from_millis(50));
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            if broke_for_skip { break; }
        }

        // --- Update track info ---
        state.track_info_ready.store(false, Ordering::Relaxed);
        state.sample_rate.store(sample_rate as u64, Ordering::Relaxed);
        state.total_samples.store(total, Ordering::Relaxed);
        state.samples_played.store(0, Ordering::Relaxed);
        state.channels.store(src_channels, Ordering::Relaxed);
        state.bits_per_sample.store(bits_per_sample as usize, Ordering::Relaxed);
        state.track_info_ready.store(true, Ordering::Relaxed);

        // Signal track transition (skip for the producer's first track — main
        // thread already knows). Repeat-one passes signal too, so the UI
        // resets lyrics scroll and position for the restarted track.
        if !first_iteration {
            state.signal_next_track(track_index);
        }
        first_iteration = false;

        // Reset filter states for new track
        eq.reset();
        effects.reset();
        crossfeed.reset();

        // --- Crossfade setup for this track ---
        let xfade_in = crossfade_tail.take();
        let mut crossfade_pos: usize = 0;
        let capture_tail = crossfade_samples > 0;
        let mut tail_buf: std::collections::VecDeque<f32> = if capture_tail { std::collections::VecDeque::with_capacity(crossfade_samples) } else { std::collections::VecDeque::new() };

        let chunk_size = resampler.as_ref().map(|r| r.input_frames_next()).unwrap_or(1024);
        let mut pending: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);

        // Persistent resampler output, sized once to the worst case. Both the
        // packet loop and the EOF flush process into this via InterleavedSlice —
        // `resampler.process()` returned a freshly allocated buffer per chunk
        // (~40 mallocs/s during resampled playback on the producer thread).
        let mut resamp_out: Vec<f32> = resampler
            .as_ref()
            .map(|r| vec![0.0; r.output_frames_max() * channels])
            .unwrap_or_default();

        // Reusable buffers
        let mut deinterleaved: Vec<Vec<f32>> =
            (0..channels).map(|_| Vec::with_capacity(chunk_size)).collect();
        let mut interleaved_out: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut decoded_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        // Scratch for per-packet symphonia → interleaved f32 conversion. Retained across
        // iterations so we don't malloc on every packet.
        let mut raw_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        // Scratch for resampler flush deinterleave on EOF.
        let mut flush_planes: Vec<Vec<f32>> = Vec::with_capacity(channels);
        // DSP-chain scratch (EQ/FX/RG/crossfeed/balance/crossfade), shared by
        // the packet loop and the EOF flush via apply_chain_and_push.
        let mut chain_bufs = ChainBufs::with_capacity(chunk_size * channels * 2);

        // --- Packet decode loop ---
        loop {
            // Quit, skip-prev, jump — all require the producer to exit entirely
            // (main is about to join us).
            if producer_should_unstick(state) {
                broke_for_skip = true;
                break;
            }
            // Check skip-next — flush buffer and advance to next track
            if state.take_skip_next() {
                if ring_capacity - producer.slots() > 0 {
                    state.reset_consumer_counter.store(true, Ordering::Relaxed);
                    await_consumer_drain(state);
                }
                skipped = true;
                break;
            }

            // Handle seek
            let seek_secs = state.take_seek();
            if seek_secs != 0 {
                let new_time = (state.time_secs() + seek_secs as f64).max(0.0);
                pending.clear();
                if let Some(ref mut r) = resampler { r.reset(); }
                eq.reset();
                effects.reset();
                crossfeed.reset();

                state.reset_consumer_counter.store(true, Ordering::Relaxed);
                await_consumer_drain(state);

                // 0.6 replaced the infallible From<f64> with a checked
                // constructor; an unrepresentable target just skips the seek.
                if let Some(time) = Time::try_from_secs_f64(new_time) {
                    if format.seek(SeekMode::Coarse, SeekTo::Time {
                        time,
                        track_id: Some(track_id),
                    }).is_ok() {
                        state.samples_played.store((new_time * output_rate as f64) as u64, Ordering::Relaxed);
                    }
                }
            }

            // Throttle when buffer is full
            let free = producer.slots();
            if free < ring_capacity / 4 {
                thread::sleep(Duration::from_millis(20));
                continue;
            }

            // Pause handling
            if state.is_paused() {
                thread::sleep(Duration::from_millis(50));
                continue;
            }

            // Check for live EQ change: Custom → the edited live bands, else
            // the selected named preset's bands.
            if state.take_eq_changed() {
                if state.is_eq_custom() {
                    eq.load_bands(&state.eq_bands_array(), state.eq_preamp_db(), output_rate as f32);
                } else {
                    let idx = state.eq_index();
                    if idx < eq_presets.len() {
                        eq.load_preset(&eq_presets[idx], output_rate as f32);
                    }
                }
            }

            // Check for live effects preset change
            if state.take_effects_changed() {
                let idx = state.effects_index();
                if idx < effects_presets.len() {
                    effects.load_preset(&effects_presets[idx], output_rate as f32);
                }
            }

            // Check for live crossfeed preset change
            if state.take_crossfeed_changed() {
                let idx = state.crossfeed_index();
                if idx < crossfeed_presets.len() {
                    crossfeed.load_preset(&crossfeed_presets[idx], output_rate as f32);
                }
            }

            // Decode next packet. 0.6 signals end-of-stream with Ok(None)
            // rather than an error, so both arms end the track.
            let packet = match format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => break,  // end of stream
                Err(_) => break,    // read error
            };

            if packet.track_id != track_id { continue; }

            let decoded = match decoder.decode(&packet) {
                Ok(d) => d,
                Err(_) => continue,
            };

            // 0.6's generic audio buffer converts and interleaves in one call,
            // replacing the hand-written per-sample-format planar walk.
            raw_buf.clear();
            raw_buf.resize(decoded.samples_interleaved(), 0.0);
            decoded.copy_to_slice_interleaved(&mut raw_buf);
            if raw_buf.is_empty() { continue; }

            // Convert the source layout to interleaved stereo (appends to pending).
            interleaved_to_stereo(&raw_buf, src_channels, &mut pending);

            // Resample if needed
            decoded_buf.clear();
            if let Some(ref mut resampler) = resampler {
                // Walk fixed-size chunks out of `pending` with a read cursor, then drop
                // the consumed prefix in a single move. Draining each chunk off the
                // front would memmove the trailing samples on every iteration.
                let mut consumed = 0usize;
                while pending.len() - consumed >= chunk_size * channels {
                    let chunk = &pending[consumed..consumed + chunk_size * channels];

                    for ch_buf in deinterleaved.iter_mut() { ch_buf.clear(); }
                    for (i, &s) in chunk.iter().enumerate() {
                        deinterleaved[i % channels].push(s);
                    }
                    consumed += chunk_size * channels;

                    let frames_in = chunk_size;
                    if let Ok(adapter_in) = SequentialSliceOfVecs::new(&deinterleaved, channels, frames_in) {
                        let out_frames = resampler.output_frames_next();
                        if let Ok(mut adapter_out) =
                            InterleavedSlice::new_mut(&mut resamp_out, channels, out_frames)
                        {
                            if let Ok((_, nbr_out)) =
                                resampler.process_into_buffer(&adapter_in, &mut adapter_out, None)
                            {
                                interleaved_out.extend_from_slice(&resamp_out[..nbr_out * channels]);
                            }
                        }
                    }
                }
                if consumed > 0 {
                    pending.drain(..consumed);
                }

                if interleaved_out.is_empty() {
                    continue;
                }

                decoded_buf.extend_from_slice(&interleaved_out);
                interleaved_out.clear();
            } else {
                decoded_buf.extend_from_slice(&pending);
                pending.clear();
            };
            apply_chain_and_push(
                &decoded_buf, producer, state, eq, effects, crossfeed, rg_linear,
                xfade_in.as_ref(), &mut crossfade_pos, crossfade_samples,
                if capture_tail { Some(&mut tail_buf) } else { None },
                &mut chain_bufs,
            );
        }

        // Flush the resampler tail. Zero-padding `pending` to a full chunk and
        // pushing everything the resampler produced appended up to ~23 ms of
        // resampled silence to every track — an audible gap in gapless albums.
        // Instead: feed the real frames with `partial_len` (rubato pads
        // internally), pump zeros to drain the sinc delay line, and emit
        // exactly ceil(ratio·pending) + delay frames.
        if let Some(ref mut resampler) = resampler {
            if !pending.is_empty() && !skipped && !broke_for_skip {
                let pending_frames = pending.len() / channels;
                let ratio = output_rate as f64 / sample_rate as f64;
                let mut frames_wanted =
                    (pending_frames as f64 * ratio).ceil() as usize + resampler.output_delay();
                deinterleave_into(&pending, channels, &mut flush_planes);
                // The adapter needs full chunk geometry; rubato only reads the
                // first `partial_len` frames of it.
                for plane in flush_planes.iter_mut() {
                    plane.resize(chunk_size, 0.0);
                }
                let mut partial = Some(pending_frames);
                while frames_wanted > 0 {
                    let adapter_in =
                        match SequentialSliceOfVecs::new(&flush_planes, channels, chunk_size) {
                            Ok(a) => a,
                            Err(_) => break,
                        };
                    let indexing = Indexing {
                        input_offset: 0,
                        output_offset: 0,
                        partial_len: Some(partial.take().unwrap_or(0)),
                        active_channels_mask: None,
                    };
                    let out_frames = resampler.output_frames_next();
                    let Ok(mut out_buf) =
                        InterleavedSlice::new_mut(&mut resamp_out, channels, out_frames)
                    else {
                        break;
                    };
                    match resampler.process_into_buffer(&adapter_in, &mut out_buf, Some(&indexing)) {
                        Ok((_, nbr_out)) => {
                            if nbr_out == 0 {
                                break;
                            }
                            let take = frames_wanted.min(nbr_out);
                            // Through the full DSP chain, same as the packet
                            // loop — pushing raw here skipped EQ/RG/etc. for
                            // the final ~20 ms of every resampled track.
                            apply_chain_and_push(
                                &resamp_out[..take * channels], producer, state,
                                eq, effects, crossfeed, rg_linear,
                                xfade_in.as_ref(), &mut crossfade_pos, crossfade_samples,
                                if capture_tail { Some(&mut tail_buf) } else { None },
                                &mut chain_bufs,
                            );
                            frames_wanted -= take;
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        // Save crossfade tail for next track (skip if user explicitly skipped)
        if capture_tail && !tail_buf.is_empty() && !skipped {
            crossfade_tail = Some(tail_buf);
        }

        if broke_for_skip {
            break; // Exit entire function
        }

        // Repeat-one: replay same track, no crossfade tail
        if state.repeat_mode() == crate::state::RepeatMode::One && !skipped {
            crossfade_tail = None;
            continue;
        }

        // Exclusive mode: check if next track needs a different sample rate
        if state.exclusive.load(Ordering::Relaxed) && track_index + 1 < playlist.len() {
            if let Some(next_rate) = crate::audio::probe_sample_rate(&playlist[track_index + 1]) {
                if next_rate != output_rate {
                    state.next_track_rate.store(next_rate, Ordering::Relaxed);
                    state.rate_change_needed.store(true, Ordering::Relaxed);
                    track_index += 1;
                    state.producer_track_index.store(track_index, Ordering::Relaxed);
                    break; // Exit decode_playlist for stream rebuild
                }
            }
        }

        track_index += 1;
    }

    if !state.rate_change_needed.load(Ordering::Relaxed) {
        state.producer_done.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_passes_through_appending() {
        let mut out = vec![9.0, 9.0]; // pre-existing content must be kept
        interleaved_to_stereo(&[0.1, 0.2, 0.3, 0.4], 2, &mut out);
        assert_eq!(out, vec![9.0, 9.0, 0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn mono_duplicates_to_both_channels() {
        let mut out = Vec::new();
        interleaved_to_stereo(&[0.5, -0.25], 1, &mut out);
        assert_eq!(out, vec![0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn five_one_downmix_center_and_surrounds_minus_3db_lfe_dropped() {
        // SMPTE order: FL FR FC LFE BL BR
        let mut out = Vec::new();
        interleaved_to_stereo(&[0.2, 0.4, 0.6, 1.0, 0.1, 0.3], 6, &mut out);
        let g = std::f32::consts::FRAC_1_SQRT_2;
        let want_l = 0.2 + g * 0.6 + g * 0.1;
        let want_r = 0.4 + g * 0.6 + g * 0.3;
        assert_eq!(out.len(), 2);
        assert!((out[0] - want_l).abs() < 1e-6, "L: got {} want {}", out[0], want_l);
        assert!((out[1] - want_r).abs() < 1e-6, "R: got {} want {}", out[1], want_r);
    }

    #[test]
    fn quad_downmix_routes_rears_to_their_sides() {
        // Quad order: FL FR BL BR
        let mut out = Vec::new();
        interleaved_to_stereo(&[0.2, 0.4, 0.1, 0.3], 4, &mut out);
        let g = std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0] - (0.2 + g * 0.1)).abs() < 1e-6);
        assert!((out[1] - (0.4 + g * 0.3)).abs() < 1e-6);
    }

    #[test]
    fn rg_track_mode_falls_back_to_album_tags() {
        let tags = RgTags {
            track_gain: None,
            track_peak: None,
            album_gain: Some(-6.0),
            album_peak: Some(0.9),
        };
        let gain = compute_rg_gain(RgMode::Track, &tags);
        let want = 10.0f32.powf(-6.0 / 20.0);
        assert!((gain - want).abs() < 1e-6, "got {} want {}", gain, want);
    }

    #[test]
    fn rg_track_mode_prefers_track_tags_when_present() {
        let tags = RgTags {
            track_gain: Some(-3.0),
            track_peak: None,
            album_gain: Some(-6.0),
            album_peak: None,
        };
        let gain = compute_rg_gain(RgMode::Track, &tags);
        let want = 10.0f32.powf(-3.0 / 20.0);
        assert!((gain - want).abs() < 1e-6);
    }

    /// Spawn `push_all` against a full ring nobody drains (dead-callback
    /// scenario) and report whether it returned within ~500 ms.
    fn push_all_returns(state: std::sync::Arc<PlayerState>) -> bool {
        let (mut producer, _consumer) = rtrb::RingBuffer::<f32>::new(8);
        producer.push_entire_slice(&[0.0; 8]).unwrap();
        let handle = thread::spawn(move || push_all(&mut producer, &state, &[0.0; 4]));
        for _ in 0..100 {
            if handle.is_finished() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn push_all_unsticks_on_jump_signal() {
        let state = std::sync::Arc::new(PlayerState::new());
        state.jump_to(0);
        assert!(
            push_all_returns(state),
            "push_all must exit when jump_to_track is set — main joins the \
             producer after setting it, and a dead stream never drains the ring"
        );
    }

    #[test]
    fn push_all_unsticks_on_skip_prev_signal() {
        let state = std::sync::Arc::new(PlayerState::new());
        state.prev();
        assert!(
            push_all_returns(state),
            "push_all must exit when skip_prev is set — main joins the \
             producer after setting it, and a dead stream never drains the ring"
        );
    }

    #[test]
    fn unstick_predicate_fires_on_each_join_signal() {
        let s = PlayerState::new();
        assert!(!producer_should_unstick(&s), "fresh state must not unstick");
        s.quit();
        assert!(producer_should_unstick(&s), "quit must unstick");

        let s = PlayerState::new();
        s.prev();
        assert!(producer_should_unstick(&s), "skip-prev must unstick");

        let s = PlayerState::new();
        s.jump_to(0);
        assert!(producer_should_unstick(&s), "jump must unstick");
    }

    #[test]
    fn push_all_unsticks_on_quit_signal() {
        let state = std::sync::Arc::new(PlayerState::new());
        state.quit();
        assert!(push_all_returns(state), "push_all must exit on quit");
    }

    #[test]
    fn await_consumer_drain_unsticks_promptly_on_jump() {
        // With the drain flag pending and a dead callback, the wait is bounded
        // at 250 ms — but a pending join (jump set) must end it promptly, not
        // ride out the full bound.
        let state = std::sync::Arc::new(PlayerState::new());
        state.reset_consumer_counter.store(true, Ordering::Relaxed);
        state.jump_to(0);
        let handle = thread::spawn(move || await_consumer_drain(&state));
        for _ in 0..24 {
            if handle.is_finished() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            handle.is_finished(),
            "await_consumer_drain must return well before its 250 ms bound \
             when a join-preceding signal is set"
        );
    }

    #[test]
    fn push_all_delivers_full_chunk_when_ring_has_space() {
        let state = PlayerState::new();
        let (mut producer, consumer) = rtrb::RingBuffer::<f32>::new(64);
        push_all(&mut producer, &state, &[1.0; 48]);
        assert_eq!(consumer.slots(), 48);
    }
}

/// DSP-chain integration tests: decode real files through the full producer
/// chain (decode → to-stereo → resample → EQ → effects → ReplayGain →
/// crossfeed → balance → limiter) with the test draining the ring in place of
/// the audio callback. No audio device involved. WAV fixtures are synthesized
/// into a temp dir; compressed fixtures live in tests/fixtures (see
/// generate.sh there).
#[cfg(test)]
mod chain_tests {
    use super::*;
    use crate::state::RgMode;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;

    // ---- WAV synthesis -----------------------------------------------------

    #[derive(Clone, Copy)]
    enum WavFmt {
        Pcm16,
        Pcm24,
        F32,
    }

    fn write_wav(path: &PathBuf, rate: u32, channels: u16, fmt: WavFmt, interleaved: &[f32]) {
        let (tag, bits): (u16, u16) = match fmt {
            WavFmt::Pcm16 => (1, 16),
            WavFmt::Pcm24 => (1, 24),
            WavFmt::F32 => (3, 32),
        };
        let bytes_per = (bits / 8) as u32;
        let mut data: Vec<u8> = Vec::with_capacity(interleaved.len() * bytes_per as usize);
        for &s in interleaved {
            let s = s.clamp(-1.0, 1.0);
            match fmt {
                WavFmt::Pcm16 => {
                    data.extend_from_slice(&((s * 32767.0).round() as i16).to_le_bytes());
                }
                WavFmt::Pcm24 => {
                    let v = (s * 8_388_607.0).round() as i32;
                    data.extend_from_slice(&v.to_le_bytes()[..3]);
                }
                WavFmt::F32 => data.extend_from_slice(&s.to_le_bytes()),
            }
        }
        let block_align = channels as u32 * bytes_per;
        // IEEE-float WAVs carry a `fact` chunk (sample count) per spec.
        let fact_len: u32 = if matches!(fmt, WavFmt::F32) { 12 } else { 0 };
        let mut out: Vec<u8> = Vec::with_capacity(data.len() + 64);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(4 + 24 + fact_len + 8 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * block_align).to_le_bytes());
        out.extend_from_slice(&(block_align as u16).to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        if fact_len > 0 {
            out.extend_from_slice(b"fact");
            out.extend_from_slice(&4u32.to_le_bytes());
            out.extend_from_slice(&((interleaved.len() / channels as usize) as u32).to_le_bytes());
        }
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        std::fs::write(path, out).expect("write wav fixture");
    }

    fn sine(rate: u32, secs: f32, freq: f32, amp: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn interleave(chs: &[Vec<f32>]) -> Vec<f32> {
        let n = chs[0].len();
        let mut out = Vec::with_capacity(n * chs.len());
        for i in 0..n {
            for ch in chs {
                out.push(ch[i]);
            }
        }
        out
    }

    fn tmp_wav(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("keet_chain_tests");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(format!("{}_{}.wav", name, std::process::id()))
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    // ---- harness -----------------------------------------------------------

    /// Run files through the real producer chain with the test acting as the
    /// audio callback: drain the ring, collect every output sample.
    fn run_chain(paths: &[PathBuf], output_rate: u32, rg: RgMode) -> Vec<f32> {
        let state = Arc::new(PlayerState::new());
        let cap = 1 << 16;
        state.ring_capacity.store(cap, Ordering::Relaxed);
        state.rg_mode.store(rg as u8, Ordering::Relaxed);
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(cap);
        let st = Arc::clone(&state);
        let list = paths.to_vec();
        let handle = thread::spawn(move || {
            let mut eq_chain = crate::eq::EqChain::new();
            let eq_presets = crate::eq::builtin_presets();
            let mut fx_chain = crate::effects::EffectsChain::new(output_rate as f32);
            let fx_presets = crate::effects::builtin_presets();
            let mut cf_filter = crate::crossfeed::CrossfeedFilter::new();
            let cf_presets = crate::crossfeed::builtin_presets();
            decode_playlist(
                &list, 0, &mut producer, &st, output_rate, false,
                &mut eq_chain, &eq_presets, &mut fx_chain, &fx_presets,
                0, &mut cf_filter, &cf_presets,
            );
        });
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            while let Ok(s) = consumer.pop() {
                out.push(s);
            }
            if state.producer_done.load(Ordering::Relaxed) && consumer.slots() == 0 {
                break;
            }
            if Instant::now() > deadline {
                state.quit();
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let _ = handle.join();
        if let Ok(mut e) = state.decode_error.lock() {
            if let Some(msg) = e.take() {
                panic!("decode error: {}", msg);
            }
        }
        out
    }

    // ---- analysis ----------------------------------------------------------

    fn channels(out: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let l = out.iter().step_by(2).copied().collect();
        let r = out.iter().skip(1).step_by(2).copied().collect();
        (l, r)
    }

    /// Goertzel amplitude of a tone, measured over the middle of the signal
    /// (away from edge transients). The window is trimmed to a whole number
    /// of cycles of the target frequency — an off-bin tone suffers up to
    /// ~3.9 dB of rectangular-window scalloping loss, which read as a fake
    /// 17% level drop in the resampler test before this trim.
    fn tone_amplitude(samples: &[f32], rate: f32, freq: f32) -> f32 {
        let total = samples.len();
        assert!(total > 8192, "not enough samples to analyze: {}", total);
        let win = &samples[2048..total - 2048];
        let cycles = (freq as f64 * win.len() as f64 / rate as f64).floor();
        let n = ((cycles * rate as f64 / freq as f64).round() as usize).min(win.len());
        let win = &win[..n];
        let n = n as f64;
        let k = cycles;
        let w = 2.0 * std::f64::consts::PI * k / n;
        let coeff = 2.0 * w.cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &x in win {
            let s0 = x as f64 + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        (2.0 * power.max(0.0).sqrt() / n) as f32
    }

    // ---- tests ---------------------------------------------------------

    #[test]
    fn chain_preserves_stereo_identity_pitch_amplitude_and_duration() {
        let path = tmp_wav("stereo");
        let l = sine(44100, 1.0, 440.0, 0.5);
        let r = sine(44100, 1.0, 1000.0, 0.5);
        write_wav(&path, 44100, 2, WavFmt::Pcm16, &interleave(&[l, r]));
        let out = run_chain(&[path], 44100, RgMode::Off);
        let frames = out.len() / 2;
        assert!((frames as i64 - 44100).unsigned_abs() < 1024, "duration: {} frames", frames);
        let (l, r) = channels(&out);
        assert!((tone_amplitude(&l, 44100.0, 440.0) - 0.5).abs() < 0.05, "L tone level");
        assert!(tone_amplitude(&l, 44100.0, 1000.0) < 0.05, "R leaked into L");
        assert!((tone_amplitude(&r, 44100.0, 1000.0) - 0.5).abs() < 0.05, "R tone level");
        assert!(tone_amplitude(&r, 44100.0, 440.0) < 0.05, "L leaked into R");
    }

    #[test]
    fn chain_plays_mono_at_correct_speed_into_both_channels() {
        // The historical bug: mono played at 2x speed (interleave assumed
        // stereo). Duration alone catches it — 1 s of mono must come out as
        // ~44100 stereo frames, not ~22050.
        let path = tmp_wav("mono");
        write_wav(&path, 44100, 1, WavFmt::Pcm16, &sine(44100, 1.0, 440.0, 0.5));
        let out = run_chain(&[path], 44100, RgMode::Off);
        let frames = out.len() / 2;
        assert!((frames as i64 - 44100).unsigned_abs() < 1024, "duration: {} frames", frames);
        let (l, r) = channels(&out);
        assert!((tone_amplitude(&l, 44100.0, 440.0) - 0.5).abs() < 0.05);
        let max_diff = l.iter().zip(&r).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(max_diff < 1e-3, "mono must duplicate identically: max L-R diff {}", max_diff);
    }

    #[test]
    fn chain_resamples_preserving_duration_and_pitch() {
        // 44.1 kHz file on a 48 kHz output: engages the resampler including
        // its EOF flush (the class that appended ~23 ms of silence).
        let path = tmp_wav("resample");
        let l = sine(44100, 1.0, 440.0, 0.5);
        let r = sine(44100, 1.0, 1000.0, 0.5);
        write_wav(&path, 44100, 2, WavFmt::Pcm16, &interleave(&[l, r]));
        let out = run_chain(&[path], 48000, RgMode::Off);
        let frames = out.len() / 2;
        assert!(
            (frames as i64 - 48000).unsigned_abs() < 1500,
            "resampled duration: {} frames (want ~48000)",
            frames
        );
        let (l, r) = channels(&out);
        let al = tone_amplitude(&l, 48000.0, 440.0);
        let ar = tone_amplitude(&r, 48000.0, 1000.0);
        assert!((al - 0.5).abs() < 0.06, "440 Hz level through resampler: {}", al);
        assert!((ar - 0.5).abs() < 0.06, "1000 Hz level through resampler: {}", ar);
    }

    #[test]
    fn chain_decodes_24bit_and_float_wavs() {
        // The convert_samples fallthrough class: non-16-bit sources used to
        // produce silence or clicks.
        for (name, fmt) in [("s24", WavFmt::Pcm24), ("f32", WavFmt::F32)] {
            let path = tmp_wav(name);
            write_wav(&path, 44100, 1, fmt, &sine(44100, 1.0, 440.0, 0.5));
            let out = run_chain(&[path], 44100, RgMode::Off);
            let frames = out.len() / 2;
            assert!((frames as i64 - 44100).unsigned_abs() < 1024, "{}: {} frames", name, frames);
            let (l, _) = channels(&out);
            let amp = tone_amplitude(&l, 44100.0, 440.0);
            assert!((amp - 0.5).abs() < 0.05, "{}: tone level {}", name, amp);
        }
    }

    #[test]
    fn chain_downmixes_5_1_itu_style() {
        // SMPTE order FL FR FC LFE BL BR. FL carries 440, FC carries 1000,
        // LFE carries 330 (must be DROPPED). Expect: L = 440 at full level +
        // 1000 at -3 dB; R = 1000 at -3 dB only; 330 nowhere.
        let rate = 44100;
        let silent = vec![0.0f32; rate as usize];
        let chs = [
            sine(rate, 1.0, 440.0, 0.4),  // FL
            silent.clone(),                // FR
            sine(rate, 1.0, 1000.0, 0.4), // FC
            sine(rate, 1.0, 330.0, 0.8),  // LFE
            silent.clone(),                // BL
            silent,                        // BR
        ];
        let path = tmp_wav("five_one");
        write_wav(&path, rate, 6, WavFmt::Pcm16, &interleave(&chs));
        let out = run_chain(&[path], rate, RgMode::Off);
        let (l, r) = channels(&out);
        let g = std::f32::consts::FRAC_1_SQRT_2;
        assert!((tone_amplitude(&l, 44100.0, 440.0) - 0.4).abs() < 0.05, "FL into L");
        assert!(tone_amplitude(&r, 44100.0, 440.0) < 0.05, "FL must not reach R");
        assert!((tone_amplitude(&l, 44100.0, 1000.0) - 0.4 * g).abs() < 0.05, "center at -3 dB into L");
        assert!((tone_amplitude(&r, 44100.0, 1000.0) - 0.4 * g).abs() < 0.05, "center at -3 dB into R");
        assert!(tone_amplitude(&l, 44100.0, 330.0) < 0.05, "LFE must be dropped (L)");
        assert!(tone_amplitude(&r, 44100.0, 330.0) < 0.05, "LFE must be dropped (R)");
    }

    #[test]
    fn chain_decodes_flac_fixture_losslessly() {
        let out = run_chain(&[fixture("sine_lr.flac")], 44100, RgMode::Off);
        let frames = out.len() / 2;
        assert!((frames as i64 - 44100).unsigned_abs() < 1024, "duration: {} frames", frames);
        let (l, r) = channels(&out);
        assert!((tone_amplitude(&l, 44100.0, 440.0) - 0.5).abs() < 0.05);
        assert!(tone_amplitude(&l, 44100.0, 1000.0) < 0.05);
        assert!((tone_amplitude(&r, 44100.0, 1000.0) - 0.5).abs() < 0.05);
        assert!(tone_amplitude(&r, 44100.0, 440.0) < 0.05);
    }

    #[test]
    fn chain_decodes_mp3_fixture_within_lossy_tolerances() {
        // MP3 has encoder delay/padding, so duration is loose; joint stereo
        // and psychoacoustics smear levels a little.
        let out = run_chain(&[fixture("sine_lr.mp3")], 44100, RgMode::Off);
        let frames = out.len() / 2;
        assert!(
            (frames as i64 - 44100).unsigned_abs() < 4410,
            "mp3 duration: {} frames (want 44100 +/- 10%)",
            frames
        );
        let (l, r) = channels(&out);
        assert!(tone_amplitude(&l, 44100.0, 440.0) > 0.35, "L tone survived encoding");
        assert!(tone_amplitude(&l, 44100.0, 1000.0) < 0.1, "stereo separation");
        assert!(tone_amplitude(&r, 44100.0, 1000.0) > 0.35, "R tone survived encoding");
    }

    #[test]
    fn chain_applies_replaygain_track_gain() {
        // Fixture is tagged REPLAYGAIN_TRACK_GAIN=-6.02 dB: 0.5 amplitude in,
        // ~0.25 out when rg-mode is Track.
        let out = run_chain(&[fixture("sine_lr_rg.flac")], 44100, RgMode::Track);
        let (l, _) = channels(&out);
        let amp = tone_amplitude(&l, 44100.0, 440.0);
        assert!((amp - 0.25).abs() < 0.04, "rg-adjusted level: {} (want ~0.25)", amp);
    }

    #[test]
    fn chain_applies_replaygain_to_resampler_flush_tail() {
        // 44.1 kHz RG-tagged file on a 48 kHz output engages the resampler's
        // EOF flush. The flush used to push raw resampled samples straight to
        // the ring — skipping EQ/FX/ReplayGain/crossfeed/balance — so the last
        // ~20 ms of every resampled track stepped back up to unprocessed level
        // (+6 dB here: 0.5 instead of 0.25).
        let out = run_chain(&[fixture("sine_lr_rg.flac")], 48000, RgMode::Track);
        assert!(out.len() > 2048, "not enough output to inspect: {}", out.len());
        let tail = &out[out.len() - 600..];
        let peak = tail.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(
            peak < 0.35,
            "flush tail must be ReplayGain-attenuated (~0.25 peak), got {peak}"
        );
    }
}
