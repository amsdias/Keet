use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use rubato::{Async, FixedAsync, SincInterpolationType, SincInterpolationParameters, WindowFunction, Resampler};
use audioadapter_buffers::direct::SequentialSliceOfVecs;

use rtrb::Producer;

use crate::state::{PlayerState, RING_BUFFER_SIZE};
use crate::state::RgMode;

fn interleave_f32_planes(buf: &symphonia::core::audio::AudioBuffer<f32>) -> Vec<f32> {
    let spec = buf.planes();
    let p = spec.planes();
    let mut out = Vec::with_capacity(buf.frames() * p.len());
    for f in 0..buf.frames() {
        for ch in p { out.push(ch[f]); }
    }
    out
}

fn convert_samples(buf: &AudioBufferRef) -> Vec<f32> {
    match buf {
        AudioBufferRef::F32(b) => interleave_f32_planes(b),
        AudioBufferRef::S16(b) => {
            let spec = b.planes();
            let p = spec.planes();
            let mut out = Vec::with_capacity(b.frames() * p.len());
            for f in 0..b.frames() {
                for ch in p { out.push(ch[f] as f32 / 32768.0); }
            }
            out
        }
        AudioBufferRef::S32(b) => {
            let spec = b.planes();
            let p = spec.planes();
            let mut out = Vec::with_capacity(b.frames() * p.len());
            for f in 0..b.frames() {
                for ch in p { out.push(ch[f] as f32 / 2147483648.0); }
            }
            out
        }
        // Catchall for S24/U8/U16/U24/U32/F64: convert via symphonia's make_equivalent.
        // Previously this returned vec![] which silently starved the ring buffer → clicks/silence.
        _ => interleave_f32_planes(&buf.make_equivalent::<f32>()),
    }
}

