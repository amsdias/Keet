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

/// Most channels a declared layout is folded by position; wider ones (and any
/// layout without positions) use the count-based fold.
const MAX_POSITIONED_CHANNELS: usize = 32;

/// Append `input` (interleaved) to `out` as interleaved stereo, using the
/// buffer's declared speaker positions when it has them. The count-based fold
/// (`interleaved_to_stereo`) has to guess a layout from the channel count, and
/// guessed wrong for 2.1 (its LFE taken for a centre and mixed into both
/// sides) and 6.1 (rear centre and side surrounds alternated left/right). Mono
/// and stereo always take the direct paths, which leave samples untouched.
fn downmix_to_stereo(input: &[f32], channels: &symphonia::core::audio::Channels, out: &mut Vec<f32>) {
    use symphonia::core::audio::Channels;
    let count = channels.count();
    let mask = match channels {
        Channels::Positioned(mask) if count > 2 && count <= MAX_POSITIONED_CHANNELS => *mask,
        _ => return interleaved_to_stereo(input, count.max(1), out),
    };
    let mut weights = [(0.0f32, 0.0f32); MAX_POSITIONED_CHANNELS];
    let mut n = 0;
    for bit in 0..64 {
        if mask.bits() & (1u64 << bit) != 0 {
            weights[n] = position_weights(bit);
            n += 1;
        }
    }
    out.reserve(input.len() / n * 2);
    for frame in input.chunks_exact(n) {
        let (mut l, mut r) = (0.0, 0.0);
        for (&s, &(wl, wr)) in frame.iter().zip(&weights[..n]) {
            l += wl * s;
            r += wr * s;
        }
        out.push(l);
        out.push(r);
    }
}

/// (left, right) weight of the speaker at `bit` of symphonia's `Position`
/// mask (WAVEFORMATEXTENSIBLE order for the first 18): front left/right at
/// unity, everything else that has a side at -3 dB on that side, centred
/// speakers at -3 dB on both, LFE dropped (as in the count-based fold).
fn position_weights(bit: u32) -> (f32, f32) {
    const G: f32 = std::f32::consts::FRAC_1_SQRT_2;
    match bit {
        0 => (1.0, 0.0),                       // front left
        1 => (0.0, 1.0),                       // front right
        3 | 18 => (0.0, 0.0),                  // LFE 1, LFE 2
        4 | 6 | 9 | 12 | 15 | 19 | 22 | 24 => (G, 0.0), // rear/left-of-centre/side/top/bottom/wide left
        5 | 7 | 10 | 14 | 17 | 20 | 23 | 25 => (0.0, G), // their right-hand counterparts
        _ => (G, G),                           // centre, rear/top/bottom centre, unknown
    }
}

fn deinterleave_into(samples: &[f32], ch: usize, out: &mut Vec<Vec<f32>>) {
    out.resize_with(ch, Vec::new);
    for plane in out.iter_mut() { plane.clear(); }
    for (i, &s) in samples.iter().enumerate() {
        out[i % ch].push(s);
    }
}

/// Where playback is within a chained stream (concatenated Ogg files,
/// internet-radio rips): each link's packets are timed from its own start, and
/// the container reports only the first link's length.
#[derive(Default)]
struct Links {
    /// Where the current link starts in the file.
    start_secs: f64,
    /// The current link (0 = first).
    index: usize,
    /// The furthest link whose length is already in `total_samples`.
    counted: usize,
}

/// Move onto the next link of a chained stream (the demuxer returned
/// ResetRequired): re-select the audio track and rebuild the decoder. The
/// track grows by each link the first time it is reached, so the progress bar
/// and seek range cover the whole file. False when the link cannot continue
/// the track (no audio, a different sample rate — that would need a new
/// resampler — or no decoder).
fn enter_next_link(
    format: &mut dyn symphonia::core::formats::FormatReader,
    track: &mut symphonia::core::formats::Track,
    decoder: &mut Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
    links: &mut Links,
    sample_rate: u32,
    state: &PlayerState,
) -> bool {
    let Some(next) = format.default_track(TrackType::Audio).cloned() else { return false };
    let Some(params) = next.codec_params.as_ref().and_then(|c| c.audio()).cloned() else { return false };
    if params.sample_rate.unwrap_or(sample_rate) != sample_rate {
        return false;
    }
    let Ok(d) = symphonia::default::get_codecs().make_audio_decoder(&params, &AudioDecoderOptions::default()) else {
        return false;
    };
    links.start_secs += track.num_frames.unwrap_or(0) as f64 / sample_rate as f64;
    links.index += 1;
    if links.index > links.counted {
        links.counted = links.index;
        if let Some(n) = next.num_frames {
            state.total_samples.fetch_add(n, Ordering::Relaxed);
        }
    }
    *decoder = d;
    *track = next;
    true
}

/// The reader, track and decoder of a file's FIRST link, for a seek back
/// past the link playing. None if the file no longer opens as before.
#[allow(clippy::type_complexity)]
fn open_first_link(
    path: &std::path::Path,
    sample_rate: u32,
) -> Option<(
    Box<dyn symphonia::core::formats::FormatReader>,
    symphonia::core::formats::Track,
    Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
)> {
    let mss = MediaSourceStream::new(Box::new(File::open(path).ok()?), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let meta_opts = MetadataOptions::default().limit_visual_bytes(Limit::Maximum(0));
    let format = symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), meta_opts).ok()?;
    let track = format.default_track(TrackType::Audio)?.clone();
    let params = track.codec_params.as_ref()?.audio()?.clone();
    if params.sample_rate.unwrap_or(sample_rate) != sample_rate {
        return None;
    }
    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default())
        .ok()?;
    Some((format, track, decoder))
}

