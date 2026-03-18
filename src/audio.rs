use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use cpal::traits::DeviceTrait;
use cpal::{Stream, StreamConfig};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use rtrb::{Producer, Consumer};

use crate::state::PlayerState;

/// On macOS, if the output device is Bluetooth, reset its nominal sample rate
/// to 48kHz (the actual hardware rate). CoreAudio can get stuck at a wrong rate
/// from a previous run that attempted to switch it. Returns the corrected rate
/// if a fix was applied, or None if no correction was needed.
pub fn fix_bluetooth_sample_rate() -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        if macos_audio::is_bluetooth_device() {
            let _ = macos_audio::set_device_sample_rate(48000);
            return Some(48000);
        }
    }
    None
}

pub fn probe_sample_rate(path: &Path) -> Option<u32> {
    let file = File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension() {
        hint.with_extension(ext.to_str().unwrap_or(""));
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?;
    let track = probed.format.tracks().iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)?;
    track.codec_params.sample_rate
}

#[cfg(target_os = "macos")]
#[allow(non_snake_case, non_upper_case_globals)]
mod macos_audio {
    use std::ffi::c_void;

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectSetPropertyData(
            inObjectID: u32,
            inAddress: *const AudioObjectPropertyAddress,
            inQualifierDataSize: u32,
            inQualifierData: *const c_void,
            inDataSize: u32,
            inData: *const c_void,
        ) -> i32;

        fn AudioObjectGetPropertyData(
            inObjectID: u32,
            inAddress: *const AudioObjectPropertyAddress,
            inQualifierDataSize: u32,
            inQualifierData: *const c_void,
            ioDataSize: *mut u32,
            outData: *mut c_void,
        ) -> i32;
    }

    #[repr(C)]
    #[allow(non_snake_case)]
    struct AudioObjectPropertyAddress {
        mSelector: u32,
        mScope: u32,
        mElement: u32,
    }

    #[allow(non_upper_case_globals)]
    const kAudioHardwarePropertyDefaultOutputDevice: u32 = 0x644F7574; // 'dOut'
    #[allow(non_upper_case_globals)]
    const kAudioDevicePropertyNominalSampleRate: u32 = 0x6E737274; // 'nsrt'
    #[allow(non_upper_case_globals)]
    const kAudioDevicePropertyTransportType: u32 = 0x7472616E; // 'tran'
    #[allow(non_upper_case_globals)]
    const kAudioObjectPropertyScopeGlobal: u32 = 0x676C6F62; // 'glob'
    #[allow(non_upper_case_globals)]
    const kAudioObjectPropertyElementMain: u32 = 0;
    #[allow(non_upper_case_globals)]
    const kAudioObjectSystemObject: u32 = 1;
    #[allow(non_upper_case_globals)]
    const kAudioDeviceTransportTypeBluetooth: u32 = 0x626C7565; // 'blue'
    #[allow(non_upper_case_globals)]
    const kAudioDeviceTransportTypeBluetoothLE: u32 = 0x626C6561; // 'blea'

    pub fn is_bluetooth_device() -> bool {
        unsafe {
            // Get default output device
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDefaultOutputDevice,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };

            let mut device_id: u32 = 0;
            let mut size: u32 = std::mem::size_of::<u32>() as u32;

            let status = AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                &mut device_id as *mut u32 as *mut c_void,
            );

            if status != 0 {
                return false;
            }

            // Get transport type
            let transport_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyTransportType,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };

            let mut transport_type: u32 = 0;
            let mut size: u32 = std::mem::size_of::<u32>() as u32;

            let status = AudioObjectGetPropertyData(
                device_id,
                &transport_address,
                0,
                std::ptr::null(),
                &mut size,
                &mut transport_type as *mut u32 as *mut c_void,
            );

            if status != 0 {
                return false;
            }

            transport_type == kAudioDeviceTransportTypeBluetooth
                || transport_type == kAudioDeviceTransportTypeBluetoothLE
        }
    }

    pub fn set_device_sample_rate(rate: u32) -> Result<(), String> {
        unsafe {
            // Get default output device
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDefaultOutputDevice,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };

            let mut device_id: u32 = 0;
            let mut size: u32 = std::mem::size_of::<u32>() as u32;

            let status = AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                &mut device_id as *mut u32 as *mut c_void,
            );

            if status != 0 {
                return Err(format!("Failed to get default output device: {}", status));
            }

            // Set sample rate
            let rate_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyNominalSampleRate,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };

            let rate_f64 = rate as f64;
            let status = AudioObjectSetPropertyData(
                device_id,
                &rate_address,
                0,
                std::ptr::null(),
                std::mem::size_of::<f64>() as u32,
                &rate_f64 as *const f64 as *const c_void,
            );

            if status != 0 {
                return Err(format!("Failed to set sample rate to {}: {}", rate, status));
            }

            // Brief delay for hardware to switch
            std::thread::sleep(std::time::Duration::from_millis(50));

            Ok(())
        }
    }

    pub fn get_device_sample_rate() -> Result<u32, String> {
        unsafe {
            // Get default output device
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDefaultOutputDevice,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };

            let mut device_id: u32 = 0;
            let mut size: u32 = std::mem::size_of::<u32>() as u32;

            let status = AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                &mut device_id as *mut u32 as *mut c_void,
            );

            if status != 0 {
                return Err(format!("Failed to get default output device: {}", status));
            }

            // Get sample rate
            let rate_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyNominalSampleRate,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };

            let mut rate_f64: f64 = 0.0;
            let mut size: u32 = std::mem::size_of::<f64>() as u32;

            let status = AudioObjectGetPropertyData(
                device_id,
                &rate_address,
                0,
                std::ptr::null(),
                &mut size,
                &mut rate_f64 as *mut f64 as *mut c_void,
            );

            if status != 0 {
                return Err(format!("Failed to get sample rate: {}", status));
            }

            Ok(rate_f64 as u32)
        }
    }
}

