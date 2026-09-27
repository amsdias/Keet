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
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::TrackType;

use rtrb::{Producer, Consumer};

use crate::state::PlayerState;

/// On macOS, if the output device is Bluetooth, reset its nominal sample rate
/// to 48kHz (the actual hardware rate). CoreAudio can get stuck at a wrong rate
/// from a previous run that attempted to switch it. Returns the corrected rate
/// if a fix was applied, or None if no correction was needed.
pub fn fix_bluetooth_sample_rate(device: &cpal::Device) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        if let Some(id) = coreaudio_device_id(device) {
            if macos_audio::is_bluetooth_device_by_id(id) {
                let _ = macos_audio::set_device_sample_rate_for_id(id, 48000);
                return Some(48000);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = device;
    None
}

/// The CoreAudio id of a cpal output device. cpal's `DeviceId` carries the
/// device's persistent UID, which identifies it exactly; the name is only a
/// fallback (cpal reports some names with trailing spaces — "FIIO KA17 " —
/// so a name lookup that silently fell through to "the system default" was
/// right only by luck, and wrong the moment the default moved).
#[cfg(target_os = "macos")]
fn coreaudio_device_id(device: &cpal::Device) -> Option<u32> {
    if let Ok(id) = device.id() {
        if let Some(found) = macos_audio::find_device_id_by_uid(id.id()) {
            return Some(found);
        }
    }
    let name = device.description().map(|d| d.name().to_string()).unwrap_or_default();
    macos_audio::find_device_id_by_name(&name).or_else(macos_audio::get_default_device_id)
}

/// The same hardware as `device`, but as a device PINNED to it. On macOS,
/// `default_output_device()` hands back a device whose stream rides the
/// DefaultOutput unit and FOLLOWS the system default; one from
/// `output_devices()` stays on its hardware. Exclusive mode must be pinned:
/// hogging the default makes macOS move the system default elsewhere, and a
/// default-following stream then drifted off the device Keet had hogged —
/// audio on one device, hog mode and rate switches on another.
pub fn pin_device(host: &cpal::Host, device: &cpal::Device) -> Option<cpal::Device> {
    let want = device.id().ok()?;
    host.output_devices()
        .ok()?
        .find(|d| d.id().ok().as_ref() == Some(&want))
}

/// Pick the device whose name matches `wanted`: an exact (case-insensitive)
/// match beats a substring one. Substring-only matching took the FIRST hit,
/// so asking for "Speakers" could land on "External Speakers" when
/// "MacBook Pro Speakers" was meant, or vice versa.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn pick_device_id(devices: &[(u32, String)], wanted: &str) -> Option<u32> {
    let want = wanted.trim().to_lowercase();
    if want.is_empty() {
        return None;
    }
    devices
        .iter()
        .find(|(_, n)| n.trim().to_lowercase() == want)
        .or_else(|| devices.iter().find(|(_, n)| n.to_lowercase().contains(&want)))
        .map(|&(id, _)| id)
}