/// A track that cannot be opened or decoded is skipped. Only when nothing is
/// playing yet (the producer's first track) does that change what is on
/// screen now. Otherwise the previous track still has up to a ring's depth
/// (~4 s) to play, and signalling here moved the title, clock and lyrics on
/// that early; the next playable track's own transition — which waits for the
/// ring to drain — signals instead, and shows the stored skip error with it.
fn signal_unplayable(state: &PlayerState, first_iteration: bool, next: usize) {
    if first_iteration {
        state.signal_next_track(next);
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
        if !state.reset_consumer_counter.load(Ordering::Acquire) || producer_should_unstick(state) {
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

/// Damaged packets in a row a track survives before it is ended: past this
/// the stream is unreadable from here on, not just a bad frame.
const MAX_BAD_PACKETS: usize = 64;

/// Whether a packet-read error is a damaged packet to skip (true) or the end
/// of what can be read (false): a malformed packet is skipped while fewer
/// than `MAX_BAD_PACKETS` have failed in a row; I/O errors and anything else
/// end the track.
fn skip_bad_packet(e: &symphonia::core::errors::Error, bad_in_a_row: usize) -> bool {
    matches!(e, symphonia::core::errors::Error::DecodeError(_)) && bad_in_a_row < MAX_BAD_PACKETS
}

/// The chunk's peak for the clip flag and the limiter. A non-finite sample
/// counts as infinitely loud: `f32::max` skips NaN, so a NaN chunk read as
/// quiet, the limiter (which zeroes non-finite samples) never engaged, and
/// the NaN went on to the output.
fn chain_peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .fold(0.0f32, |m, &s| if s.is_finite() { m.max(s.abs()) } else { f32::INFINITY })
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

    // Clipping detection — flag for the UI when the DSP output (before the
    // limiter below) times volume would exceed 0 dBFS. That is the useful
    // signal: "the limiter is working, lower the EQ preamp". The audio
    // callback still clamps post-gain, since volume can change between this
    // scan and consumption (~ring-buffer-depth latency).
    let vol = state.volume.load(Ordering::Relaxed) as f32 / 100.0;
    let peak = chain_peak(if using_final_buf { &bufs.final_buf } else { bal_output });
    if peak * vol > 1.0 {
        state.clipping.store(true, Ordering::Relaxed);
    }

    // Output limiter: the LAST DSP stage, so EQ, effects, ReplayGain,
    // crossfeed and crossfade boosts are all caught. It used to live inside
    // the effects chain — running only with an effect on, and before
    // ReplayGain, so a hot master was squashed and then turned down anyway
    // while every other boost went straight to the callback's hard clamp.
    // Zero-copy while idle: it only touches samples when engaged.
    if effects.limiter_engaged(peak) {
        if !using_final_buf {
            bufs.final_buf.clear();
            bufs.final_buf.extend_from_slice(bal_output);
            using_final_buf = true;
        }
        effects.limit_output(&mut bufs.final_buf);
    }

    let out: &[f32] = if using_final_buf { &bufs.final_buf } else { bal_output };

    // With crossfade on, the newest `crossfade_samples` are HELD BACK in
    // `tail_buf` instead of being pushed: they are the part of this track that
    // will play underneath the next one. Only what falls off the front of the
    // hold is pushed. Pushing everything and keeping a copy (the old code)
    // played the tail twice — once in full, then again faded under the next
    // track — so there was never a real overlap. Whoever ends the track and
    // does NOT hand the tail on must push it (`push_held_tail`).
    match tail_buf {
        Some(tb) => {
            tb.extend(out.iter().copied());
            if tb.len() > crossfade_samples {
                let excess = tb.len() - crossfade_samples;
                push_deque_front(producer, state, tb, excess);
            }
        }
        None => push_all(producer, state, out),
    }
}

/// Push the first `n` samples of `dq` to the ring and remove them. `n` is even
/// (the deque only ever grows by whole stereo frames), so the ring's L/R
/// alignment survives.
fn push_deque_front(
    producer: &mut Producer<f32>,
    state: &PlayerState,
    dq: &mut std::collections::VecDeque<f32>,
    n: usize,
) {
    let (a, b) = dq.as_slices();
    let from_a = n.min(a.len());
    push_all(producer, state, &a[..from_a]);
    push_all(producer, state, &b[..n - from_a]);
    dq.drain(..n);
}

/// Play a held-back crossfade tail unmixed: the track ended and nothing is
/// going to fade in over it (last track, repeat-one, a rate-change rebuild).
fn push_held_tail(
    producer: &mut Producer<f32>,
    state: &PlayerState,
    tail: &mut std::collections::VecDeque<f32>,
) {
    let n = tail.len();
    push_deque_front(producer, state, tail, n);
}

/// Take a pending seek that targets a track whose decode has already finished
/// (its audio still draining from the ring). `None` = no seek pending.
/// `Some(Some(t))` = reopen that track at `t` seconds; `Some(None)` = the target
/// lies past its end, so just drop the rest of it.
fn take_seek_for_finished_track(state: &PlayerState) -> Option<Option<f64>> {
    let rel = state.take_seek();
    if rel == 0 {
        return None;
    }
    let target = (state.time_secs() + rel as f64).max(0.0);
    let len = state.total_secs();
    Some((len <= 0.0 || target < len).then_some(target))
}

/// Wait for the final track's audio to finish playing. Returns a seek target
/// if one arrives meanwhile and lands inside the track (it is then reopened);
/// a seek past the end drops the remaining audio and returns `None`.
fn wait_out_final_tail(
    producer: &mut Producer<f32>,
    state: &PlayerState,
    ring_capacity: usize,
) -> Option<f64> {
    loop {
        if producer_should_unstick(state) || ring_capacity - producer.slots() == 0 {
            return None;
        }
        if let Some(target) = take_seek_for_finished_track(state) {
            state.reset_consumer_counter.store(true, Ordering::Release);
            await_consumer_drain(state);
            return target;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// A resampler handed from one track to the next so a gapless join stays
/// continuous. A fresh resampler per track starts from an empty delay line, so
/// every boundary of a resampled album (44.1 kHz on a 48 kHz device — the most
/// common setup) got the filter's start-up latency as a burst of near-silence
/// and a discontinuity. Carrying the resampler AND its unconsumed input across
/// the boundary makes a split file play back identical to the unsplit one.
struct ResamplerCarry {
    resampler: Async<f32>,
    /// Source rate the resampler was built for; the next track reuses it only
    /// at the same rate.
    src_rate: u32,
    /// Input frames not yet consumed (less than one chunk).
    pending: Vec<f32>,
    /// ReplayGain of the track `pending` came from. Gain commutes with the
    /// (linear) resampler and EQ, so the next track rescales `pending` by
    /// old/new to keep these frames at their own track's gain.
    rg_linear: f32,
}

/// Drain a resampler at the end of its input: feed the final partial chunk
/// with `partial_len` (rubato pads internally), pump zeros to empty the sinc
/// delay line, and emit exactly ceil(ratio·pending) + output_delay frames.
/// Zero-padding `pending` to a full chunk and pushing everything instead
/// appended ~23 ms of resampled silence per track — a gap in gapless albums.
/// Runs even when `pending` is empty: the delay line still holds the real
/// last few ms of the track (skipping it dropped them whenever a track's
/// length was an exact multiple of the chunk size).
#[allow(clippy::too_many_arguments)] // producer-thread chain context, as apply_chain_and_push
fn flush_resampler(
    resampler: &mut Async<f32>,
    pending: &[f32],
    src_rate: u32,
    output_rate: u32,
    resamp_out: &mut Vec<f32>,
    flush_planes: &mut Vec<Vec<f32>>,
    producer: &mut Producer<f32>,
    state: &PlayerState,
    eq: &mut crate::eq::EqChain,
    effects: &mut crate::effects::EffectsChain,
    crossfeed: &mut crate::crossfeed::CrossfeedFilter,
    rg_linear: f32,
    xfade_in: Option<&std::collections::VecDeque<f32>>,
    crossfade_pos: &mut usize,
    crossfade_samples: usize,
    mut tail_buf: Option<&mut std::collections::VecDeque<f32>>,
    chain_bufs: &mut ChainBufs,
) {
    let channels = 2usize;
    let chunk_size = resampler.input_frames_next();
    let pending_frames = pending.len() / channels;
    let ratio = output_rate as f64 / src_rate as f64;
    let mut frames_wanted =
        (pending_frames as f64 * ratio).ceil() as usize + resampler.output_delay();
    if resamp_out.len() < resampler.output_frames_max() * channels {
        resamp_out.resize(resampler.output_frames_max() * channels, 0.0);
    }
    deinterleave_into(pending, channels, flush_planes);
    // The adapter needs full chunk geometry; rubato only reads the first
    // `partial_len` frames of it.
    for plane in flush_planes.iter_mut() {
        plane.resize(chunk_size, 0.0);
    }
    let mut partial = Some(pending_frames);
    while frames_wanted > 0 {
        let Ok(adapter_in) = SequentialSliceOfVecs::new(&*flush_planes, channels, chunk_size) else {
            break;
        };
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(partial.take().unwrap_or(0)),
            active_channels_mask: None,
        };
        let out_frames = resampler.output_frames_next();
        let Ok(mut out_buf) = InterleavedSlice::new_mut(&mut resamp_out[..], channels, out_frames) else {
            break;
        };
        match resampler.process_into_buffer(&adapter_in, &mut out_buf, Some(&indexing)) {
            Ok((_, nbr_out)) => {
                if nbr_out == 0 {
                    break;
                }
                let take = frames_wanted.min(nbr_out);
                // Through the full DSP chain, same as the packet loop — pushing
                // raw here skipped EQ/RG/etc. for the final ~20 ms of every
                // resampled track.
                apply_chain_and_push(
                    &resamp_out[..take * channels], producer, state,
                    eq, effects, crossfeed, rg_linear,
                    xfade_in, crossfade_pos, crossfade_samples,
                    tail_buf.as_deref_mut(),
                    chain_bufs,
                );
                frames_wanted -= take;
            }
            Err(_) => break,
        }
    }
}

/// Flush a carried resampler that no track is going to continue (the next
/// track is at another rate, the playlist ended, or the stream is about to be
/// rebuilt). Rare path, so it allocates its own scratch.
fn flush_carry(
    mut c: ResamplerCarry,
    output_rate: u32,
    producer: &mut Producer<f32>,
    state: &PlayerState,
    eq: &mut crate::eq::EqChain,
    effects: &mut crate::effects::EffectsChain,
    crossfeed: &mut crate::crossfeed::CrossfeedFilter,
) {
    let mut out = Vec::new();
    let mut planes = Vec::with_capacity(2);
    let mut bufs = ChainBufs::with_capacity(c.resampler.output_frames_max() * 2);
    let mut pos = 0;
    flush_resampler(
        &mut c.resampler, &c.pending, c.src_rate, output_rate, &mut out, &mut planes,
        producer, state, eq, effects, crossfeed, c.rg_linear, None, &mut pos, 0, None, &mut bufs,
    );
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
                rg.track_peak = crate::metadata::parse_rg_peak_value(s);
            }
            Some(StandardTag::ReplayGainAlbumGain(s)) if rg.album_gain.is_none() => {
                rg.album_gain = crate::metadata::parse_rg_gain_value(s);
            }
            Some(StandardTag::ReplayGainAlbumPeak(s)) if rg.album_peak.is_none() => {
                rg.album_peak = crate::metadata::parse_rg_peak_value(s);
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
                    rg.track_peak = crate::metadata::parse_rg_peak_value(s);
                }
                "replaygain_album_gain" if rg.album_gain.is_none() => {
                    rg.album_gain = crate::metadata::parse_rg_gain_value(s);
                }
                "replaygain_album_peak" if rg.album_peak.is_none() => {
                    rg.album_peak = crate::metadata::parse_rg_peak_value(s);
                }
                _ => {}
            }
        }
    }
}

