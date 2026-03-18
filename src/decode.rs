use std::fs::File;
use std::path::Path;
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

fn convert_samples(buf: &AudioBufferRef) -> Vec<f32> {
    match buf {
        AudioBufferRef::F32(b) => {
            let spec = b.planes();
            let p = spec.planes();
            let mut out = Vec::with_capacity(b.frames() * p.len());
            for f in 0..b.frames() {
                for ch in p { out.push(ch[f]); }
            }
            out
        }
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
        _ => vec![],
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

pub fn decode_track(
    path: &Path,
    producer: &mut Producer<f32>,
    state: &PlayerState,
    output_rate: u32,
    hq_resampler: bool,
    eq: &mut crate::eq::EqChain,
    eq_presets: &[crate::eq::EqPreset],
    effects: &mut crate::effects::EffectsChain,
    effects_presets: &[crate::effects::EffectsPreset],
    crossfade_in: Option<&[f32]>,
    crossfade_samples: usize,
) -> Result<Option<Vec<f32>>, String> {
    // Open file
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension() {
        hint.with_extension(ext.to_str().unwrap_or(""));
    }

    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| e.to_string())?;

    let mut format = probed.format;
    let track = format.tracks().iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or("No audio track")?;

    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);
    let bits_per_sample = track.codec_params.bits_per_sample.unwrap_or(16);
    let total = track.codec_params.n_frames.unwrap_or(0);

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| e.to_string())?;

    // Update state
    state.sample_rate.store(sample_rate as u64, Ordering::Relaxed);
    state.total_samples.store(total, Ordering::Relaxed);
    state.samples_played.store(0, Ordering::Relaxed);
    state.channels.store(channels, Ordering::Relaxed);
    state.bits_per_sample.store(bits_per_sample as usize, Ordering::Relaxed);
    state.track_info_ready.store(true, Ordering::Relaxed);

    // Reset filter states for new track
    eq.reset();
    effects.reset();

    // Crossfade state
    let mut crossfade_pos: usize = 0;
    // Rolling tail buffer: keeps the last crossfade_samples of processed audio
    let capture_tail = crossfade_samples > 0;
    let mut tail_buf: Vec<f32> = if capture_tail { Vec::with_capacity(crossfade_samples) } else { Vec::new() };

    // Create resampler only if needed
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

    // Reusable buffers to avoid allocations in hot loop
    let mut deinterleaved: Vec<Vec<f32>> = vec![Vec::with_capacity(chunk_size); channels];
    let mut interleaved_out: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
    let mut chunk_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels);
    let mut eq_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
    let mut fx_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);
    let mut xfade_buf: Vec<f32> = Vec::with_capacity(chunk_size * channels * 2);

    loop {
        // Check control flags - only READ skip flags, don't consume (main loop consumes)
        if state.should_quit() || state.is_skip_requested() {
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

            // Tell consumer to discard buffered samples for instant seek
            let buffered = RING_BUFFER_SIZE - producer.slots();
            state.discard_samples.store(buffered as u64, Ordering::Relaxed);
            state.reset_consumer_counter.store(true, Ordering::Relaxed);

            if format.seek(SeekMode::Coarse, SeekTo::Time {
                time: Time::from(new_time),
                track_id: Some(track_id)
            }).is_ok() {
                // samples_played is counted at output_rate, not source sample_rate
                state.samples_played.store((new_time * output_rate as f64) as u64, Ordering::Relaxed);
            }
        }

        // Throttle when buffer is full (but stay responsive to seek)
        let free = producer.slots();

        if free < RING_BUFFER_SIZE / 4 {
            // Short sleep, check for seek more frequently
            thread::sleep(Duration::from_millis(20));
            continue;
        }

        // Pause handling (shorter sleep for responsiveness)
        if state.is_paused() {
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        // Decode next packet
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break, // EOF or error
        };

        if packet.track_id() != track_id { continue; }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };

        let samples = convert_samples(&decoded);

        // Resample if needed (reusing buffers to avoid allocations)
        let output = if let Some(ref mut resampler) = resampler {
            pending.extend(&samples);
            let chunk_samples = chunk_size * channels;
            interleaved_out.clear();

            while pending.len() >= chunk_samples {
                // Reuse chunk_buf instead of allocating
                chunk_buf.clear();
                chunk_buf.extend(pending.drain(..chunk_samples));

                // Deinterleave into reusable buffers
                for ch in &mut deinterleaved { ch.clear(); }
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
            &interleaved_out[..]
        } else {
            &samples[..]
        };

        // Check for live EQ preset change (crossfades naturally as buffer drains)
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

        // Apply EQ processing
        let eq_output = if eq.is_active() {
            eq_buf.clear();
            eq_buf.extend_from_slice(output);
            eq.process_stereo(&mut eq_buf);
            &eq_buf[..]
        } else {
            output
        };

        // Apply effects processing (chorus -> delay -> reverb)
        let processed = if effects.is_active() {
            fx_buf.clear();
            fx_buf.extend_from_slice(eq_output);
            effects.process_stereo(&mut fx_buf);
            &fx_buf[..]
        } else {
            eq_output
        };

        // Apply crossfade mixing with previous track's tail
        let final_output = if let Some(tail) = crossfade_in {
            if crossfade_pos < crossfade_samples && crossfade_samples > 0 {
                xfade_buf.clear();
                xfade_buf.extend_from_slice(processed);

                for sample in xfade_buf.iter_mut() {
                    if crossfade_pos < crossfade_samples {
                        let pos_f = crossfade_pos as f32 / crossfade_samples as f32;
                        let fade_in = (pos_f * std::f32::consts::FRAC_PI_2).sin();
                        let fade_out = ((1.0 - pos_f) * std::f32::consts::FRAC_PI_2).sin();

                        let tail_sample = if crossfade_pos < tail.len() { tail[crossfade_pos] } else { 0.0 };
                        *sample = *sample * fade_in + tail_sample * fade_out;
                        crossfade_pos += 1;
                    }
                }
                &xfade_buf[..]
            } else {
                processed
            }
        } else {
            processed
        };

        // Push to ring buffer using write_chunk for efficiency
        if !final_output.is_empty() {
            if let Ok(mut chunk) = producer.write_chunk(final_output.len()) {
                let (first, second) = chunk.as_mut_slices();
                let first_len = first.len().min(final_output.len());
                first[..first_len].copy_from_slice(&final_output[..first_len]);
                if first_len < final_output.len() && !second.is_empty() {
                    let second_len = second.len().min(final_output.len() - first_len);
                    second[..second_len].copy_from_slice(&final_output[first_len..first_len + second_len]);
                }
                chunk.commit_all();
            }

            // Capture tail for crossfade into next track
            if capture_tail {
                tail_buf.extend_from_slice(final_output);
                if tail_buf.len() > crossfade_samples {
                    let excess = tail_buf.len() - crossfade_samples;
                    tail_buf.drain(..excess);
                }
            }
        }
    }

    // Flush remaining samples
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

    if capture_tail && !tail_buf.is_empty() {
        Ok(Some(tail_buf))
    } else {
        Ok(None)
    }
}