pub fn probe_sample_rate(path: &Path) -> Option<u32> {
    let file = File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension() {
        hint.with_extension(ext.to_str().unwrap_or(""));
    }
    let format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .ok()?;
    // 0.6: codec_params is Option (None = unplayable track) and carries a
    // per-media-type payload, so audio() replaces the old CODEC_TYPE_NULL check.
    let track = format.default_track(TrackType::Audio)?;
    track.codec_params.as_ref()?.audio()?.sample_rate
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
                // The shared-mode mixer format. On Windows this is the ONLY
                // format WASAPI accepts without conversion, so it's the first
                // thing to check when a stream fails to build.
                match device.default_output_config() {
                    Ok(c) => println!(
                        "       default: {} ch, {} Hz, {:?}",
                        c.channels(), c.sample_rate(), c.sample_format()
                    ),
                    Err(e) => println!("       default: <unavailable: {}>", e),
                }
                // Group by (channels, format) and list the rates — devices
                // enumerate one entry per discrete rate, so a flat dump is
                // dozens of near-identical lines (and truncating it hides the
                // rate that actually matters).
                if let Ok(configs) = device.supported_output_configs() {
                    let mut groups: Vec<((u16, cpal::SampleFormat), Vec<u32>)> = Vec::new();
                    for sc in configs {
                        let key = (sc.channels(), sc.sample_format());
                        let lo = sc.min_sample_rate();
                        let hi = sc.max_sample_rate();
                        let entry = match groups.iter_mut().find(|(k, _)| *k == key) {
                            Some((_, rates)) => rates,
                            None => {
                                groups.push((key, Vec::new()));
                                &mut groups.last_mut().unwrap().1
                            }
                        };
                        entry.push(lo);
                        if hi != lo {
                            entry.push(hi);
                        }
                    }
                    for ((ch, fmt), mut rates) in groups {
                        rates.sort_unstable();
                        rates.dedup();
                        let list: Vec<String> = rates.iter().map(|r| r.to_string()).collect();
                        println!("       supports: {} ch, {:?}, rates: {}", ch, fmt, list.join(", "));
                    }
                }
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
    // 'oink' is Apple's actual selector. This was once written as 'hogm', a
    // property that does not exist: CoreAudio answered "not settable" on every
    // device, so `--exclusive` never acquired hog mode on anything (USB DACs
    // included) and always printed "device does not support hog mode".
    const kAudioDevicePropertyHogMode: u32 = 0x6F696E6B; // 'oink'
    const kAudioHardwarePropertyDevices: u32 = 0x64657623; // 'dev#'

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
    const kAudioDevicePropertyDeviceUID: u32 = 0x75696420; // 'uid '

    // Every hand-written CoreAudio constant is pinned to the official bindings
    // at compile time — a wrong four-char code (the 'hogm' hog-mode selector)
    // fails silently at runtime, so make it fail the build instead.
    const _: () = {
        use coreaudio_sys as ca;
        assert!(kAudioDevicePropertyHogMode == ca::kAudioDevicePropertyHogMode);
        assert!(kAudioHardwarePropertyDefaultOutputDevice == ca::kAudioHardwarePropertyDefaultOutputDevice);
        assert!(kAudioDevicePropertyNominalSampleRate == ca::kAudioDevicePropertyNominalSampleRate);
        assert!(kAudioDevicePropertyTransportType == ca::kAudioDevicePropertyTransportType);
        assert!(kAudioObjectPropertyScopeGlobal == ca::kAudioObjectPropertyScopeGlobal);
        assert!(kAudioObjectPropertyElementMain == ca::kAudioObjectPropertyElementMain);
        assert!(kAudioObjectSystemObject == ca::kAudioObjectSystemObject);
        assert!(kAudioDeviceTransportTypeBluetooth == ca::kAudioDeviceTransportTypeBluetooth);
        assert!(kAudioDeviceTransportTypeBluetoothLE == ca::kAudioDeviceTransportTypeBluetoothLE);
        assert!(kAudioHardwarePropertyDevices == ca::kAudioHardwarePropertyDevices);
        assert!(kAudioObjectPropertyName == ca::kAudioObjectPropertyName);
        assert!(kAudioDevicePropertyDeviceUID == ca::kAudioDevicePropertyDeviceUID);
        assert!(kCFStringEncodingUTF8 == ca::kCFStringEncodingUTF8);
    };

    fn get_device_name_by_id(device_id: u32) -> Option<String> {
        get_cfstring_property(device_id, kAudioObjectPropertyName)
    }

    /// The device's persistent UID — what cpal's `DeviceId` carries on macOS.
    fn get_device_uid_by_id(device_id: u32) -> Option<String> {
        get_cfstring_property(device_id, kAudioDevicePropertyDeviceUID)
    }

    fn get_cfstring_property(device_id: u32, selector: u32) -> Option<String> {
        unsafe {
            let address = AudioObjectPropertyAddress {
                mSelector: selector,
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
        let named: Vec<(u32, String)> = all_device_ids()?
            .into_iter()
            .filter_map(|did| get_device_name_by_id(did).map(|n| (did, n)))
            .collect();
        super::pick_device_id(&named, name)
    }

    /// Exact lookup by the persistent device UID — no name matching.
    pub fn find_device_id_by_uid(uid: &str) -> Option<u32> {
        all_device_ids()?
            .into_iter()
            .find(|&did| get_device_uid_by_id(did).as_deref() == Some(uid))
    }

    fn all_device_ids() -> Option<Vec<u32>> {
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
            Some(device_ids)
        }
    }

    pub fn get_device_sample_rate_for_id(device_id: u32) -> Result<u32, String> {
        unsafe {
            let rate_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyNominalSampleRate,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let mut rate_f64: f64 = 0.0;
            let mut size: u32 = std::mem::size_of::<f64>() as u32;
            let status = AudioObjectGetPropertyData(
                device_id, &rate_address, 0, std::ptr::null(),
                &mut size, &mut rate_f64 as *mut f64 as *mut c_void,
            );
            if status != 0 {
                return Err(format!("Failed to get sample rate: {}", status));
            }
            Ok(rate_f64 as u32)
        }
    }

    /// Whether the device lets a process take hog (exclusive) mode. Read-only:
    /// asks CoreAudio, takes nothing.
    pub fn hog_mode_settable(device_id: u32) -> bool {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyHogMode,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut settable: u8 = 0;
        let status = unsafe { AudioObjectIsPropertySettable(device_id, &address, &mut settable) };
        status == 0 && settable != 0
    }

    /// PID of the process holding hog mode on the device, or None if nobody
    /// does (CoreAudio reports -1). Read-only.
    pub fn hog_owner(device_id: u32) -> Option<i32> {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyHogMode,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut pid: i32 = -1;
        let mut size = std::mem::size_of::<i32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id, &address, 0, std::ptr::null(), &mut size,
                &mut pid as *mut i32 as *mut c_void,
            )
        };
        (status == 0 && pid >= 0).then_some(pid)
    }

    pub fn set_hog_mode(device_id: u32) -> Result<(), String> {
        unsafe {
            let address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyHogMode,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };

            if !hog_mode_settable(device_id) {
                return Err("the device does not allow exclusive (hog) mode".to_string());
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
            // Confirm it took: a success status is not proof, and "exclusive
            // mode is on" must mean this process really owns the device.
            match hog_owner(device_id) {
                Some(owner) if owner == pid => Ok(()),
                other => Err(format!("hog mode not granted (owner: {other:?})")),
            }
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
        }
        // The change is asynchronous: wait until the device REPORTS the new
        // rate. A fixed 50 ms was too short for a USB DAC (FiiO KA17): the
        // read-back still showed the old rate, Keet concluded the switch had
        // failed and resampled — while the DAC had in fact switched.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if get_device_sample_rate_for_id(device_id).ok() == Some(rate) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("device did not settle at {rate} Hz within 2 s"));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

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

/// Capture what `set_output_sample_rate` (plus main's max-rate cap) can reach
/// on `device`, so the producer can tell in advance whether a track boundary
/// would actually change the output rate. Without this it compared the raw
/// file rate against the output rate and tore the stream down and rebuilt it
/// at the SAME rate on every boundary a device could not follow (44.1 kHz
/// albums on a 48 kHz-only DAC, Bluetooth, 352.8 kHz files on a 192 kHz DAC).
pub fn probe_rate_caps(device: &cpal::Device) -> crate::state::RateCaps {
    let ranges: Vec<(u32, u32)> = device
        .supported_output_configs()
        .map(|configs| configs.map(|c| (c.min_sample_rate(), c.max_sample_rate())).collect())
        .unwrap_or_default();
    let max = max_supported_rate(device);
    #[cfg(target_os = "macos")]
    let caps = crate::state::RateCaps {
        ranges,
        max,
        fixed: coreaudio_device_id(device).is_some_and(macos_audio::is_bluetooth_device_by_id),
        any: false,
    };
    #[cfg(target_os = "windows")]
    let caps = crate::state::RateCaps { ranges, max, fixed: true, any: false };
    #[cfg(target_os = "linux")]
    let caps = crate::state::RateCaps { ranges, max, fixed: false, any: true };
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let caps = crate::state::RateCaps { ranges, max, fixed: false, any: false };
    caps
}

/// Try to set the output device's sample rate to match the source.
/// Returns the actual rate to use (may differ if switching failed).
///
/// Call it with NO stream open on the device: cpal listens to the device's
/// rate and reports `StreamInvalidated` for a stream alive during a change,
/// which Keet's recovery then acted on — a spurious rebuild, "output moved"
/// status and a restart at the last whole second on every rate switch.
pub fn set_output_sample_rate(desired_rate: u32, current_rate: u32, device: &cpal::Device) -> u32 {
    if desired_rate == current_rate {
        return current_rate;
    }

    #[cfg(target_os = "macos")]
    {
        // Bluetooth devices (like AirPods) operate at a fixed rate (typically 48kHz).
        // CoreAudio lies about rate changes succeeding, causing sped-up audio and
        // buffer underruns. Skip rate switching entirely for Bluetooth.
        // Everything below acts on THIS device's CoreAudio id. The helpers
        // used to target the system default output, so with `--device DAC`
        // while the default was the built-in speakers, the speakers' rate was
        // changed and then "verified" — the DAC never moved.
        let Some(device_id) = coreaudio_device_id(device) else {
            return current_rate;
        };
        if macos_audio::is_bluetooth_device_by_id(device_id) {
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

        match macos_audio::set_device_sample_rate_for_id(device_id, desired_rate) {
            // Ok means the device has already settled at `desired_rate`.
            Ok(()) => return desired_rate,
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
        let device_id = coreaudio_device_id(device)
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

/// One physical (hardware) format a device's output stream offers, reduced to
/// what choosing a bit depth needs. Plain data so the choice is testable.
// Only the macOS half of `set_max_bit_depth` uses this (and the chooser below);
// elsewhere it is test-only, and Linux CI's `-D warnings` rejects dead code.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PhysFormat {
    pub lpcm: bool,
    pub float: bool,
    /// Exclusive-only variant (kAudioFormatFlagIsNonMixable).
    pub nonmixable: bool,
    pub bits: u32,
    pub channels: u32,
    pub rate_min: f64,
    pub rate_max: f64,
}

/// Bits a format carries EXACTLY: an integer format its width; a float its
/// significand (24 for 32-bit float, 53 for 64-bit).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn usable_bits(f: &PhysFormat) -> u32 {
    match (f.float, f.bits) {
        (false, b) => b,
        (true, 32) => 24,
        (true, 64) => 53,
        (true, b) => b.min(24),
    }
}

/// The linear-PCM format at `rate` with `channels` channels that carries the
/// most bits exactly, or None. Ranked by usable precision, so 32-bit float
/// (24 exact bits) beats 16-bit integer — an integer-only rule truncated
/// 24-bit files there — and a float-only device (MacBook speakers) keeps its
/// float format. On a tie integer wins (the DAC's native format), then the
/// MIXABLE variant: devices like the FiiO KA17 list each integer format twice,
/// and the non-mixable one can be refused before hog mode is held.
/// Formats at other rates or channel counts are never candidates.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn pick_max_bit_format(avail: &[PhysFormat], rate: u32, channels: u32) -> Option<usize> {
    let r = rate as f64;
    avail
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            f.lpcm && f.channels == channels && f.rate_min - 0.5 <= r && r <= f.rate_max + 0.5
        })
        .max_by_key(|(_, f)| (usable_bits(f), !f.float, !f.nonmixable, f.bits))
        .map(|(i, _)| i)
}