fn deinterleave(samples: &[f32], ch: usize) -> Vec<Vec<f32>> {
    let frames = samples.len() / ch;
    let mut out = vec![Vec::with_capacity(frames); ch];
    for (i, &s) in samples.iter().enumerate() {
        out[i % ch].push(s);
    }
    out
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
            (tags.track_gain, tags.track_peak)
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

    while track_index < playlist.len() {
        if state.should_quit() || state.take_skip_prev() || state.take_jump().is_some() {
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

        let mut probed = match symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
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
        let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);
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

        let mut broke_for_skip = false;
        let mut skipped = false;

        // Wait for buffer to drain so display update matches audio playback
        if track_index != start_index {
            let drain_threshold = output_rate as usize; // ~0.5s stereo
            loop {
                let buffered = RING_BUFFER_SIZE - producer.slots();
                if buffered <= drain_threshold { break; }
                if state.should_quit() || state.skip_prev.load(Ordering::Relaxed)
                    || state.jump_to_track.load(Ordering::Relaxed) >= 0 {
                    broke_for_skip = true;
                    break;
                }
                if state.take_skip_next() {
                    state.discard_samples.store(buffered as u64, Ordering::Relaxed);
                    state.reset_consumer_counter.store(true, Ordering::Relaxed);
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
        state.channels.store(channels, Ordering::Relaxed);
        state.bits_per_sample.store(bits_per_sample as usize, Ordering::Relaxed);
        state.track_info_ready.store(true, Ordering::Relaxed);

        // Signal track transition (skip for first track — main thread already knows)
        if track_index != start_index {
            state.signal_next_track(track_index);
        }

        // Reset filter states for new track
        eq.reset();
        effects.reset();
        crossfeed.reset();

        // --- Crossfade setup for this track ---
        let xfade_in = crossfade_tail.take();
        let mut crossfade_pos: usize = 0;
        let capture_tail = crossfade_samples > 0;
        let mut tail_buf: std::collections::VecDeque<f32> = if capture_tail { std::collections::VecDeque::with_capacity(crossfade_samples) } else { std::collections::VecDeque::new() };

        // --- Create resampler if needed ---
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
            Async::new_sinc(
                output_rate as f64 / sample_rate as f64,
                2.0,
                &params,
                1024,
                channels,
                FixedAsync::Input,
            ).ok()
        } else {
            None
        };

        let chunk_size = resampler.as_ref().map(|r| r.input_frames_next()).unwrap_or(1024);
        let mut pending: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);

        // Reusable buffers
        let mut deinterleaved: Vec<Vec<f32>> = vec![Vec::with_capacity(chunk_size); channels];
        let mut interleaved_out: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut decoded_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut chunk_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels);
        let mut eq_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut fx_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut rg_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut xfeed_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        let mut bal_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
        // Scratch for crossfade-mixed / limited output. Only touched when samples need
        // mutation; steady-state playback reads bal_output directly and skips this.
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
                let buffered = RING_BUFFER_SIZE - producer.slots();
                if buffered > 0 {
                    state.discard_samples.store(buffered as u64, Ordering::Relaxed);
                    state.reset_consumer_counter.store(true, Ordering::Relaxed);
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

                let buffered = RING_BUFFER_SIZE - producer.slots();
                state.discard_samples.store(buffered as u64, Ordering::Relaxed);
                state.reset_consumer_counter.store(true, Ordering::Relaxed);

                if probed.format.seek(SeekMode::Coarse, SeekTo::Time {
                    time: Time::from(new_time),
                    track_id: Some(track_id)
                }).is_ok() {
                    state.samples_played.store((new_time * output_rate as f64) as u64, Ordering::Relaxed);
                }
            }

            // Throttle when buffer is full
            let free = producer.slots();
            if free < RING_BUFFER_SIZE / 4 {
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

            let raw = convert_samples(&decoded);
            if raw.is_empty() { continue; }

            pending.extend_from_slice(&raw);

            // Resample if needed
            decoded_buf.clear();
            if let Some(ref mut resampler) = resampler {
                while pending.len() >= chunk_size * channels {
                    chunk_buf.clear();
                    chunk_buf.extend_from_slice(&pending[..chunk_size * channels]);
                    pending.drain(..chunk_size * channels);

                    for ch_buf in deinterleaved.iter_mut() { ch_buf.clear(); }
                    for (i, &s) in chunk_buf.iter().enumerate() {
                        deinterleaved[i % channels].push(s);
                    }

                    let frames_in = chunk_size;
                    if let Ok(adapter_in) = SequentialSliceOfVecs::new(&deinterleaved, channels, frames_in) {
                        if let Ok(resampled) = resampler.process(&adapter_in, 0, None) {
                            interleaved_out.extend(resampled.take_data());
                        }
                    }
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
                &output[..]
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

            // Clipping check — scan whichever buffer holds the current samples
            if !bal_output.is_empty() {
                let vol = state.volume.load(Ordering::Relaxed) as f32 / 100.0;
                let threshold = if vol > 0.0 { 1.0 / vol } else { f32::MAX };
                let scan: &[f32] = if using_final_buf { &final_buf } else { bal_output };
                let peak = scan.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
                if peak > threshold {
                    state.clipping.store(true, Ordering::Relaxed);
                    // Peak-limit to prevent hard clipping at the DAC
                    if !using_final_buf {
                        final_buf.clear();
                        final_buf.extend_from_slice(bal_output);
                        using_final_buf = true;
                    }
                    let scale = threshold / peak;
                    for s in final_buf.iter_mut() {
                        *s *= scale;
                    }
                }
            }

            // Push to ring buffer
            let out: &[f32] = if using_final_buf { &final_buf } else { bal_output };
            if !out.is_empty() {
                if let Ok(mut chunk) = producer.write_chunk(out.len()) {
                    let (first, second) = chunk.as_mut_slices();
                    let first_len = first.len().min(out.len());
                    first[..first_len].copy_from_slice(&out[..first_len]);
                    if first_len < out.len() && !second.is_empty() {
                        let second_len = second.len().min(out.len() - first_len);
                        second[..second_len].copy_from_slice(&out[first_len..first_len + second_len]);
                    }
                    chunk.commit_all();
                }

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

        // Flush resampler
        if let Some(ref mut resampler) = resampler {
            if !pending.is_empty() {
                pending.resize(chunk_size * channels, 0.0);
                let input = deinterleave(&pending, channels);
                let frames_in = chunk_size;
                if let Ok(adapter_in) = SequentialSliceOfVecs::new(&input, channels, frames_in) {
                    if let Ok(resampled) = resampler.process(&adapter_in, 0, None) {
                        let output = resampled.take_data();
                        if let Ok(mut chunk) = producer.write_chunk(output.len()) {
                            let (first, second) = chunk.as_mut_slices();
                            let first_len = first.len().min(output.len());
                            first[..first_len].copy_from_slice(&output[..first_len]);
                            if first_len < output.len() && !second.is_empty() {
                                let second_len = second.len().min(output.len() - first_len);
                                second[..second_len].copy_from_slice(&output[first_len..first_len + second_len]);
                            }
                            chunk.commit_all();
                        }
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
