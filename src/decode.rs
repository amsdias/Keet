use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{Limit, MetadataOptions};
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use rubato::{Async, FixedAsync, Indexing, SincInterpolationType, SincInterpolationParameters, WindowFunction, Resampler};
use audioadapter_buffers::direct::SequentialSliceOfVecs;
use audioadapter_buffers::owned::InterleavedOwned;

use rtrb::Producer;

use crate::state::PlayerState;
use crate::state::RgMode;

fn interleave_f32_planes_into(buf: &symphonia::core::audio::AudioBuffer<f32>, out: &mut Vec<f32>) {
    let spec = buf.planes();
    let p = spec.planes();
    out.reserve(buf.frames() * p.len());
    for f in 0..buf.frames() {
        for ch in p { out.push(ch[f]); }
    }
}

fn convert_samples_into(buf: &AudioBufferRef, out: &mut Vec<f32>) {
    match buf {
        AudioBufferRef::F32(b) => interleave_f32_planes_into(b, out),
        AudioBufferRef::S16(b) => {
            let spec = b.planes();
            let p = spec.planes();
            out.reserve(b.frames() * p.len());
            for f in 0..b.frames() {
                for ch in p { out.push(ch[f] as f32 / 32768.0); }
            }
        }
        AudioBufferRef::S32(b) => {
            let spec = b.planes();
            let p = spec.planes();
            out.reserve(b.frames() * p.len());
            for f in 0..b.frames() {
                for ch in p { out.push(ch[f] as f32 / 2147483648.0); }
            }
        }
        AudioBufferRef::S24(b) => {
            let spec = b.planes();
            let p = spec.planes();
            out.reserve(b.frames() * p.len());
            for f in 0..b.frames() {
                // i24 carries its value in an i32; full scale is 2^23.
                for ch in p { out.push(ch[f].inner() as f32 / 8_388_608.0); }
            }
        }
        // Catchall for U8/U16/U24/U32/F64: convert via symphonia's make_equivalent.
        _ => interleave_f32_planes_into(&buf.make_equivalent::<f32>(), out),
    }
}

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
            for frame in input.chunks_exact(4) {
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

/// Wait (bounded) until the audio callback has consumed a drain request set via
/// `reset_consumer_counter`. The producer must not push new samples while the
/// flag is pending: the callback drains *everything* in the ring when it sees
/// the flag, so samples pushed in between would be silently discarded —
/// clipping the start of post-seek/skip audio. Bounded so a dead stream
/// (device error) can't hang the producer; 250 ms is many callback periods.
pub(crate) fn await_consumer_drain(state: &PlayerState) {
    for _ in 0..50 {
        if !state.reset_consumer_counter.load(Ordering::Relaxed) || state.should_quit() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Push the whole slice into the ring, waiting briefly when full.
/// `push_entire_slice` alone is all-or-nothing — on a full ring it pushes
/// NOTHING and the chunk would be silently dropped (the EOF flush runs without
/// the buffer-space throttle, so it can actually hit that).
fn push_all(producer: &mut Producer<f32>, state: &PlayerState, mut data: &[f32]) {
    while !data.is_empty() && !state.should_quit() {
        let n = producer.slots().min(data.len());
        if n > 0 && producer.push_entire_slice(&data[..n]).is_ok() {
            data = &data[n..];
            continue;
        }
        thread::sleep(Duration::from_millis(5));
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
    for tag in tags {
        if let symphonia::core::meta::Value::String(ref s) = tag.value {
            let key_lower = tag.key.to_lowercase();
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
        let meta_opts = MetadataOptions {
            limit_visual_bytes: Limit::Maximum(0),
            limit_metadata_bytes: Limit::Default,
        };
        let mut probed = match symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &meta_opts)
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

        let track = match probed.format.tracks().iter()
            .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        {
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
        let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
        // Source layout, kept for the UI. Everything downstream of decode —
        // resampler, DSP chain, ring buffer, audio callback — runs on stereo;
        // interleaved_to_stereo converts right after each packet is decoded.
        let src_channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);
        let channels = 2usize;
        let bits_per_sample = track.codec_params.bits_per_sample.unwrap_or(16);
        let total = track.codec_params.n_frames.unwrap_or(0);

        let mut decoder = match symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
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
        // Extract from both metadata sources (same pattern as metadata.rs)
        let mut rg_tags = RgTags {
            track_gain: None, track_peak: None,
            album_gain: None, album_peak: None,
        };
        if let Some(rev) = probed.format.metadata().current() {
            extract_rg_from_tags(rev.tags(), &mut rg_tags);
        }
        if let Some(meta) = probed.metadata.get() {
            if let Some(rev) = meta.current() {
                extract_rg_from_tags(rev.tags(), &mut rg_tags);
            }
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
                    f_cutoff: 0.95,
                    interpolation: SincInterpolationType::Cubic,
                    oversampling_factor: 128,
                    window: WindowFunction::BlackmanHarris2,
                }
            } else {
                SincInterpolationParameters {
                    sinc_len: 64,
                    f_cutoff: 0.95,
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
                if state.should_quit() || state.skip_prev.load(Ordering::Relaxed)
                    || state.jump_to_track.load(Ordering::Relaxed) >= 0 {
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
        let mut eq_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut fx_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut rg_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut xfeed_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut bal_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        // Scratch for crossfade-mixed output. Only touched while a tail is being
        // mixed in; steady-state playback reads bal_output directly and skips this.
        let mut final_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);

        // --- Packet decode loop ---
        loop {
            if state.should_quit() {
                broke_for_skip = true;
                break;
            }
            // Check skip-prev and jump — these require producer to exit entirely
            if state.skip_prev.load(Ordering::Relaxed) || state.jump_to_track.load(Ordering::Relaxed) >= 0 {
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

                if probed.format.seek(SeekMode::Coarse, SeekTo::Time {
                    time: Time::from(new_time),
                    track_id: Some(track_id)
                }).is_ok() {
                    state.samples_played.store((new_time * output_rate as f64) as u64, Ordering::Relaxed);
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

            // Check for live EQ preset change
            if state.take_eq_changed() {
                let idx = state.eq_index();
                if idx < eq_presets.len() {
                    eq.load_preset(&eq_presets[idx], output_rate as f32);
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

            // Decode next packet
            let packet = match probed.format.next_packet() {
                Ok(p) => p,
                Err(_) => break, // EOF
            };

            if packet.track_id() != track_id { continue; }

            let decoded = match decoder.decode(&packet) {
                Ok(d) => d,
                Err(_) => continue,
            };

            raw_buf.clear();
            convert_samples_into(&decoded, &mut raw_buf);
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
                        if let Ok(resampled) = resampler.process(&adapter_in, 0, None) {
                            interleaved_out.extend(resampled.take_data());
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
            let output = &decoded_buf[..];

            // EQ processing
            let eq_output = if eq.is_active() {
                eq_buf.clear();
                eq_buf.extend_from_slice(output);
                eq.process_stereo(&mut eq_buf);
                &eq_buf[..]
            } else {
                output
            };

            // Effects processing
            let fx_output = if effects.is_active() {
                fx_buf.clear();
                fx_buf.extend_from_slice(eq_output);
                effects.process_stereo(&mut fx_buf);
                &fx_buf[..]
            } else {
                eq_output
            };

            // ReplayGain
            let rg_output = if rg_linear != 1.0 {
                rg_buf.clear();
                rg_buf.extend_from_slice(fx_output);
                for sample in rg_buf.iter_mut() {
                    *sample *= rg_linear;
                }
                &rg_buf[..]
            } else {
                fx_output
            };

            // Crossfeed processing (after RG, before balance)
            let cf_output = if crossfeed.is_active() {
                xfeed_buf.clear();
                xfeed_buf.extend_from_slice(rg_output);
                crossfeed.process_stereo(&mut xfeed_buf);
                &xfeed_buf[..]
            } else {
                rg_output
            };

            // Balance processing (after crossfeed, before crossfade)
            let balance = state.balance_value();
            let bal_output = if balance != 0 {
                bal_buf.clear();
                bal_buf.extend_from_slice(cf_output);
                let left_gain = ((100 - balance) as f32 / 100.0).clamp(0.0, 1.0);
                let right_gain = ((100 + balance) as f32 / 100.0).clamp(0.0, 1.0);
                for i in (0..bal_buf.len()).step_by(2) {
                    bal_buf[i] *= left_gain;
                    if i + 1 < bal_buf.len() {
                        bal_buf[i + 1] *= right_gain;
                    }
                }
                &bal_buf[..]
            } else {
                cf_output
            };

            // Crossfade mixing with previous track's tail.
            // Only populate final_buf when we actually need to mutate samples —
            // steady-state playback skips this and reads bal_output directly.
            let mut using_final_buf = false;
            if let Some(ref tail) = xfade_in {
                if crossfade_pos < crossfade_samples && crossfade_samples > 0 {
                    final_buf.clear();
                    final_buf.extend_from_slice(bal_output);
                    for sample in final_buf.iter_mut() {
                        if crossfade_pos < crossfade_samples {
                            let pos_f = crossfade_pos as f32 / crossfade_samples as f32;
                            let fade_in = (pos_f * std::f32::consts::FRAC_PI_2).sin();
                            let fade_out = ((1.0 - pos_f) * std::f32::consts::FRAC_PI_2).sin();

                            let tail_sample = if crossfade_pos < tail.len() { tail[crossfade_pos] } else { 0.0 };
                            *sample = *sample * fade_in + tail_sample * fade_out;
                            crossfade_pos += 1;
                        }
                    }
                    using_final_buf = true;
                }
            }

            // Clipping detection only — flag for the UI when peak * volume would
            // exceed 0dBFS at the DAC. Hard prevention happens in the audio
            // callback (clamp post-gain), since volume can change between this
            // scan and consumption (~ring-buffer-depth latency).
            if !bal_output.is_empty() {
                let vol = state.volume.load(Ordering::Relaxed) as f32 / 100.0;
                let scan: &[f32] = if using_final_buf { &final_buf } else { bal_output };
                let peak = scan.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                if peak * vol > 1.0 {
                    state.clipping.store(true, Ordering::Relaxed);
                }
            }

            // Push to ring buffer
            let out: &[f32] = if using_final_buf { &final_buf } else { bal_output };
            if !out.is_empty() {
                push_all(producer, state, out);

                // Capture tail for crossfade into next track
                if capture_tail {
                    tail_buf.extend(out.iter().copied());
                    if tail_buf.len() > crossfade_samples {
                        let excess = tail_buf.len() - crossfade_samples;
                        tail_buf.drain(..excess);
                    }
                }
            }
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
                    let mut out_buf = InterleavedOwned::<f32>::new(0.0f32, channels, out_frames);
                    match resampler.process_into_buffer(&adapter_in, &mut out_buf, Some(&indexing)) {
                        Ok((_, nbr_out)) => {
                            if nbr_out == 0 {
                                break;
                            }
                            let data = out_buf.take_data();
                            let take = frames_wanted.min(nbr_out);
                            push_all(producer, state, &data[..take * channels]);
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
}