/// Exclusive mode: set the device's physical format to the one carrying the
/// most bits exactly at `rate` (see `pick_max_bit_format`), and wait until the
/// device reports it. Returns the bits it carries exactly (None where
/// unsupported or unreadable). Keet
/// otherwise set only the RATE, so a DAC left at 16-bit in Audio MIDI Setup
/// truncated every 24-bit file. Like a rate change, call it with no stream
/// open, and re-apply after every rate switch (a new rate can come with a
/// different default format).
pub fn set_max_bit_depth(device: &cpal::Device, rate: u32) -> Option<u32> {
    #[cfg(target_os = "macos")]
    {
        let id = coreaudio_device_id(device)?;
        let stream = *macos_format::output_stream_ids(id).first()?;
        let current = macos_format::physical_format(stream)?;
        let avail = macos_format::available_physical_formats(stream);
        let reduced: Vec<PhysFormat> = avail.iter().map(macos_format::reduce).collect();
        let Some(i) = pick_max_bit_format(&reduced, rate, current.mChannelsPerFrame) else {
            return Some(usable_bits(&macos_format::reduce_basic(&current)));
        };
        let cur = macos_format::reduce_basic(&current);
        let best = reduced[i];
        if cur.lpcm && cur.float == best.float && cur.bits == best.bits
            && (current.mSampleRate - rate as f64).abs() < 0.5
        {
            return Some(usable_bits(&cur)); // already there
        }
        let mut target = avail[i].mFormat;
        target.mSampleRate = rate as f64;
        macos_format::set_physical_format(stream, &target).ok()?;
        macos_format::physical_format(stream).map(|f| usable_bits(&macos_format::reduce_basic(&f)))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (device, rate);
        None
    }
}

