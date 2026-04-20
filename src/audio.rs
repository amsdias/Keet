use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use cpal::traits::DeviceTrait;
use cpal::traits::HostTrait;
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

/// Print numbered list of output devices to stdout
pub fn list_output_devices(host: &cpal::Host) {
    match host.output_devices() {
        Ok(devices) => {
            let default_name = host.default_output_device()
                .and_then(|d| d.description().ok())
                .map(|d| d.name().to_string());

            println!("Output devices:");
            for (i, device) in devices.enumerate() {
                let name = device.description()
                    .map(|d| d.name().to_string())
                    .unwrap_or_else(|_| "Unknown".to_string());
                let suffix = if default_name.as_ref() == Some(&name) { " (default)" } else { "" };
                println!("  {}. {}{}", i + 1, name, suffix);
            }
        }
        Err(e) => eprintln!("Cannot enumerate devices: {}", e),
    }
}

/// Find an output device by substring match (case-insensitive)
pub fn find_device_by_name(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    let name_lower = name.to_lowercase();
    host.output_devices().ok()?
        .find(|d| {
            d.description()
                .map(|desc| desc.name().to_lowercase().contains(&name_lower))
                .unwrap_or(false)
        })
}

/// Query the maximum sample rate supported by a device
pub fn max_supported_rate(device: &cpal::Device) -> u32 {
    device.supported_output_configs()
        .map(|configs| {
            configs.into_iter()
                .map(|c| c.max_sample_rate())
                .max()
                .unwrap_or(48000)
        })
        .unwrap_or(48000)
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

        fn AudioObjectGetPropertyDataSize(
            inObjectID: u32,
            inAddress: *const AudioObjectPropertyAddress,
            inQualifierDataSize: u32,
            inQualifierData: *const c_void,
            outDataSize: *mut u32,
        ) -> i32;

        fn AudioObjectIsPropertySettable(
            inObjectID: u32,
            inAddress: *const AudioObjectPropertyAddress,
            outIsSettable: *mut u8,
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
    const kAudioDevicePropertyHogMode: u32 = 0x686F676D; // 'hogm'
    const kAudioHardwarePropertyDevices: u32 = 0x64657623; // 'dev#'

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

    pub fn get_default_device_id() -> Option<u32> {
        unsafe {
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDefaultOutputDevice,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut device_id: u32 = 0;
            let mut size: u32 = std::mem::size_of::<u32>() as u32;
            let status = AudioObjectGetPropertyData(
                kAudioObjectSystemObject, &address, 0, std::ptr::null(),
                &mut size, &mut device_id as *mut u32 as *mut c_void,
            );
            if status != 0 { None } else { Some(device_id) }
        }
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringGetLength(theString: *const c_void) -> isize;
        fn CFStringGetCString(
            theString: *const c_void,
            buffer: *mut u8,
            bufferSize: isize,
            encoding: u32,
        ) -> bool;
        fn CFRelease(cf: *const c_void);
    }

    const kCFStringEncodingUTF8: u32 = 0x08000100;
    const kAudioObjectPropertyName: u32 = 0x6C6E616D; // 'lnam'

    fn get_device_name_by_id(device_id: u32) -> Option<String> {
        unsafe {
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioObjectPropertyName,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut name_ref: *const c_void = std::ptr::null();
            let mut size: u32 = std::mem::size_of::<*const c_void>() as u32;
            let status = AudioObjectGetPropertyData(
                device_id, &address, 0, std::ptr::null(),
                &mut size, &mut name_ref as *mut _ as *mut c_void,
            );
            if status != 0 || name_ref.is_null() { return None; }
            let len = CFStringGetLength(name_ref);
            let buf_size = (len * 4 + 1) as usize;
            let mut buf = vec![0u8; buf_size];
            let ok = CFStringGetCString(name_ref, buf.as_mut_ptr(), buf_size as isize, kCFStringEncodingUTF8);
            CFRelease(name_ref);
            if ok {
                let cstr = std::ffi::CStr::from_ptr(buf.as_ptr() as *const std::ffi::c_char);
                Some(cstr.to_string_lossy().into_owned())
            } else {
                None
            }
        }
    }

    pub fn find_device_id_by_name(name: &str) -> Option<u32> {
        unsafe {
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDevices,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut size: u32 = 0;
            let status = AudioObjectGetPropertyDataSize(
                kAudioObjectSystemObject, &address, 0, std::ptr::null(), &mut size,
            );
            if status != 0 { return None; }
            let count = size as usize / std::mem::size_of::<u32>();
            let mut device_ids = vec![0u32; count];
            let status = AudioObjectGetPropertyData(
                kAudioObjectSystemObject, &address, 0, std::ptr::null(),
                &mut size, device_ids.as_mut_ptr() as *mut c_void,
            );
            if status != 0 { return None; }
            let name_lower = name.to_lowercase();
            for &did in &device_ids {
                if let Some(device_name) = get_device_name_by_id(did) {
                    if device_name.to_lowercase().contains(&name_lower) {
                        return Some(did);
                    }
                }
            }
            None
        }
    }

    pub fn set_hog_mode(device_id: u32) -> Result<(), String> {
        unsafe {
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyHogMode,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };

            // Check if hog mode is supported by this device
            let mut settable: u8 = 0;
            let has_prop = AudioObjectIsPropertySettable(
                device_id, &address, &mut settable,
            );
            if has_prop != 0 || settable == 0 {
                return Err("device does not support hog mode (built-in speakers, AirPlay, and virtual devices typically don't)".to_string());
            }

            let pid = std::process::id() as i32;
            let status = AudioObjectSetPropertyData(
                device_id, &address, 0, std::ptr::null(),
                std::mem::size_of::<i32>() as u32,
                &pid as *const i32 as *const c_void,
            );
            if status != 0 {
                let code_bytes = status.to_be_bytes();
                let fourcc: String = code_bytes.iter()
                    .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
                    .collect();
                return Err(format!("CoreAudio error '{}' ({})", fourcc, status));
            }
            Ok(())
        }
    }

    pub fn release_hog_mode(device_id: u32) {
        unsafe {
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyHogMode,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let pid: i32 = -1;
            let _ = AudioObjectSetPropertyData(
                device_id, &address, 0, std::ptr::null(),
                std::mem::size_of::<i32>() as u32,
                &pid as *const i32 as *const c_void,
            );
        }
    }

    #[allow(dead_code)]
    pub fn set_device_sample_rate_for_id(device_id: u32, rate: u32) -> Result<(), String> {
        unsafe {
            let rate_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyNominalSampleRate,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let rate_f64 = rate as f64;
            let status = AudioObjectSetPropertyData(
                device_id, &rate_address, 0, std::ptr::null(),
                std::mem::size_of::<f64>() as u32,
                &rate_f64 as *const f64 as *const c_void,
            );
            if status != 0 {
                return Err(format!("Failed to set sample rate to {}: {}", rate, status));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok(())
        }
    }

    #[allow(dead_code)]
    pub fn is_bluetooth_device_by_id(device_id: u32) -> bool {
        unsafe {
            let transport_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyTransportType,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut transport_type: u32 = 0;
            let mut size: u32 = std::mem::size_of::<u32>() as u32;
            let status = AudioObjectGetPropertyData(
                device_id, &transport_address, 0, std::ptr::null(),
                &mut size, &mut transport_type as *mut u32 as *mut c_void,
            );
            if status != 0 { return false; }
            transport_type == kAudioDeviceTransportTypeBluetooth
                || transport_type == kAudioDeviceTransportTypeBluetoothLE
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

/// Set exclusive (hog) mode on the output device. macOS only.
/// Returns the CoreAudio device ID if successful, for later release.
pub fn set_exclusive_mode(device: &cpal::Device) -> Result<u32, String> {
    #[cfg(target_os = "macos")]
    {
        let device_name = device.description()
            .map(|d| d.name().to_string())
            .unwrap_or_default();
        let device_id = macos_audio::find_device_id_by_name(&device_name)
            .or_else(|| macos_audio::get_default_device_id())
            .ok_or_else(|| "Could not find CoreAudio device ID".to_string())?;

        macos_audio::set_hog_mode(device_id)?;
        Ok(device_id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = device;
        Err("Exclusive mode not supported on this platform".to_string())
    }
}

/// Release exclusive (hog) mode on the output device. macOS only.
pub fn release_exclusive_mode(device_id: u32) {
    #[cfg(target_os = "macos")]
    {
        macos_audio::release_hog_mode(device_id);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = device_id;
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
    let err_state = Arc::clone(&state);

    let stream = device.build_output_stream(
        config,
        move |data: &mut [f32], _| {
            let paused = state.is_paused();

            // Check if seek happened - drain buffer immediately for instant response
            if state.reset_consumer_counter.swap(false, Ordering::Relaxed) {
                // Drain all buffered samples instantly
                let to_drain = consumer.slots();
                if to_drain > 0 {
                    if let Ok(chunk) = consumer.read_chunk(to_drain) {
                        chunk.commit_all(); // Discard without processing
                    }
                }
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

                    // Clamp post-gain to prevent DAC clipping. Producer only
                    // flags clipping (state.clipping) — can't scale there since
                    // volume may change between scan and this callback.
                    let left = (slice[src_idx] * gain).clamp(-1.0, 1.0);
                    let right = (slice[src_idx + 1] * gain).clamp(-1.0, 1.0);

                    if channels == 1 {
                        data[out_idx] = ((left + right) * 0.5).clamp(-1.0, 1.0);
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
                }

                chunk.commit_all();

                // Track playback position (frames consumed from ring buffer)
                let consumed_frames = samples_to_read / source_channels;
                state.samples_played.fetch_add(consumed_frames as u64, Ordering::Relaxed);

                // Tap played stereo samples into viz buffer (best-effort, drop if full)
                // Pre-fader mode: undo volume gain so viz shows raw signal levels
                let frames_written = if channels > 0 { out_idx / channels } else { 0 };
                let viz_samples = frames_written * 2; // stereo
                let pre_fader = state.is_pre_fader();
                let viz_scale = if pre_fader && gain > 0.0 { 1.0 / gain } else { 1.0 };
                if viz_samples > 0 {
                    if channels == 2 && viz_scale == 1.0 && viz_samples <= data.len() {
                        // Fast path: post-fader stereo bulk copy
                        let _ = viz_producer.push_partial_slice(&data[..viz_samples]);
                    } else if viz_producer.slots() >= viz_samples {
                        if let Ok(mut vchunk) = viz_producer.write_chunk(viz_samples) {
                            let (vfirst, vsecond) = vchunk.as_mut_slices();
                            let viz_total = vfirst.len() + vsecond.len();

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
                            vchunk.commit_all();
                        }
                    }
                }

                // Fill remainder with silence
                data[out_idx..].fill(0.0);
            } else {
                data.fill(0.0);
            }

        },
        move |_e| {
            // cpal's error callback can run on audio-thread adjacent paths; avoid I/O here.
            // The main loop detects stream_error and reports to the UI.
            err_state.stream_error.store(true, std::sync::atomic::Ordering::Relaxed);
        },
        None,
    )?;

    Ok(stream)
}