/// Try to set the system audio output sample rate to match the source.
/// Returns the actual rate to use (may differ if switching failed).
pub fn set_output_sample_rate(desired_rate: u32, current_rate: u32, device: &cpal::Device) -> u32 {
    if desired_rate == current_rate {
        return current_rate;
    }

    #[cfg(target_os = "macos")]
    {
        // Bluetooth devices (like AirPods) operate at a fixed rate (typically 48kHz).
        // CoreAudio lies about rate changes succeeding, causing sped-up audio and
        // buffer underruns. Skip rate switching entirely for Bluetooth.
        if macos_audio::is_bluetooth_device() {
            return current_rate;
        }

        let device_supports_rate = device.supported_output_configs()
            .map(|configs| {
                configs.into_iter().any(|config| {
                    config.min_sample_rate() <= desired_rate
                        && desired_rate <= config.max_sample_rate()
                })
            })
            .unwrap_or(false);

        if !device_supports_rate {
            return current_rate;
        }

        match macos_audio::set_device_sample_rate(desired_rate) {
            Ok(()) => {
                // Verify it actually changed
                if let Ok(actual) = macos_audio::get_device_sample_rate() {
                    if actual == desired_rate {
                        return desired_rate;
                    }
                }
            }
            Err(e) => {
                eprintln!("  Note: Could not switch to {}Hz: {}", desired_rate, e);
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // On Windows, virtual audio devices (like SteelSeries Sonar) don't support rate switching
        // and may report incorrect capabilities. Always use the current device rate and let
        // our resampler handle conversion if needed. This prevents pitch shifting issues.
        let _ = (desired_rate, device);
        return current_rate;
    }

    #[cfg(target_os = "linux")]
    {
        // On Linux with PipeWire, just request the rate - PipeWire handles switching
        let _ = device;
        return desired_rate;
    }

    // Fallback: keep current rate (will resample)
    #[allow(unreachable_code)]
    {
        let _ = device;
        current_rate
    }
}

pub fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    mut consumer: Consumer<f32>,
    mut viz_producer: Producer<f32>,
    state: Arc<PlayerState>,
) -> Result<Stream, Box<dyn std::error::Error>> {
    let channels = config.channels as usize;
    let sample_rate = config.sample_rate;

    // Local counter for batched updates
    let mut local_played: u64 = 0;
    let batch_size = sample_rate as u64 / 4; // Update ~4x per second

    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _| {
            let paused = state.is_paused();

            // Check if seek happened - drain buffer immediately for instant response
            if state.reset_consumer_counter.swap(false, Ordering::Relaxed) {
                local_played = 0;
                // Drain all buffered samples instantly
                let to_drain = consumer.slots();
                if to_drain > 0 {
                    if let Ok(chunk) = consumer.read_chunk(to_drain) {
                        chunk.commit_all(); // Discard without processing
                    }
                }
                state.discard_samples.store(0, Ordering::Relaxed);
                data.fill(0.0);
                return;
            }

            // Use chunk reads for efficiency
            let available = consumer.slots();

            // Update buffer level so main thread can detect track end
            state.buffer_level.store(available, Ordering::Relaxed);

            if paused || available == 0 {
                // Output silence
                data.fill(0.0);
                return;
            }

            // Ring buffer always contains stereo (2ch) samples
            // Guard: channels must be >= 1 to avoid division by zero
            if channels == 0 {
                data.fill(0.0);
                return;
            }

            let source_channels = 2usize; // Our ring buffer is always stereo
            let frames_needed = data.len() / channels;
            let samples_to_read = (frames_needed * source_channels).min(available);

            if let Ok(chunk) = consumer.read_chunk(samples_to_read) {
                let (first, second) = chunk.as_slices();
                let gain = state.volume_gain();

                // Process both ring buffer slices sequentially (no heap allocation)
                // out_step: always need at least 2 free slots (L+R) even for mono downmix
                let mut out_idx = 0;
                let slices: [&[f32]; 2] = [first, second];
                let mut src_idx = 0;
                let mut current_slice = 0;

                while current_slice < 2 && out_idx < data.len() {
                    let slice = slices[current_slice];
                    if src_idx + 1 >= slice.len() {
                        current_slice += 1;
                        src_idx = 0;
                        continue;
                    }

                    let left = slice[src_idx] * gain;
                    let right = slice[src_idx + 1] * gain;

                    if channels == 1 {
                        data[out_idx] = (left + right) * 0.5;
                    } else if out_idx + 1 < data.len() {
                        data[out_idx] = left;
                        data[out_idx + 1] = right;
                        for ch in 2..channels {
                            if out_idx + ch < data.len() {
                                data[out_idx + ch] = 0.0;
                            }
                        }
                    } else {
                        break; // Not enough space for a full frame
                    }

                    out_idx += channels;
                    src_idx += source_channels;
                    local_played += 1;
                }

                chunk.commit_all();

                // Tap played stereo samples into viz buffer (best-effort, drop if full)
                // Pre-fader mode: undo volume gain so viz shows raw signal levels
                let frames_written = if channels > 0 { out_idx / channels } else { 0 };
                let viz_samples = frames_written * 2; // stereo
                let pre_fader = state.is_pre_fader();
                let viz_scale = if pre_fader && gain > 0.0 { 1.0 / gain } else { 1.0 };
                if viz_samples > 0 {
                    let viz_free = viz_producer.slots();
                    if viz_free >= viz_samples {
                        if let Ok(mut vchunk) = viz_producer.write_chunk(viz_samples) {
                            let (vfirst, vsecond) = vchunk.as_mut_slices();
                            let viz_total = vfirst.len() + vsecond.len();

                            if channels == 2 && viz_scale == 1.0 && viz_samples <= data.len() {
                                // Fast path: post-fader stereo bulk copy
                                let src = &data[..viz_samples];
                                let first_len = vfirst.len().min(viz_samples);
                                vfirst[..first_len].copy_from_slice(&src[..first_len]);
                                if first_len < viz_samples {
                                    let rem = viz_samples - first_len;
                                    let rem = rem.min(vsecond.len());
                                    vsecond[..rem].copy_from_slice(&src[first_len..first_len + rem]);
                                }
                            } else {
                                // Scaled path: extract L/R, apply viz_scale
                                let mut vi = 0;
                                for f in 0..frames_written {
                                    let di = f * channels;
                                    if di >= data.len() { break; }
                                    let l = data[di] * viz_scale;
                                    let r = if channels >= 2 && di + 1 < data.len() {
                                        data[di + 1] * viz_scale
                                    } else {
                                        l
                                    };
                                    for &val in &[l, r] {
                                        if vi >= viz_total { break; }
                                        if vi < vfirst.len() {
                                            vfirst[vi] = val;
                                        } else {
                                            vsecond[vi - vfirst.len()] = val;
                                        }
                                        vi += 1;
                                    }
                                }
                            }
                            vchunk.commit_all();
                        }
                    }
                }

                // Fill remainder with silence
                data[out_idx..].fill(0.0);
            } else {
                data.fill(0.0);
            }

            // Batch update
            if local_played >= batch_size {
                state.samples_played.fetch_add(local_played, Ordering::Relaxed);
                local_played = 0;
            }
        },
        |e| eprintln!("Audio error: {}", e),
        None,
    )?;

    Ok(stream)
}