/// A device's physical output format (rate and bit depth together), captured
/// before exclusive mode changes it so quitting can put it back. Keet would
/// otherwise leave the DAC at whatever it last set — 32-bit, last track's rate.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct SavedFormat {
    /// Persistent UID: CoreAudio's numeric ids change across reconnects.
    #[cfg(target_os = "macos")]
    uid: String,
    #[cfg(target_os = "macos")]
    format: coreaudio_sys::AudioStreamBasicDescription,
}

/// Capture `device`'s current physical output format (None where unsupported).
pub fn capture_format(device: &cpal::Device) -> Option<SavedFormat> {
    #[cfg(target_os = "macos")]
    {
        let uid = device.id().ok()?.id().to_string();
        let id = coreaudio_device_id(device)?;
        let stream = *macos_format::output_stream_ids(id).first()?;
        let format = macos_format::physical_format(stream)?;
        Some(SavedFormat { uid, format })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = device;
        None
    }
}

/// Put a captured format back, waiting until the device reports it. Like any
/// format change, call with no stream open. A device that has gone away since
/// the capture is left alone.
pub fn restore_format(saved: &SavedFormat) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let id = macos_audio::find_device_id_by_uid(&saved.uid).ok_or("device no longer present")?;
        let stream = *macos_format::output_stream_ids(id).first().ok_or("no output stream")?;
        macos_format::set_physical_format(stream, &saved.format)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = saved;
        Ok(())
    }
}

/// CoreAudio stream physical-format calls, through the official
/// `coreaudio-sys` bindings (types and constants), unlike the hand-rolled
/// `macos_audio` above.
#[cfg(target_os = "macos")]
mod macos_format {
    use coreaudio_sys as ca;
    use std::ffi::c_void;