/// The frames of a decoded buffer to keep, as (first, end) offsets into it:
/// the buffer holds `n` frames starting at stream frame `first`, and only
/// [delay, delay + length) is the track's real audio.
fn gapless_keep(g: crate::gapless::Gapless, first: u64, n: u64) -> (usize, usize) {
    let start = g.delay;
    let end = g.length.map_or(u64::MAX, |l| start + l);
    let lo = start.saturating_sub(first).min(n);
    let hi = end.saturating_sub(first).min(n).max(lo);
    (lo as usize, hi as usize)
}

/// Largest ReplayGain adjustment applied, either way, in dB.
const RG_GAIN_LIMIT_DB: f32 = 24.0;

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

    // A sane range: real ReplayGain values sit within about ±20 dB, and a
    // corrupt tag must not turn into a 60 dB boost (the limiter would then
    // flatten the whole track) or a near-mute.
    let mut linear = 10.0_f32.powf(gain_db.clamp(-RG_GAIN_LIMIT_DB, RG_GAIN_LIMIT_DB) / 20.0);

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
    // Resampler handed from the previous track across a gapless join.
    let mut carry: Option<ResamplerCarry> = None;
    // The previous track ended by a user skip rather than running out. Only a
    // jump in the audio (skip, seek, a new producer) should reset the DSP
    // filters; a natural track change is continuous audio and must not.
    let mut prev_track_skipped = false;
    // The last track whose audio reached the ring, and an absolute seek target
    // to apply when a track is reopened (see `drain_or_seek`).
    let mut last_track: Option<usize> = None;
    let mut initial_seek: Option<f64> = None;
    // The size of the ring this producer actually writes to, read from the
    // ring itself: `state.ring_capacity` can describe a different ring (a
    // reopen at another rate that failed kept the old one), and then
    // `ring_capacity - producer.slots()` underflowed.
    let ring_capacity = producer.buffer().capacity();

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
        state.producer_decoding.store(track_index, Ordering::Relaxed);

        // --- Open file and probe format ---
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                if let Ok(mut err) = state.decode_error.lock() {
                    *err = Some(format!("{}: {}", path.display(), e));
                }
                signal_unplayable(state, first_iteration, track_index + 1);
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
                signal_unplayable(state, first_iteration, track_index + 1);
                track_index += 1;
                continue;
            }
        };

        let mut track = match format.default_track(TrackType::Audio) {
            Some(t) => t.clone(),
            None => {
                if let Ok(mut err) = state.decode_error.lock() {
                    *err = Some(format!("{}: No audio track", path.display()));
                }
                signal_unplayable(state, first_iteration, track_index + 1);
                track_index += 1;
                continue;
            }
        };

        let mut track_id = track.id;
        // 0.6: codec_params is Option and splits per media type, so the audio
        // parameters come out of .audio(); track length moved onto Track.
        let audio_params = match track.codec_params.as_ref().and_then(|c| c.audio()) {
            Some(a) => a.clone(),
            None => {
                if let Ok(mut err) = state.decode_error.lock() {
                    *err = Some(format!("{}: No audio codec parameters", path.display()));
                }
                signal_unplayable(state, first_iteration, track_index + 1);
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
        // 0 = no bit depth: lossy codecs (AAC, MP3, Vorbis, Opus) have none,
        // and showing "16-bit" for them (a guess) put it on every .m4a.
        let bits_per_sample = audio_params.bits_per_sample.unwrap_or(0);
        let total = track.num_frames.unwrap_or(0);

        let mut decoder = match symphonia::default::get_codecs()
            .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        {
            Ok(d) => d,
            Err(e) => {
                if let Ok(mut err) = state.decode_error.lock() {
                    *err = Some(format!("{}: {}", path.display(), e));
                }
                signal_unplayable(state, first_iteration, track_index + 1);
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
        let revisions = crate::metadata::revisions_newest_first(format.as_mut());
        for rev in &revisions {
            extract_rg_from_tags(&rev.media.tags, &mut rg_tags);
        }
        let rg_linear = compute_rg_gain(state.rg_mode(), &rg_tags);

        // AAC in MP4: the real audio inside the decoded stream, in frames at
        // the track's sample rate (see gapless.rs).
        let gapless = crate::gapless::for_mp4(path, &revisions, sample_rate);
        let total = gapless.and_then(|g| g.length).unwrap_or(total);
        // Priming frames are not part of the track's time: the clock and seek
        // targets are shifted by them.
        let priming_secs = gapless.map_or(0.0, |g| g.delay as f64 / sample_rate as f64);

        // --- Create resampler if needed ---
        // Created before the drain wait so a failure skips the track like the
        // decoder-creation failures above. Falling back to "no resampler" here
        // would silently play the track at the wrong pitch.
        // A carried resampler at this track's rate continues the stream; one
        // at any other rate is flushed here, before this track pushes anything.
        let mut carried_pending: Option<Vec<f32>> = None;
        let reuse = match carry.take() {
            Some(c) if sample_rate != output_rate && c.src_rate == sample_rate => Some(c),
            Some(c) => {
                flush_carry(c, output_rate, producer, state, eq, effects, crossfeed);
                None
            }
            None => None,
        };
        let mut resampler: Option<Async<f32>> = if let Some(c) = reuse {
            let mut p = c.pending;
            if rg_linear > 0.0 && c.rg_linear != rg_linear {
                let k = c.rg_linear / rg_linear;
                for x in p.iter_mut() {
                    *x *= k;
                }
            }
            carried_pending = Some(p);
            Some(c.resampler)
        } else if sample_rate != output_rate {
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
                    signal_unplayable(state, first_iteration, track_index + 1);
                    track_index += 1;
                    continue;
                }
            }
        } else {
            None
        };

        let mut broke_for_skip = false;
        let mut skipped = false;
        // Set when a seek aimed at the previous track needs it reopened.
        let mut reopen: Option<(usize, f64)> = None;

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
                    state.reset_consumer_counter.store(true, Ordering::Release);
                    await_consumer_drain(state);
                    // The held tail and any carried resampler input belong to
                    // the track being skipped; neither may reach the next one.
                    crossfade_tail = None;
                    if carried_pending.take().is_some() {
                        if let Some(ref mut r) = resampler {
                            r.reset();
                        }
                    }
                    prev_track_skipped = true;
                    break;
                }
                // A seek here is aimed at the PREVIOUS track: its decode has
                // finished, but its last seconds are still playing and it is
                // still the track on screen. Applying it to this track's clock
                // (which is about to be zeroed) jumped the next track ahead or
                // silently dropped a backward seek.
                if let Some(target) = take_seek_for_finished_track(state) {
                    state.reset_consumer_counter.store(true, Ordering::Release);
                    await_consumer_drain(state);
                    crossfade_tail = None;
                    if carried_pending.take().is_some() {
                        if let Some(ref mut r) = resampler {
                            r.reset();
                        }
                    }
                    prev_track_skipped = true;
                    reopen = last_track.zip(target);
                    break;
                }
                if state.is_paused() {
                    thread::sleep(Duration::from_millis(50));
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            if broke_for_skip { break; }
            if let Some((a, t)) = reopen {
                track_index = a;
                initial_seek = Some(t);
                continue;
            }
        }

        // --- Update track info ---
        state.track_info_ready.store(false, Ordering::Relaxed);
        state.sample_rate.store(sample_rate as u64, Ordering::Relaxed);
        state.total_samples.store(total, Ordering::Relaxed);
        state.samples_played.store(0, Ordering::Relaxed);
        // Whatever is still queued belongs to the previous track; the clock
        // starts counting this one only once that has played (see state.rs).
        state.clock_preroll.store(
            ((ring_capacity - producer.slots()) / 2) as u64,
            Ordering::Relaxed,
        );
        last_track = Some(track_index);
        state.channels.store(src_channels, Ordering::Relaxed);
        state.bits_per_sample.store(bits_per_sample as usize, Ordering::Relaxed);
        let rg_db = if rg_linear == 1.0 { 0.0 } else { 20.0 * rg_linear.log10() };
        state.rg_gain_db.store(rg_db.to_bits(), Ordering::Relaxed);
        state.track_info_ready.store(true, Ordering::Relaxed);

        // Signal track transition (skip for the producer's first track — main
        // thread already knows). Repeat-one passes signal too, so the UI
        // resets lyrics scroll and position for the restarted track.
        if !first_iteration {
            state.signal_next_track(track_index);
        }
        // Reset the DSP filters only where the audio actually jumps: the first
        // track of a producer (it follows a skip-back, a jump, a respawn) or
        // after a skip. A natural track change is continuous audio — resetting
        // there restarted the biquads from zero state (a step right at the
        // join, audible on bass-heavy EQ) and cut reverb/delay tails dead.
        if first_iteration || prev_track_skipped {
            eq.reset();
            effects.reset();
            crossfeed.reset();
        }
        first_iteration = false;

        // --- Crossfade setup for this track ---
        let xfade_in = crossfade_tail.take();
        let mut crossfade_pos: usize = 0;
        let capture_tail = crossfade_samples > 0;
        let mut tail_buf: std::collections::VecDeque<f32> = if capture_tail { std::collections::VecDeque::with_capacity(crossfade_samples) } else { std::collections::VecDeque::new() };

        let chunk_size = resampler.as_ref().map(|r| r.input_frames_next()).unwrap_or(1024);
        // Start time of the last decoded packet — where decoding resumes if a
        // seek fails after the ring has been drained.
        let mut decode_pos_secs = 0.0f64;
        // Damaged packets: skipped, said once per track, and only a long run
        // of them (the stream is unreadable from here) ends the track.
        let mut bad_in_a_row = 0usize;
        let mut damage_reported = false;
        let mut report_damage = |state: &PlayerState| {
            if !damage_reported {
                damage_reported = true;
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if let Ok(mut n) = state.decode_notice.lock() {
                    *n = Some(format!("{name}: damaged audio skipped"));
                }
            }
        };
        // Where playback is in a chained stream (concatenated Ogg).
        let mut links = Links::default();
        let chained_capable = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "ogg" | "oga" | "opus"));
        let mut pending: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        if let Some(p) = carried_pending.take() {
            pending.extend_from_slice(&p);
        }

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
                    state.reset_consumer_counter.store(true, Ordering::Release);
                    await_consumer_drain(state);
                }
                skipped = true;
                break;
            }

            // Handle seek. `initial_seek` is an absolute target carried in by a
            // reopen; relative requests made meanwhile stack on top of it.
            let rel = state.take_seek();
            let seek_to = match initial_seek.take() {
                Some(t) => Some((t + rel as f64).max(0.0)),
                None if rel != 0 => Some((state.time_secs() + rel as f64).max(0.0)),
                None => None,
            };
            if let Some(new_time) = seek_to {
                // A chained stream's demuxer seeks within ONE link: bring the
                // reader to the link that holds the target first. Before the
                // current link, start over from the first; past it, skip
                // through packets (no decoding) link by link.
                if chained_capable {
                    if new_time < links.start_secs {
                        if let Some((f, t, d)) = open_first_link(path, sample_rate) {
                            (format, track, decoder) = (f, t, d);
                            track_id = track.id;
                            links.start_secs = 0.0;
                            links.index = 0;
                        }
                    }
                    while let Some(n) = track.num_frames {
                        if new_time < links.start_secs + n as f64 / sample_rate as f64 {
                            break;
                        }
                        match format.next_packet() {
                            Ok(Some(_)) => {}
                            Err(symphonia::core::errors::Error::ResetRequired) => {
                                if !enter_next_link(format.as_mut(), &mut track, &mut decoder, &mut links, sample_rate, state) {
                                    break;
                                }
                                track_id = track.id;
                            }
                            _ => break, // no further link: past the end, below
                        }
                    }
                }
                let track_len = state.total_secs();
                if track_len > 0.0 && new_time >= track_len {
                    // Past the end: move on to the next track. Draining first
                    // and letting the seek fail (OutOfRange) dropped a ring's
                    // worth of audio and left the clock that far behind.
                    if ring_capacity - producer.slots() > 0 {
                        state.reset_consumer_counter.store(true, Ordering::Release);
                        await_consumer_drain(state);
                    }
                    skipped = true;
                    break;
                }
                pending.clear();
                // Held-back crossfade audio is pre-seek audio: drop it, and
                // stop fading the previous track's tail into the new position.
                tail_buf.clear();
                crossfade_pos = crossfade_samples;
                if let Some(ref mut r) = resampler { r.reset(); }
                eq.reset();
                effects.reset();
                crossfeed.reset();

                state.reset_consumer_counter.store(true, Ordering::Release);
                await_consumer_drain(state);
                state.clock_preroll.store(0, Ordering::Relaxed);

                // 0.6 replaced the infallible From<f64> with a checked
                // constructor; an unrepresentable target just skips the seek.
                // A chained stream seeks within its current link only (the
                // demuxer has no view of earlier links): a target before the
                // link starts lands on its start.
                let link_time = (new_time - links.start_secs + priming_secs).max(0.0);
                let landed = Time::try_from_secs_f64(link_time).and_then(|time| {
                    format.seek(SeekMode::Coarse, SeekTo::Time { time, track_id: Some(track_id) }).ok()
                });
                let clock_at = match landed {
                    Some(seeked) => {
                        // Codec state (MP3 bit reservoir, AAC/Vorbis overlap)
                        // belongs to the old position; symphonia requires a
                        // reset after a seek.
                        decoder.reset();
                        // A coarse seek lands at or before the target; clock
                        // the position actually reached (FLAC lands up to
                        // ~80 ms early), not the one asked for.
                        track
                            .time_base
                            .and_then(|tb| tb.calc_time(seeked.actual_ts))
                            .map(|t| (links.start_secs + t.as_secs_f64() - priming_secs).max(0.0))
                            .unwrap_or(new_time)
                    }
                    // The seek failed after the ring was already drained: the
                    // decoder carries on from where it was, so the clock must
                    // too — it had been left a ring's depth behind the audio.
                    None => decode_pos_secs,
                };
                state
                    .samples_played
                    .store((clock_at * output_rate as f64) as u64, Ordering::Relaxed);
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
                // A chained stream (concatenated Ogg files, many internet-radio
                // rips) began a new logical stream: the track list changed.
                // Re-select the audio track and rebuild the decoder, then carry
                // on — this used to end the track after the first link. A link
                // at a different sample rate would need a new resampler, so
                // that case still ends the track.
                Err(symphonia::core::errors::Error::ResetRequired) => {
                    if !enter_next_link(format.as_mut(), &mut track, &mut decoder, &mut links, sample_rate, state) {
                        break;
                    }
                    track_id = track.id;
                    continue;
                }
                // A malformed packet: the demuxer can carry on past it. Ending
                // the track here cut it short at the first damaged frame.
                Err(e) if skip_bad_packet(&e, bad_in_a_row) => {
                    bad_in_a_row += 1;
                    report_damage(state);
                    continue;
                }
                Err(_) => break,    // I/O error, or too much damage in a row
            };

            if packet.track_id != track_id { continue; }
            let packet_secs = track.time_base.and_then(|tb| tb.calc_time(packet.pts)).map(|t| t.as_secs_f64());
            if let Some(t) = packet_secs {
                decode_pos_secs = (links.start_secs + t - priming_secs).max(0.0);
            }

            let decoded = match decoder.decode(&packet) {
                Ok(d) => {
                    bad_in_a_row = 0;
                    d
                }
                Err(_) => {
                    // Skipped as before, but no longer silently.
                    report_damage(state);
                    continue;
                }
            };

            // 0.6's generic audio buffer converts and interleaves in one call,
            // replacing the hand-written per-sample-format planar walk.
            raw_buf.clear();
            raw_buf.resize(decoded.samples_interleaved(), 0.0);
            decoded.copy_to_slice_interleaved(&mut raw_buf);
            // Cut the encoder's priming and padding (AAC in MP4).
            if let (Some(g), Some(t)) = (gapless, packet_secs) {
                let first = (t * sample_rate as f64).round() as u64;
                let ch = decoded.spec().channels().count().max(1);
                let (lo, hi) = gapless_keep(g, first, (raw_buf.len() / ch) as u64);
                raw_buf.truncate(hi * ch);
                raw_buf.drain(..lo * ch);
            }
            if raw_buf.is_empty() { continue; }

            // Convert the source layout to interleaved stereo (appends to pending).
            // The layout comes from THIS buffer, not the codec parameters: a
            // stream can change channel count mid-file (concatenated MP3s going
            // mono -> stereo), and reading it with the old count misreads the
            // interleave — half- or double-speed playback.
            downmix_to_stereo(&raw_buf, decoded.spec().channels(), &mut pending);

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

        // End of the track's input. If the next thing to play continues this
        // audio gaplessly (no crossfade, a next track exists), hand the
        // resampler and its unconsumed input across instead of flushing — the
        // next track reuses them at the same source rate, or flushes them first
        // at any other. Otherwise drain it now. A skip or producer exit
        // discards it: that audio is being thrown away.
        if let Some(mut r) = resampler.take() {
            if !skipped && !broke_for_skip {
                let repeats = state.repeat_mode() == crate::state::RepeatMode::One;
                let next_exists = repeats || track_index + 1 < playlist.len();
                if crossfade_samples == 0 && next_exists {
                    carry = Some(ResamplerCarry {
                        resampler: r,
                        src_rate: sample_rate,
                        pending: std::mem::take(&mut pending),
                        rg_linear,
                    });
                } else {
                    flush_resampler(
                        &mut r, &pending, sample_rate, output_rate,
                        &mut resamp_out, &mut flush_planes, producer, state,
                        eq, effects, crossfeed, rg_linear,
                        xfade_in.as_ref(), &mut crossfade_pos, crossfade_samples,
                        if capture_tail { Some(&mut tail_buf) } else { None },
                        &mut chain_bufs,
                    );
                }
            }
        }
        prev_track_skipped = skipped;

        // Hand the held-back tail to the next track to fade under it. A skip
        // or a producer exit discards it: the user asked for this audio to
        // stop, and the ring has been (or is about to be) drained anyway.
        if capture_tail && !tail_buf.is_empty() && !skipped && !broke_for_skip {
            crossfade_tail = Some(tail_buf);
        }

        if broke_for_skip {
            break; // Exit entire function
        }

        // Repeat-one: replay the same track without a crossfade — so the tail
        // that was held back for one is played out plain instead of lost.
        if state.repeat_mode() == crate::state::RepeatMode::One && !skipped {
            if let Some(mut t) = crossfade_tail.take() {
                push_held_tail(producer, state, &mut t);
            }
            continue;
        }

        // Exclusive mode: check if next track needs a different sample rate
        if state.exclusive.load(Ordering::Relaxed) && track_index + 1 < playlist.len() {
            if let Some(next_rate) = crate::audio::probe_sample_rate(&playlist[track_index + 1]) {
                // Compare against the rate a switch would actually reach, not
                // the file's rate: when the device cannot follow, the rebuild
                // lands on the current rate and only costs a drain and a gap.
                if state.exclusive_target_rate(next_rate, output_rate) != output_rate {
                    // The stream is rebuilt at another rate, so nothing can
                    // fade across the rebuild: play the held tail out now.
                    if let Some(mut t) = crossfade_tail.take() {
                        push_held_tail(producer, state, &mut t);
                    }
                    if let Some(c) = carry.take() {
                        flush_carry(c, output_rate, producer, state, eq, effects, crossfeed);
                    }
                    state.next_track_rate.store(next_rate, Ordering::Relaxed);
                    state.rate_change_needed.store(true, Ordering::Relaxed);
                    track_index += 1;
                    state.producer_track_index.store(track_index, Ordering::Relaxed);
                    break; // Exit decode_playlist for stream rebuild
                }
            }
        }

        // Last track: its decode is done but its audio is still playing. Stay
        // until it has, so a seek in those seconds reaches this track instead
        // of lingering until a respawned producer applies it to another one.
        if track_index + 1 >= playlist.len() && !producer_should_unstick(state) {
            if let Some(mut t) = crossfade_tail.take() {
                push_held_tail(producer, state, &mut t);
            }
            if let Some(target) = wait_out_final_tail(producer, state, ring_capacity) {
                track_index = last_track.unwrap_or(track_index);
                initial_seek = Some(target);
                prev_track_skipped = true;
                continue;
            }
        }

        track_index += 1;
    }

    // The playlist ran out with a tail still held for a next track that never
    // came: play it, or the last N seconds of the final track vanish. Not on
    // a quit/skip exit — that audio is being thrown away on purpose.
    if let Some(mut t) = crossfade_tail.take() {
        if !producer_should_unstick(state) {
            push_held_tail(producer, state, &mut t);
        }
    }
    // Same for a resampler carried toward a next track that failed to open.
    if let Some(c) = carry.take() {
        if !producer_should_unstick(state) {
            flush_carry(c, output_rate, producer, state, eq, effects, crossfeed);
        }
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
    fn declared_speaker_positions_decide_the_downmix() {
        use symphonia::core::audio::{Channels, Position};
        let g = std::f32::consts::FRAC_1_SQRT_2;
        // 2.1 (FL FR LFE): the third channel is the LFE, not a centre. The
        // count-based guess took it for FC and mixed the sub into both sides.
        let two_one = Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::LFE1);
        let mut out = Vec::new();
        downmix_to_stereo(&[0.2, 0.4, 1.0], &two_one, &mut out);
        assert_eq!(out, vec![0.2, 0.4]);

        // 6.1 (FL FR FC LFE RC SL SR): the guess alternated the surrounds
        // left/right in order, sending RC left, SL right and SR left.
        let six_one = Channels::Positioned(
            Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::FRONT_CENTER | Position::LFE1
                | Position::REAR_CENTER | Position::SIDE_LEFT | Position::SIDE_RIGHT,
        );
        let mut out = Vec::new();
        downmix_to_stereo(&[0.1, 0.2, 0.3, 1.0, 0.4, 0.5, 0.6], &six_one, &mut out);
        let want_l = 0.1 + g * 0.3 + g * 0.4 + g * 0.5;
        let want_r = 0.2 + g * 0.3 + g * 0.4 + g * 0.6;
        assert!((out[0] - want_l).abs() < 1e-6 && (out[1] - want_r).abs() < 1e-6, "{out:?}");

        // Undeclared layouts keep the count-based fold.
        let mut a = Vec::new();
        let mut b = Vec::new();
        let frame = [0.2, 0.4, 0.6, 1.0, 0.1, 0.3];
        downmix_to_stereo(&frame, &Channels::Discrete(6), &mut a);
        interleaved_to_stereo(&frame, 6, &mut b);
        assert_eq!(a, b);
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
        state.reset_consumer_counter.store(true, Ordering::Release);
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
        run_chain_with(paths, output_rate, rg, 0, false, None)
    }

    /// `run_chain` with the crossfade length (seconds) and resampler quality
    /// exposed, for the tests that exercise track boundaries.
    fn run_chain_with(
        paths: &[PathBuf], output_rate: u32, rg: RgMode, crossfade_secs: u32, hq: bool,
        eq_preset: Option<usize>,
    ) -> Vec<f32> {
        run_chain_state(paths, output_rate, rg, crossfade_secs, hq, eq_preset).0
    }

    /// `run_chain_with`, also returning the player state as the run left it.
    fn run_chain_state(
        paths: &[PathBuf], output_rate: u32, rg: RgMode, crossfade_secs: u32, hq: bool,
        eq_preset: Option<usize>,
    ) -> (Vec<f32>, Arc<PlayerState>) {
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
            if let Some(i) = eq_preset {
                eq_chain.load_preset(&eq_presets[i], output_rate as f32);
            }
            let mut fx_chain = crate::effects::EffectsChain::new(output_rate as f32);
            let fx_presets = crate::effects::builtin_presets();
            let mut cf_filter = crate::crossfeed::CrossfeedFilter::new();
            let cf_presets = crate::crossfeed::builtin_presets();
            decode_playlist(
                &list, 0, &mut producer, &st, output_rate, hq,
                &mut eq_chain, &eq_presets, &mut fx_chain, &fx_presets,
                crossfade_secs, &mut cf_filter, &cf_presets,
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
        (out, state)
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
    fn a_damaged_packet_is_skipped_and_only_a_long_run_ends_the_track() {
        use symphonia::core::errors::Error;
        // symphonia's readers resync past most damage on their own (FLAC
        // drops the frame, MP3 decodes through it), so this is the rule for
        // the errors that do surface.
        assert!(skip_bad_packet(&Error::DecodeError("bad frame"), 0));
        assert!(skip_bad_packet(&Error::DecodeError("bad frame"), MAX_BAD_PACKETS - 1));
        assert!(!skip_bad_packet(&Error::DecodeError("bad frame"), MAX_BAD_PACKETS));
        let io = Error::IoError(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof"));
        assert!(!skip_bad_packet(&io, 0), "an I/O error is the end, not damage");
    }

    #[test]
    fn lossy_tracks_report_no_bit_depth() {
        let (_, aac) = run_chain_state(&[fixture("sine_aac_editlist.m4a")], 44100, RgMode::Off, 0, false, None);
        assert_eq!(aac.bits_per_sample.load(Ordering::Relaxed), 0, "AAC has no bit depth");
        let (_, flac) = run_chain_state(&[fixture("sine_lr.flac")], 44100, RgMode::Off, 0, false, None);
        assert_eq!(flac.bits_per_sample.load(Ordering::Relaxed), 16);
    }

    #[test]
    fn a_nan_sample_engages_the_limiter_instead_of_reading_as_silence() {
        assert_eq!(chain_peak(&[0.1, -0.2]), 0.2);
        assert!(chain_peak(&[0.1, f32::NAN, 0.0]).is_infinite());
        assert!(chain_peak(&[f32::INFINITY]).is_infinite());
        let fx = crate::effects::EffectsChain::new(48000.0);
        assert!(fx.limiter_engaged(chain_peak(&[0.0, f32::NAN])));
    }

    #[test]
    fn the_producer_sizes_the_ring_from_the_ring_not_from_state() {
        // A reopen at another rate that FAILED stored the new rate's size on
        // state but kept the old ring; with a smaller size on state,
        // `ring_capacity - producer.slots()` underflowed (a panic in debug, a
        // wrapped clock and buffer level in release).
        let state = Arc::new(PlayerState::new());
        state.ring_capacity.store(1024, Ordering::Relaxed); // the lie
        state.rg_mode.store(RgMode::Off as u8, Ordering::Relaxed);
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(1 << 16);
        let st = Arc::clone(&state);
        let list = vec![fixture("sine_lr.flac")];
        let handle = thread::spawn(move || {
            let (mut eq, mut fx, mut cf) = (
                crate::eq::EqChain::new(),
                crate::effects::EffectsChain::new(44100.0),
                crate::crossfeed::CrossfeedFilter::new(),
            );
            decode_playlist(
                &list, 0, &mut producer, &st, 44100, false,
                &mut eq, &crate::eq::builtin_presets(), &mut fx, &crate::effects::builtin_presets(),
                0, &mut cf, &crate::crossfeed::builtin_presets(),
            );
        });
        let mut got = 0usize;
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(state.producer_done.load(Ordering::Relaxed) && consumer.slots() == 0) && Instant::now() < deadline {
            while consumer.pop().is_ok() {
                got += 1;
            }
            thread::sleep(Duration::from_millis(1));
        }
        state.quit();
        assert!(handle.join().is_ok(), "the producer panicked");
        assert!(got > 44100, "only {got} samples came out");
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
    fn chain_applies_replaygain_from_id3v2_on_an_mp3_that_also_has_id3v1() {
        // ReplayGain sits in the leading ID3v2 (TXXX); the trailing ID3v1 has
        // none. Reading only symphonia's oldest metadata block (the ID3v1)
        // played the file at full level.
        let out = run_chain(&[fixture("sine_lr_id3v1v2.mp3")], 44100, RgMode::Track);
        let (l, _) = channels(&out);
        let amp = tone_amplitude(&l, 44100.0, 440.0);
        assert!((amp - 0.25).abs() < 0.05, "rg-adjusted level: {amp} (want ~0.25)");
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

    #[test]
    fn crossfade_overlaps_the_tail_instead_of_replaying_it() {
        // A (440 Hz) and B (1 kHz), 2 s each, 1 s crossfade. A true overlap is
        // 3 s long: A alone, then A fading under B, then B alone. The old code
        // pushed A in full and then mixed A's last second under B again — 4 s,
        // with A audible a second time inside B.
        let (a, b) = (tmp_wav("xfade_a"), tmp_wav("xfade_b"));
        let sa = sine(44100, 2.0, 440.0, 0.5);
        let sb = sine(44100, 2.0, 1000.0, 0.5);
        write_wav(&a, 44100, 2, WavFmt::Pcm16, &interleave(&[sa.clone(), sa]));
        write_wav(&b, 44100, 2, WavFmt::Pcm16, &interleave(&[sb.clone(), sb]));
        let out = run_chain_with(&[a, b], 44100, RgMode::Off, 1, false, None);
        let (l, _) = channels(&out);
        assert_eq!(l.len(), 132_300, "3 s of output for 2 s + 2 s with a 1 s overlap");
        // After the overlap only B remains: A must not come back.
        let after = &l[88_200..132_300];
        assert!(tone_amplitude(after, 44100.0, 440.0) < 0.01, "A replayed after the crossfade");
        assert!(tone_amplitude(after, 44100.0, 1000.0) > 0.4, "B missing after the crossfade");
        // Before the overlap only A plays.
        let before = &l[..44_100];
        assert!(tone_amplitude(before, 44100.0, 1000.0) < 0.01, "B leaked before the crossfade");
    }

    #[test]
    fn crossfade_tail_is_played_when_no_track_follows() {
        // With crossfade on, the last N seconds are held back to be mixed into
        // the next track. On the last track nothing follows, and the held tail
        // must still reach the output rather than being dropped.
        let a = tmp_wav("xfade_last");
        let sa = sine(44100, 2.0, 440.0, 0.5);
        write_wav(&a, 44100, 2, WavFmt::Pcm16, &interleave(&[sa.clone(), sa]));
        let out = run_chain_with(&[a], 44100, RgMode::Off, 1, false, None);
        assert_eq!(out.len() / 2, 88_200, "the whole track, tail included");
    }

    /// Split one continuous signal into two files at `split` frames, run both
    /// and the unsplit original through the chain, and return (split, whole).
    fn split_vs_whole(
        tag: &str, full: &[f32], split: usize, src_rate: u32, out_rate: u32, hq: bool,
        eq_preset: Option<usize>,
    ) -> (Vec<f32>, Vec<f32>) {
        let (h1, h2) = full.split_at(split);
        let (a, b, w) = (
            tmp_wav(&format!("{tag}_a")), tmp_wav(&format!("{tag}_b")), tmp_wav(&format!("{tag}_w")),
        );
        write_wav(&a, src_rate, 2, WavFmt::F32, &interleave(&[h1.to_vec(), h1.to_vec()]));
        write_wav(&b, src_rate, 2, WavFmt::F32, &interleave(&[h2.to_vec(), h2.to_vec()]));
        write_wav(&w, src_rate, 2, WavFmt::F32, &interleave(&[full.to_vec(), full.to_vec()]));
        let split_out = run_chain_with(&[a, b], out_rate, RgMode::Off, 0, hq, eq_preset);
        let whole_out = run_chain_with(&[w], out_rate, RgMode::Off, 0, hq, eq_preset);
        (split_out, whole_out)
    }

    fn max_abs_diff(x: &[f32], y: &[f32]) -> f32 {
        x.iter().zip(y).fold(0.0f32, |m, (a, b)| m.max((a - b).abs()))
    }

    fn assert_gapless_resampled(hq: bool) {
        // A continuous sine cut mid-cycle, 44.1 kHz source on a 48 kHz output.
        // Gapless means the join is inaudible: the split playback must match
        // the unsplit file sample for sample. A fresh resampler per track put
        // its start-up delay (tens to a hundred-odd frames of near-silence) at
        // every boundary.
        let full = sine(44100, 2.0, 441.0, 0.5);
        let (split, whole) =
            split_vs_whole(&format!("gapless_{hq}"), &full, 44100 + 123, 44100, 48000, hq, None);
        assert_eq!(split.len(), whole.len(), "hq={hq}: split playback is a different length");
        let err = max_abs_diff(&split, &whole);
        assert!(err < 1e-4, "hq={hq}: split differs from whole by {err} at the join");
    }

    #[test]
    fn gapless_join_survives_resampling() {
        assert_gapless_resampled(false);
    }

    #[test]
    fn gapless_join_survives_hq_resampling() {
        assert_gapless_resampled(true);
    }

    #[test]
    fn eq_state_carries_across_a_gapless_boundary() {
        // Same rate (no resampler), bass-boost EQ on a 60 Hz tone. Resetting
        // the biquads at the track change restarted them from zero state — a
        // step in the output exactly at the join.
        let full = sine(44100, 2.0, 60.0, 0.4);
        let (split, whole) = split_vs_whole("eq_join", &full, 44100 + 300, 44100, 44100, false, Some(1));
        assert_eq!(split.len(), whole.len());
        let err = max_abs_diff(&split, &whole);
        assert!(err < 1e-4, "EQ output steps by {err} at the track change");
    }

    #[test]
    fn resampler_tail_is_flushed_when_no_input_is_left_over() {
        // A track whose length is an exact multiple of the 1024-frame chunk
        // leaves nothing in `pending` at EOF. The flush was skipped then, so
        // the resampler's delay line — the real last few ms — was dropped.
        // One frame longer must yield roughly one output frame more, not one
        // plus the whole delay line.
        let n = 43 * 1024;
        let run = |frames: usize, tag: &str| {
            let p = tmp_wav(tag);
            let s = sine(44100, frames as f32 / 44100.0, 441.0, 0.5);
            let s = s[..frames].to_vec();
            write_wav(&p, 44100, 2, WavFmt::F32, &interleave(&[s.clone(), s]));
            run_chain_with(&[p], 48000, RgMode::Off, 0, false, None).len() / 2
        };
        let exact = run(n, "flush_exact");
        let plus_one = run(n + 1, "flush_plus1");
        let d = plus_one as i64 - exact as i64;
        assert!((0..=2).contains(&d), "one extra input frame changed the output by {d} frames");
    }

    /// A harness closer to the real callback than `run_chain`: it consumes at a
    /// fixed multiple of real time (so the producer runs a full ring ahead, as
    /// in playback), advances `samples_played`, honours drain requests, and
    /// fires `seeks` — (seconds of audio consumed, relative seek) — on cue.
    fn run_chain_paced(paths: &[PathBuf], rate: u32, seeks: &[(f64, i64)]) -> Vec<f32> {
        const SPEEDUP: f64 = 10.0;
        let state = Arc::new(PlayerState::new());
        let cap = crate::state::ring_capacity_for(rate);
        state.ring_capacity.store(cap, Ordering::Relaxed);
        state.output_rate.store(rate as u64, Ordering::Relaxed);
        state.rg_mode.store(RgMode::Off as u8, Ordering::Relaxed);
        let (mut producer, mut consumer) = rtrb::RingBuffer::<f32>::new(cap);
        let st = Arc::clone(&state);
        let list = paths.to_vec();
        let handle = thread::spawn(move || {
            let mut eq_chain = crate::eq::EqChain::new();
            let eq_presets = crate::eq::builtin_presets();
            let mut fx_chain = crate::effects::EffectsChain::new(rate as f32);
            let fx_presets = crate::effects::builtin_presets();
            let mut cf_filter = crate::crossfeed::CrossfeedFilter::new();
            let cf_presets = crate::crossfeed::builtin_presets();
            decode_playlist(
                &list, 0, &mut producer, &st, rate, false,
                &mut eq_chain, &eq_presets, &mut fx_chain, &fx_presets,
                0, &mut cf_filter, &cf_presets,
            );
        });
        let mut out = Vec::new();
        let mut next_seek = 0;
        let start = Instant::now();
        let deadline = start + Duration::from_secs(30);
        loop {
            if state.reset_consumer_counter.swap(false, Ordering::AcqRel) {
                while consumer.pop().is_ok() {}
            }
            let played_frames = out.len() / 2;
            if next_seek < seeks.len() && played_frames as f64 >= seeks[next_seek].0 * rate as f64 {
                state.seek(seeks[next_seek].1);
                next_seek += 1;
            }
            let due = (start.elapsed().as_secs_f64() * rate as f64 * SPEEDUP) as usize;
            let mut frames = due.saturating_sub(played_frames);
            state.buffer_level.store(consumer.slots(), Ordering::Relaxed);
            while frames > 0 {
                let (Ok(l), Ok(r)) = (consumer.pop(), consumer.pop()) else { break };
                out.push(l);
                out.push(r);
                state.samples_played.fetch_add(1, Ordering::Relaxed);
                frames -= 1;
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
        out
    }

    fn secs(out: &[f32], rate: u32) -> f64 {
        out.len() as f64 / 2.0 / rate as f64
    }

    #[test]
    fn a_seek_into_a_later_link_of_a_chained_ogg_lands_there() {
        // chained.ogg: 1 s of 440 Hz, then a second link of 1 kHz. The
        // demuxer seeks within ONE link, so a target in the second link (1.2 s)
        // was handed to the first, which cannot reach it.
        let out = run_chain_paced(&[fixture("chained.ogg")], 44100, &[(0.2, 1)]);
        let t = secs(&out, 44100);
        assert!((0.8..1.3).contains(&t), "expected ~0.2 s + the last 0.8 s, got {t:.2} s");
        let (l, _) = channels(&out);
        let after = &l[l.len() - 22_050..];
        assert!(tone_amplitude(after, 44100.0, 1000.0) > 0.3, "not in the second link after the seek");
        assert!(tone_amplitude(after, 44100.0, 440.0) < 0.05, "still in the first link");
    }

    #[test]
    fn a_seek_back_into_an_earlier_link_of_a_chained_ogg_lands_there() {
        // Twelve links (440 Hz / 1 kHz alternating, 1 s each), far longer than
        // the ring, so the producer is still in a late link when the seek comes:
        // the demuxer cannot go back across links, the file is reopened.
        let one = std::fs::read(fixture("chained.ogg")).unwrap();
        let path = std::env::temp_dir().join(format!("keet_chain6_{}.ogg", std::process::id()));
        std::fs::write(&path, one.repeat(6)).unwrap();
        // At 5.0 s (the producer ~4 s ahead, in link 10) jump back 3 s to
        // 2.0 s: link 3, 440 Hz.
        let out = run_chain_paced(std::slice::from_ref(&path), 44100, &[(5.0, -3)]);
        let _ = std::fs::remove_file(&path);
        // The length is what tells the two apart: from ~2 s the rest of the
        // file (~10 s) still plays. Unfixed, the target fell before the link
        // playing and the seek only reached that link's start (~3 s left).
        // Where exactly the seek lands is left to the coarse demuxer seek, and
        // when the harness applies the drain varies under load.
        let t = secs(&out, 44100);
        assert!((14.0..16.0).contains(&t), "expected 5 s + the last ~10 s, got {t:.2} s");
    }

    #[test]
    fn seeking_past_the_end_moves_on_to_the_next_track() {
        // 1 s into an 8 s track, seek +10 s: that lands past the end, so the
        // player should move on to B. It used to drain the ring, have the seek
        // fail (OutOfRange), and carry on decoding A from wherever the producer
        // was — dropping ~4 s and leaving the clock ~4 s wrong.
        let (a, b) = (tmp_wav("seekend_a"), tmp_wav("seekend_b"));
        let sa = sine(44100, 8.0, 440.0, 0.5);
        let sb = sine(44100, 2.0, 1000.0, 0.5);
        write_wav(&a, 44100, 2, WavFmt::Pcm16, &interleave(&[sa.clone(), sa]));
        write_wav(&b, 44100, 2, WavFmt::Pcm16, &interleave(&[sb.clone(), sb]));
        let out = run_chain_paced(&[a, b], 44100, &[(1.0, 10)]);
        let t = secs(&out, 44100);
        // Upper bound is the regression check (unfixed: ~7.2 s). The lower
        // bound is loose on purpose: under a loaded parallel test run the
        // harness thread can miss the 250 ms drain window, and the late drain
        // then eats some of B — a harness artifact the real-time callback
        // does not share.
        assert!((2.0..3.6).contains(&t), "expected ~1 s of A then 2 s of B, got {t:.2} s");
        let (l, _) = channels(&out);
        // Only the final 0.3 s: B's end is always the last audio out, while a
        // late (starved-harness) drain can shorten B enough that a wider
        // window reaches back into A's first second.
        let tail = &l[l.len() - 13_230..];
        assert!(tone_amplitude(tail, 44100.0, 440.0) < 0.01, "A still playing after the seek");
    }

    #[test]
    fn seeking_in_the_last_seconds_seeks_the_track_you_hear() {
        // The producer finishes decoding A seconds before A finishes playing.
        // A seek pressed in that window belongs to A (A is what is on screen
        // and in your ears); it used to wait for B and be applied to B's clock.
        // Here: at 2.0 s of a 3 s track, seek back 1 s -> A again from ~1 s.
        let (a, b) = (tmp_wav("seektail_a"), tmp_wav("seektail_b"));
        let sa = sine(44100, 3.0, 440.0, 0.5);
        let sb = sine(44100, 3.0, 1000.0, 0.5);
        write_wav(&a, 44100, 2, WavFmt::Pcm16, &interleave(&[sa.clone(), sa]));
        write_wav(&b, 44100, 2, WavFmt::Pcm16, &interleave(&[sb.clone(), sb]));
        let out = run_chain_paced(&[a, b], 44100, &[(2.0, -1)]);
        let t = secs(&out, 44100);
        // Unfixed: ~5.6 s (the seek was spent on B). Lower bound loose for the
        // same harness-starvation reason as the test above.
        assert!((6.2..7.3).contains(&t), "expected 2 s + A from 1 s (2 s) + B (3 s) = 7 s, got {t:.2} s");
        let (l, _) = channels(&out);
        let replay = &l[(2.2 * 44100.0) as usize..(3.8 * 44100.0) as usize];
        assert!(tone_amplitude(replay, 44100.0, 440.0) > 0.4, "A was not replayed after the seek");
    }

    #[test]
    fn dsp_boosts_are_limited_not_left_to_clip() {
        // Bass-boost EQ on a hot 60 Hz tone pushes peaks past full scale. The
        // only limiter lived inside the effects chain (so it ran only with an
        // effect on, and before ReplayGain/crossfeed); everything else reached
        // the callback's hard clamp — i.e. clipped. The chain's own output must
        // stay within 0 dBFS.
        let a = tmp_wav("hot_eq");
        let t = sine(44100, 1.5, 60.0, 0.95);
        write_wav(&a, 44100, 2, WavFmt::F32, &interleave(&[t.clone(), t]));
        let out = run_chain_with(&[a], 44100, RgMode::Off, 0, false, Some(1));
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 1.0 + 1e-6, "chain output peaks at {peak}, past 0 dBFS");
    }

    #[test]
    fn chained_ogg_plays_every_link() {
        // Two Ogg Vorbis streams concatenated in one file. At the join the
        // demuxer returns ResetRequired (new logical stream); that used to be
        // treated as end of track, so only the first link ever played.
        let out = run_chain(&[fixture("chained.ogg")], 44100, RgMode::Off);
        let (l, _) = channels(&out);
        let t = l.len() as f64 / 44100.0;
        assert!(t > 1.8, "only {t:.2} s played of a 2 s chained file");
        let second = &l[l.len() - 30_000..];
        assert!(tone_amplitude(second, 44100.0, 1000.0) > 0.3, "second link's 1 kHz missing");
    }

    #[test]
    fn aac_plays_exactly_its_real_length() {
        // AAC decodes to priming + audio + padding: 47104 frames for a 1 s
        // Apple file (2112 + 44100 + 892), 46080 for ffmpeg's (1024 + 44100 +
        // padding). Played untrimmed, each track started ~25-50 ms late and
        // ended in silence — a gap between gapless tracks.
        for f in ["sine_aac_itunsmpb.m4a", "sine_aac_editlist.m4a"] {
            let (out, state) = run_chain_state(&[fixture(f)], 44100, RgMode::Off, 0, false, None);
            assert_eq!(out.len() / 2, 44100, "{f}: frames played");
            assert_eq!(state.total_samples.load(Ordering::Relaxed), 44100, "{f}: reported length");
            // The tone starts at once: no priming silence at the front.
            let head = out[..2 * 2048].iter().step_by(2).fold(0.0f32, |m, s| m.max(s.abs()));
            assert!(head > 0.3, "{f}: silent start (peak {head})");
        }
    }

    #[test]
    fn chained_ogg_length_covers_every_link() {
        // The container reports only the FIRST link's length, so a 2 s file
        // showed 1 s: the progress bar ran past its end and a seek beyond
        // 1 s counted as "past the end" and skipped the rest.
        let (_, state) = run_chain_state(&[fixture("chained.ogg")], 44100, RgMode::Off, 0, false, None);
        let secs = state.total_secs();
        assert!(secs > 1.9, "length after both links: {secs:.2} s");
    }

    // ---- bit-perfect ------------------------------------------------------
    //
    // With every DSP stage off, no resampling (output rate = file rate) and
    // volume at 100%, Keet must hand the device the file's samples UNCHANGED:
    // each f32 equal to `integer / 2^(bits-1)`, which a DAC's float->int
    // conversion maps straight back to the original integer. These tests pin
    // Keet's side of the bit-perfect claim, file on disk to the samples the
    // audio callback writes; CoreAudio's own conversion is outside them.

    /// A WAV written from exact integers (the float writer above rounds).
    fn write_wav_int(path: &PathBuf, rate: u32, channels: u16, bits: u16, samples: &[i32]) {
        let bytes_per = (bits / 8) as usize;
        let mut data = Vec::with_capacity(samples.len() * bytes_per);
        for &v in samples {
            data.extend_from_slice(&v.to_le_bytes()[..bytes_per]);
        }
        let block_align = channels as u32 * bytes_per as u32;
        let mut out = Vec::with_capacity(data.len() + 44);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * block_align).to_le_bytes());
        out.extend_from_slice(&(block_align as u16).to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        std::fs::write(path, out).expect("write int wav");
    }

    /// Full-range integers at `bits`: both extremes, zero, +-1 and their
    /// neighbours first, then a deterministic spread over the whole range.
    fn full_range_ints(n: usize, bits: u32) -> Vec<i32> {
        let max = (1i64 << (bits - 1)) - 1;
        let min = -(1i64 << (bits - 1));
        let mut v: Vec<i32> = [min, max, 0, 1, -1, min + 1, max - 1]
            .iter().map(|&x| x as i32).collect();
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        while v.len() < n {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let span = (max - min + 1) as u64;
            v.push((min + ((x >> 11) % span) as i64) as i32);
        }
        v
    }

    /// Push samples through the REAL output callback (`OutputRenderer`) at
    /// 100% volume on a stereo device, rendering to sample type T.
    fn through_callback<T: crate::audio::OutputSample>(samples: &[f32]) -> Vec<T> {
        let st = Arc::new(PlayerState::new());
        st.volume.store(100, Ordering::Relaxed);
        let (mut p, c) = rtrb::RingBuffer::<f32>::new(samples.len().max(2));
        let (vp, _vc) = rtrb::RingBuffer::<f32>::new(samples.len().max(2));
        p.push_entire_slice(samples).unwrap();
        let mut r = crate::audio::OutputRenderer::new(c, vp, st, 2);
        let mut out = vec![T::EQUILIBRIUM; samples.len()];
        r.render(&mut out);
        out
    }

    fn assert_output_stage_is_identity(out: &[f32]) {
        let st = PlayerState::new();
        st.volume.store(100, Ordering::Relaxed);
        let gain = st.volume_gain();
        assert_eq!(gain.to_bits(), 1.0f32.to_bits(), "100% volume must be a gain of exactly 1.0");
        for &o in out {
            let played = crate::audio::output_sample(o, gain);
            assert_eq!(played.to_bits(), o.to_bits(), "callback changed {o} at 100% volume");
        }
    }

    #[test]
    fn bit_perfect_pcm_reaches_the_callback_unchanged() {
        for (bits, rate) in [(16u16, 44_100u32), (24, 96_000)] {
            let scale = (1u64 << (bits - 1)) as f64;
            for channels in [1u16, 2] {
                let frames = 12_000;
                let src = full_range_ints(frames * channels as usize, bits as u32);
                let path = tmp_wav(&format!("bitperfect_{bits}_{channels}"));
                write_wav_int(&path, rate, channels, bits, &src);
                // Output rate = file rate: no resampler; RG off; DSP at defaults (off).
                let out = run_chain(&[path], rate, RgMode::Off);
                assert_eq!(out.len(), frames * 2, "{bits}-bit/{channels}ch: sample count changed");
                for (frame, pair) in out.as_chunks::<2>().0.iter().enumerate() {
                    for (ch, &o) in pair.iter().enumerate() {
                        // Mono is duplicated into both channels.
                        let v = src[frame * channels as usize + if channels == 1 { 0 } else { ch }];
                        let back = o as f64 * scale;
                        assert!(
                            back == v as f64,
                            "{bits}-bit/{channels}ch frame {frame} ch {ch}: source {v}, delivered {o} (= {back} at {bits} bits)"
                        );
                    }
                    if channels == 1 {
                        // A mono device folds the frame back: must be exact too.
                        assert_eq!(crate::audio::mono_fold(pair[0], pair[1]).to_bits(), pair[0].to_bits());
                    }
                }
                assert_output_stage_is_identity(&out);

                // Through the real callback: the float output is untouched, and
                // a 32-bit integer output (an ALSA hw: / WASAPI exclusive
                // device) receives the source integers exactly, shifted up.
                let delivered: Vec<f32> = through_callback(&out);
                assert!(delivered.iter().zip(&out).all(|(a, b)| a.to_bits() == b.to_bits()),
                    "{bits}-bit/{channels}ch: the callback changed float samples");
                let as_i32: Vec<i32> = through_callback(&out);
                for (i, &o) in out.iter().enumerate() {
                    let v = (o as f64 * scale) as i32;
                    assert_eq!(as_i32[i], v << (32 - bits as u32),
                        "{bits}-bit/{channels}ch sample {i}: integer output is not the source value");
                }
            }
        }
    }

    #[test]
    fn bit_perfect_24_bit_flac_matches_a_direct_decode() {
        // The hi-res format people actually play: the chain's output must be
        // exactly the decoder's output, and sit on the 24-bit integer grid.
        let path = fixture("hires_24_96.flac");
        let out = run_chain(std::slice::from_ref(&path), 96_000, RgMode::Off);

        let file = File::open(&path).unwrap();
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        hint.with_extension("flac");
        let mut format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .unwrap();
        let track = format.default_track(TrackType::Audio).unwrap().clone();
        let params = track.codec_params.as_ref().and_then(|c| c.audio()).unwrap().clone();
        assert_eq!(params.bits_per_sample, Some(24));
        let mut dec = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .unwrap();
        let mut direct: Vec<f32> = Vec::new();
        while let Ok(Some(pkt)) = format.next_packet() {
            let buf = dec.decode(&pkt).unwrap();
            let mut tmp = vec![0.0f32; buf.samples_interleaved()];
            buf.copy_to_slice_interleaved(&mut tmp);
            direct.extend_from_slice(&tmp);
        }
        assert_eq!(out.len(), direct.len(), "sample count changed");
        for (i, (&o, &d)) in out.iter().zip(&direct).enumerate() {
            assert_eq!(o.to_bits(), d.to_bits(), "sample {i}: decoded {d}, delivered {o}");
            let back = o as f64 * 8_388_608.0;
            assert!(back == back.round(), "sample {i} ({o}) is off the 24-bit grid");
        }
        assert_output_stage_is_identity(&out);
    }

    #[test]
    fn the_bit_perfect_checks_notice_a_change() {
        // Negative control: the identity check must fail for any real change.
        let x = 12_345.0f32 / 8_388_608.0;
        assert_ne!(crate::audio::output_sample(x, 0.99).to_bits(), x.to_bits(), "99% volume");
        assert_ne!(crate::audio::output_sample(x, 1.0 + f32::EPSILON).to_bits(), x.to_bits());
    }
}