    fn addr(selector: u32, scope: u32) -> ca::AudioObjectPropertyAddress {
        ca::AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: ca::kAudioObjectPropertyElementMain,
        }
    }

    fn get_array<T: Copy>(object: u32, selector: u32, scope: u32) -> Vec<T> {
        let a = addr(selector, scope);
        let mut size: u32 = 0;
        let st = unsafe { ca::AudioObjectGetPropertyDataSize(object, &a, 0, std::ptr::null(), &mut size) };
        if st != 0 || size == 0 {
            return Vec::new();
        }
        let n = size as usize / std::mem::size_of::<T>();
        let mut v: Vec<T> = Vec::with_capacity(n);
        let st = unsafe {
            ca::AudioObjectGetPropertyData(object, &a, 0, std::ptr::null(), &mut size, v.as_mut_ptr() as *mut c_void)
        };
        if st != 0 {
            return Vec::new();
        }
        unsafe { v.set_len(size as usize / std::mem::size_of::<T>()) };
        v
    }

    pub fn output_stream_ids(device: u32) -> Vec<u32> {
        get_array::<u32>(device, ca::kAudioDevicePropertyStreams, ca::kAudioObjectPropertyScopeOutput)
    }

    pub fn physical_format(stream: u32) -> Option<ca::AudioStreamBasicDescription> {
        get_array::<ca::AudioStreamBasicDescription>(
            stream, ca::kAudioStreamPropertyPhysicalFormat, ca::kAudioObjectPropertyScopeGlobal,
        )
        .first()
        .copied()
    }

    pub fn available_physical_formats(stream: u32) -> Vec<ca::AudioStreamRangedDescription> {
        get_array(stream, ca::kAudioStreamPropertyAvailablePhysicalFormats, ca::kAudioObjectPropertyScopeGlobal)
    }

    pub fn reduce_basic(f: &ca::AudioStreamBasicDescription) -> super::PhysFormat {
        super::PhysFormat {
            lpcm: f.mFormatID == ca::kAudioFormatLinearPCM,
            float: f.mFormatFlags & ca::kAudioFormatFlagIsFloat != 0,
            nonmixable: f.mFormatFlags & ca::kAudioFormatFlagIsNonMixable != 0,
            bits: f.mBitsPerChannel,
            channels: f.mChannelsPerFrame,
            rate_min: f.mSampleRate,
            rate_max: f.mSampleRate,
        }
    }

    pub fn reduce(r: &ca::AudioStreamRangedDescription) -> super::PhysFormat {
        super::PhysFormat {
            rate_min: r.mSampleRateRange.mMinimum,
            rate_max: r.mSampleRateRange.mMaximum,
            ..reduce_basic(&r.mFormat)
        }
    }

    /// Set the stream's physical format and wait (<= 2 s) until the device
    /// reports it — the same asynchronous settle as a rate change.
    pub fn set_physical_format(stream: u32, f: &ca::AudioStreamBasicDescription) -> Result<(), String> {
        let a = addr(ca::kAudioStreamPropertyPhysicalFormat, ca::kAudioObjectPropertyScopeGlobal);
        let st = unsafe {
            ca::AudioObjectSetPropertyData(
                stream, &a, 0, std::ptr::null(),
                std::mem::size_of::<ca::AudioStreamBasicDescription>() as u32,
                f as *const _ as *const c_void,
            )
        };
        if st != 0 {
            return Err(format!("set physical format: OSStatus {st}"));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(now) = physical_format(stream) {
                if now.mBitsPerChannel == f.mBitsPerChannel && (now.mSampleRate - f.mSampleRate).abs() < 0.5 {
                    return Ok(());
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err("device did not settle at the new format within 2 s".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Read-only description of a device's output formats, for diagnostics.
    #[cfg(test)]
    pub fn describe(device: u32) -> String {
        let mut out = String::new();
        for s in output_stream_ids(device) {
            if let Some(cur) = physical_format(s) {
                out.push_str(&format!("  stream {s}: current {} Hz {}-bit {}ch{} flags=0x{:x}\n",
                    cur.mSampleRate, cur.mBitsPerChannel, cur.mChannelsPerFrame,
                    if cur.mFormatFlags & ca::kAudioFormatFlagIsFloat != 0 { " float" } else { "" },
                    cur.mFormatFlags));
            }
            for r in available_physical_formats(s) {
                let f = reduce(&r);
                out.push_str(&format!("    avail {}-{} Hz {}-bit {}ch{}{} flags=0x{:x} bytes/frame={}\n",
                    f.rate_min, f.rate_max, f.bits, f.channels,
                    if f.float { " float" } else { "" }, if f.lpcm { "" } else { " non-LPCM" },
                    r.mFormat.mFormatFlags, r.mFormat.mBytesPerFrame));
            }
        }
        out
    }
}

/// The audio callback's per-sample output stage: volume, then a clamp so a
/// sample can never exceed full scale at the DAC. At 100% volume the gain is
/// exactly 1.0, making this an exact identity — part of the bit-perfect
/// guarantee, pinned by `chain_tests::bit_perfect_*`.
#[inline]
pub(crate) fn output_sample(s: f32, gain: f32) -> f32 {
    (s * gain).clamp(-1.0, 1.0)
}

/// Fold a stereo frame onto a mono output device. Exact for a mono source
/// (duplicated into both channels): (x + x) * 0.5 == x in floating point.
#[inline]
pub(crate) fn mono_fold(left: f32, right: f32) -> f32 {
    ((left + right) * 0.5).clamp(-1.0, 1.0)
}

/// What an output-stream error calls for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamErrorAction {
    /// A dropout (xrun). Count it; the stream carries on.
    Glitch,
    /// The OS moved a default-device stream to a new output and it is still
    /// playing. Refresh the device handle/label only.
    Rerouted,
    /// The stream is gone or broken: tear down and rebuild (stream_error).
    Rebuild,
}

/// Classify a cpal stream error. Every error used to mean "rebuild", which
/// became a regression with cpal 0.18.2: CoreAudio now reports processor
/// overloads as `Xrun` through this callback, so one CPU spike tore the stream
/// down and restarted the track at the last whole second (ALSA and PipeWire
/// xruns did the same; cpal already recovers from those itself). Players treat
/// xruns as glitches. `DeviceChanged` means CoreAudio rerouted a default-device
/// stream and it keeps playing — except in exclusive mode, where hog mode and
/// the bit-perfect rate belong to the old device and a rebuild is required.
/// Unknown kinds keep the old, safe answer: rebuild.
pub(crate) fn classify_stream_error(kind: cpal::ErrorKind, exclusive: bool) -> StreamErrorAction {
    match kind {
        cpal::ErrorKind::Xrun => StreamErrorAction::Glitch,
        cpal::ErrorKind::DeviceChanged if !exclusive => StreamErrorAction::Rerouted,
        _ => StreamErrorAction::Rebuild,
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
        *config,
        move |data: &mut [f32], _| {
            let paused = state.is_paused();

            // Check if seek happened - drain buffer immediately for instant response
            if state.reset_consumer_counter.swap(false, Ordering::AcqRel) {
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
                    let left = output_sample(slice[src_idx], gain);
                    let right = output_sample(slice[src_idx + 1], gain);

                    if channels == 1 {
                        data[out_idx] = mono_fold(left, right);
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
                let frames_written = out_idx.checked_div(channels).unwrap_or(0);
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
        move |e: cpal::Error| {
            // cpal's error callback can run on audio-thread adjacent paths; avoid
            // I/O here — only atomics. The main loop acts on them.
            use std::sync::atomic::Ordering;
            let exclusive = err_state.exclusive.load(Ordering::Relaxed);
            match classify_stream_error(e.kind(), exclusive) {
                StreamErrorAction::Glitch => {
                    err_state.xrun_count.fetch_add(1, Ordering::Relaxed);
                }
                StreamErrorAction::Rerouted => {
                    err_state.device_rerouted.store(true, Ordering::Relaxed);
                }
                StreamErrorAction::Rebuild => {
                    err_state.stream_error.store(true, Ordering::Relaxed);
                }
            }
        },
        None,
    )?;

    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_name_prefers_an_exact_match_over_a_substring() {
        let devs = vec![
            (10, "External Speakers".to_string()),
            (20, "Speakers".to_string()),
            (30, "USB DAC Pro".to_string()),
        ];
        assert_eq!(pick_device_id(&devs, "speakers"), Some(20), "exact beats first substring hit");
        assert_eq!(pick_device_id(&devs, "dac"), Some(30), "substring still works");
        assert_eq!(pick_device_id(&devs, "headphones"), None);
        // cpal reports some names with trailing whitespace ("FIIO KA17 ").
        assert_eq!(pick_device_id(&devs, "Speakers "), Some(20));
        assert_eq!(pick_device_id(&devs, ""), None, "empty name matches nothing");
    }

    /// Real hardware: every output device cpal lists must resolve to its own
    /// CoreAudio id by name — the lookup every rate switch now depends on.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires audio hardware"]
    fn every_output_device_resolves_to_a_coreaudio_id() {
        use cpal::traits::HostTrait;
        let host = cpal::default_host();
        let mut ids = Vec::new();
        for dev in host.output_devices().expect("output devices") {
            let name = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
            let id = coreaudio_device_id(&dev);
            let rate = id.and_then(|i| macos_audio::get_device_sample_rate_for_id(i).ok());
            let hog = id.map(macos_audio::hog_mode_settable);
            let owner = id.and_then(macos_audio::hog_owner);
            eprintln!("{name:40} id={id:?} rate={rate:?} hog_settable={hog:?} hog_owner={owner:?}");
            assert!(id.is_some(), "{name} did not resolve");
            ids.push(id);
        }
        let def = macos_audio::get_default_device_id();
        eprintln!("system default output id = {def:?}");
        let mut uniq = ids.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), ids.len(), "two devices resolved to the same id");
    }

    #[test]
    fn only_a_lost_stream_triggers_recovery() {
        use cpal::ErrorKind as K;
        // A dropout is a glitch, never a reason to rebuild (cpal 0.18.2 started
        // reporting CoreAudio overloads here; ALSA/PipeWire xruns too).
        assert_eq!(classify_stream_error(K::Xrun, false), StreamErrorAction::Glitch);
        assert_eq!(classify_stream_error(K::Xrun, true), StreamErrorAction::Glitch);
        // A default-device stream rerouted by the OS keeps playing ...
        assert_eq!(classify_stream_error(K::DeviceChanged, false), StreamErrorAction::Rerouted);
        // ... except in exclusive mode: hog mode and the bit-perfect rate were
        // set on the old device, so that stream must be rebuilt.
        assert_eq!(classify_stream_error(K::DeviceChanged, true), StreamErrorAction::Rebuild);
        for k in [K::DeviceNotAvailable, K::StreamInvalidated, K::HostUnavailable, K::BackendError, K::Other] {
            assert_eq!(classify_stream_error(k, false), StreamErrorAction::Rebuild, "{k:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires audio hardware"]
    fn default_device_resolves_to_the_default_coreaudio_id() {
        use cpal::traits::HostTrait;
        let host = cpal::default_host();
        let dev = host.default_output_device().expect("default device");
        let name = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
        let id = coreaudio_device_id(&dev);
        let sys = macos_audio::get_default_device_id();
        eprintln!("cpal default device name={name:?} -> coreaudio id {id:?}; system default id {sys:?}");
        eprintln!("cpal device id() = {:?}", cpal::traits::DeviceTrait::id(&dev));
        assert_eq!(id, sys, "the default device resolved to a different CoreAudio device");
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires audio hardware"]
    fn exclusive_devices_are_pinned_and_found_by_uid() {
        use cpal::traits::HostTrait;
        let host = cpal::default_host();
        // Every device resolves through its UID alone — no name fallback.
        for dev in host.output_devices().expect("devices") {
            let uid = dev.id().expect("id");
            let by_uid = macos_audio::find_device_id_by_uid(uid.id());
            eprintln!("{:40} uid-> {by_uid:?}", dev.description().map(|d| d.name().to_string()).unwrap_or_default());
            assert!(by_uid.is_some(), "{} not found by UID", uid.id());
        }
        // Pinning the default yields the same hardware, as a pinned device.
        let def = host.default_output_device().expect("default");
        let pinned = pin_device(&host, &def).expect("default device must be pinnable");
        assert_eq!(pinned.id().unwrap(), def.id().unwrap());
        assert_eq!(coreaudio_device_id(&pinned), macos_audio::get_default_device_id());
    }

    #[test]
    fn exclusive_mode_picks_the_most_precise_format_at_the_rate() {
        let f = |bits, float, rate_min, rate_max| PhysFormat {
            lpcm: true, float, nonmixable: false, bits, channels: 2, rate_min, rate_max,
        };
        let avail = [
            f(16, false, 44_100.0, 44_100.0),
            f(24, false, 44_100.0, 44_100.0),
            f(32, true, 44_100.0, 44_100.0),   // float: 24 exact bits, ties 24-bit int
            f(32, false, 96_000.0, 96_000.0),  // deeper, but another rate
            f(24, false, 96_000.0, 96_000.0),
        ];
        assert_eq!(pick_max_bit_format(&avail, 44_100, 2), Some(1), "24-bit int at 44.1k");
        assert_eq!(pick_max_bit_format(&avail, 96_000, 2), Some(3), "32-bit int at 96k");
        assert_eq!(pick_max_bit_format(&avail, 48_000, 2), None, "no format at that rate");
        assert_eq!(pick_max_bit_format(&avail, 44_100, 6), None, "channel count must match");
        // A ranged format covering the rate counts.
        let ranged = [f(24, false, 8_000.0, 192_000.0)];
        assert_eq!(pick_max_bit_format(&ranged, 88_200, 2), Some(0));
        // 32-bit float carries 24 bits exactly: it beats 16-bit integer (the
        // integer-only rule truncated 24-bit files to 16 there) ...
        let int16_or_float = [f(16, false, 48_000.0, 48_000.0), f(32, true, 48_000.0, 48_000.0)];
        assert_eq!(pick_max_bit_format(&int16_or_float, 48_000, 2), Some(1));
        // ... ties with 24-bit integer, where integer (the DAC's native) wins ...
        let tie = [f(32, true, 48_000.0, 48_000.0), f(24, false, 48_000.0, 48_000.0)];
        assert_eq!(pick_max_bit_format(&tie, 48_000, 2), Some(1));
        // ... and a float-only device (MacBook speakers) keeps its float format.
        let float_only = [f(32, true, 96_000.0, 96_000.0)];
        assert_eq!(pick_max_bit_format(&float_only, 96_000, 2), Some(0));
        // FiiO KA17: every integer format is listed twice, mixable and
        // NON-MIXABLE (exclusive-only). Same bits; the mixable one must win —
        // it works before hog mode is held (the startup call runs before), and
        // max_by_key returned the LAST of equals, i.e. the non-mixable one.
        let nm = |bits| PhysFormat { nonmixable: true, ..f(bits, false, 44_100.0, 44_100.0) };
        let ka17 = [f(32, false, 44_100.0, 44_100.0), f(16, false, 44_100.0, 44_100.0), nm(32), nm(16),
                    f(32, true, 44_100.0, 44_100.0)];
        assert_eq!(pick_max_bit_format(&ka17, 44_100, 2), Some(0), "32-bit integer, mixable");
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires audio hardware"]
    fn list_physical_formats_of_every_output_device() {
        use cpal::traits::HostTrait;
        for dev in cpal::default_host().output_devices().expect("devices") {
            let name = dev.description().map(|d| d.name().to_string()).unwrap_or_default();
            if let Some(id) = coreaudio_device_id(&dev) {
                eprintln!("{name}:\n{}", macos_format::describe(id));
            }
        }
    }

    /// Real hardware, and it CHANGES the device's format — then restores it.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "changes a DAC's format (restored afterwards)"]
    fn set_max_bit_depth_on_the_ka17_then_restore() {
        use cpal::traits::HostTrait;
        let dev = cpal::default_host()
            .output_devices()
            .expect("devices")
            .find(|d| d.description().map(|x| x.name().trim() == "FIIO KA17").unwrap_or(false))
            .expect("FIIO KA17 not connected");
        let id = coreaudio_device_id(&dev).expect("id");
        let stream = macos_format::output_stream_ids(id)[0];
        let before = macos_format::physical_format(stream).expect("format");
        let rate = before.mSampleRate as u32;
        eprintln!("before: {} Hz {}-bit flags=0x{:x}", rate, before.mBitsPerChannel, before.mFormatFlags);

        let got = set_max_bit_depth(&dev, rate);
        let after = macos_format::physical_format(stream).expect("format");
        eprintln!("set_max_bit_depth -> {got:?}; device now {} Hz {}-bit flags=0x{:x}",
            after.mSampleRate, after.mBitsPerChannel, after.mFormatFlags);

        let restored = macos_format::set_physical_format(stream, &before);
        let now = macos_format::physical_format(stream).expect("format");
        eprintln!("restored: {:?} -> {} Hz {}-bit flags=0x{:x}",
            restored, now.mSampleRate, now.mBitsPerChannel, now.mFormatFlags);

        assert_eq!(got, Some(32), "KA17 offers 32-bit integer");
        assert_eq!(after.mBitsPerChannel, 32);
        assert_eq!(after.mFormatFlags & coreaudio_sys::kAudioFormatFlagIsNonMixable, 0, "mixable variant");
        assert_eq!(after.mSampleRate as u32, rate, "rate must not change");
        assert!(restored.is_ok(), "restore failed");
    }

    /// Real hardware: capture, change, restore — the quit-time round trip.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "changes a DAC's format (restored afterwards)"]
    fn exclusive_mode_puts_the_ka17_back_as_it_found_it() {
        use cpal::traits::HostTrait;
        let dev = cpal::default_host()
            .output_devices()
            .expect("devices")
            .find(|d| d.description().map(|x| x.name().trim() == "FIIO KA17").unwrap_or(false))
            .expect("FIIO KA17 not connected");
        let id = coreaudio_device_id(&dev).expect("id");
        let stream = macos_format::output_stream_ids(id)[0];
        let saved = capture_format(&dev).expect("capture");
        let before = macos_format::physical_format(stream).unwrap();
        // What exclusive mode does: another rate, the deepest format.
        let other = if before.mSampleRate as u32 == 96_000 { 44_100 } else { 96_000 };
        assert_eq!(set_output_sample_rate(other, before.mSampleRate as u32, &dev), other);
        set_max_bit_depth(&dev, other);
        let during = macos_format::physical_format(stream).unwrap();
        restore_format(&saved).expect("restore");
        let after = macos_format::physical_format(stream).unwrap();
        eprintln!("before {} Hz {}-bit | during {} Hz {}-bit | after {} Hz {}-bit",
            before.mSampleRate, before.mBitsPerChannel, during.mSampleRate,
            during.mBitsPerChannel, after.mSampleRate, after.mBitsPerChannel);
        assert_eq!(during.mSampleRate as u32, other, "exclusive mode moved the rate");
        assert_eq!(after.mSampleRate, before.mSampleRate, "rate restored");
        assert_eq!(after.mBitsPerChannel, before.mBitsPerChannel, "bit depth restored");
        assert_eq!(after.mFormatFlags, before.mFormatFlags, "format flags restored");
    }
}
