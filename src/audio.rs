use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use cpal::traits::DeviceTrait;
use cpal::traits::HostTrait;
use cpal::traits::StreamTrait;
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
/// Used for `--device` on every platform and for CoreAudio ids on macOS.
fn pick_device_id<T: Copy>(devices: &[(T, String)], wanted: &str) -> Option<T> {
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
/// One device as the compact listing sees it (no device is opened for it).
pub(crate) struct ListedDevice {
    pub id: String,
    pub name: String,
    pub default: bool,
}

/// Sound-server routes worth showing on Linux; other card-less ALSA PCMs are
/// plugins (null, samplerate, upmix, …) that no one picks by hand.
const ALSA_ROUTES: [&str; 5] = ["default", "sysdefault", "pipewire", "pulse", "jack"];

/// The `--list-devices` lines. Linux (`alsa`): every ALSA name of a card —
/// hw:, plughw:, front:, surround51:, iec958:, dmix: … — is its own "device",
/// so a laptop with a USB DAC listed 40+ entries. Instead: the sound-server
/// routes (normal mode), then one line per card output by its raw hw: id (what
/// --exclusive needs); cpal's numeric duplicates (hw:CARD=1 for hw:CARD=KA17)
/// are merged, keeping the named form. Elsewhere: one line per device.
pub(crate) fn compact_device_list(devs: &[ListedDevice], alsa: bool) -> Vec<String> {
    let mark = |d: &ListedDevice| if d.default { "  (default)" } else { "" };
    let mut out = Vec::new();
    if alsa {
        let routes: Vec<&ListedDevice> = devs.iter().filter(|d| ALSA_ROUTES.contains(&d.id.as_str())).collect();
        // hw: outputs, one per (card name, device), named CARD preferred.
        let mut hw: Vec<&ListedDevice> = Vec::new();
        for d in devs.iter().filter(|d| d.id.starts_with("hw:")) {
            let dev_of = |x: &ListedDevice| hw_pcm_id_for(&x.id).and_then(|h| h.rsplit_once("DEV=").map(|(_, v)| v.to_string()));
            let numeric = |x: &ListedDevice| {
                hw_pcm_id_for(&x.id).is_some_and(|h| h.trim_start_matches("hw:CARD=").split(',').next().is_some_and(|c| c.chars().all(|ch| ch.is_ascii_digit())))
            };
            match hw.iter().position(|e| e.name == d.name && dev_of(e) == dev_of(d)) {
                Some(i) if numeric(hw[i]) && !numeric(d) => hw[i] = d,
                Some(_) => {}
                None => hw.push(d),
            }
        }
        if !routes.is_empty() || !hw.is_empty() {
            out.push("Output devices (--list-devices --verbose shows every device and its formats)".to_string());
            if !routes.is_empty() {
                out.push(String::new());
                out.push("  Normal mode:".to_string());
                let w = routes.iter().map(|d| d.id.len()).max().unwrap_or(0);
                for d in routes {
                    out.push(format!("    {:<w$}   {}{}", d.id, d.name, mark(d)));
                }
            }
            if !hw.is_empty() {
                out.push(String::new());
                out.push("  --exclusive (pass the id to --device):".to_string());
                let w = hw.iter().map(|d| d.id.len()).max().unwrap_or(0);
                for d in hw {
                    out.push(format!("    {:<w$}   {}{}", d.id, d.name, mark(d)));
                }
            }
            return out;
        }
    }
    out.push("Output devices (--list-devices --verbose shows each device's formats):".to_string());
    for (i, d) in devs.iter().enumerate() {
        out.push(format!("  {}. {}{}", i + 1, d.name, mark(d)));
    }
    out
}

/// `--list-devices`: the compact list, or with `verbose` every device with
/// its id, default format and supported formats (for troubleshooting).
pub fn list_output_devices(host: &cpal::Host, verbose: bool) {
    if !verbose {
        let default_id = host.default_output_device().and_then(|d| d.id().ok()).map(|i| i.id().to_string());
        match host.output_devices() {
            Ok(devices) => {
                let listed: Vec<ListedDevice> = devices
                    .map(|d| {
                        let id = d.id().map(|i| i.id().to_string()).unwrap_or_default();
                        let name = d.description().map(|x| x.name().trim().to_string()).unwrap_or_else(|_| "Unknown".into());
                        ListedDevice { default: default_id.as_deref() == Some(id.as_str()), id, name }
                    })
                    .collect();
                for line in compact_device_list(&listed, cfg!(target_os = "linux")) {
                    println!("{line}");
                }
            }
            Err(e) => eprintln!("Cannot enumerate devices: {}", e),
        }
        return;
    }
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
                // The id is what --device matches exactly; on Linux it is the
                // ALSA PCM (hw:CARD=…,DEV=… is the one for exclusive mode).
                if let Ok(id) = device.id() {
                    println!("       id: {}", id.id());
                }
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
                // cpal lists only the shared-mode format; what exclusive mode
                // (and so per-track rate switching) can do comes from WASAPI.
                #[cfg(target_os = "windows")]
                if let (Ok(id), Ok(c)) = (device.id(), device.default_output_config()) {
                    for line in crate::wasapi_out::exclusive_report(id.id(), c.channels()) {
                        println!("       {line}");
                    }
                }
            }
        }
        Err(e) => eprintln!("Cannot enumerate devices: {}", e),
    }
}

/// Find an output device: an exact device id first (on Linux the ALSA PCM
/// name, e.g. `hw:CARD=0,DEV=0` — several PCMs of one card share its friendly
/// name, so the id is the unambiguous handle), then a case-insensitive
/// substring of the name.
pub fn find_device_by_name(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    let devices: Vec<cpal::Device> = host.output_devices().ok()?.collect();
    if let Some(d) = devices.iter().find(|d| d.id().is_ok_and(|i| i.id() == name)) {
        return Some(d.clone());
    }
    // Then by name, exact before substring (see pick_device_id): taking the
    // first substring hit let "Speakers" pick "External Speakers".
    let named: Vec<(usize, String)> = devices
        .iter()
        .enumerate()
        .filter_map(|(i, d)| d.description().ok().map(|desc| (i, desc.name().to_string())))
        .collect();
    pick_device_id(&named, name).map(|i| devices[i].clone())
}

/// Query the maximum sample rate supported by a device
// Unused on Windows, where exclusive-mode rates come from WASAPI (probe_rate_caps).
#[cfg_attr(target_os = "windows", allow(dead_code))]
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
///
/// None when the device's rates could not be read (e.g. a Linux hw: device
/// already open — it admits one client): unknown capabilities must not read
/// as "only 48 kHz". Without caps, `exclusive_target_rate` falls back to the
/// requested rate, and the switch itself (made with no stream open) checks
/// the device lists it.
pub fn probe_rate_caps(device: &cpal::Device) -> Option<crate::state::RateCaps> {
    // Windows exclusive mode opens the device itself: its rates are the ones
    // WASAPI exclusive accepts, not the shared-mode mixer's single format
    // that cpal reports.
    #[cfg(target_os = "windows")]
    {
        let id = device.id().ok()?.id().to_string();
        let ch = device.default_output_config().map(|c| c.channels()).unwrap_or(2);
        let rates = crate::wasapi_out::supported_rates(&id, ch);
        let max = rates.iter().copied().max()?;
        Some(crate::state::RateCaps { ranges: rates.iter().map(|&r| (r, r)).collect(), max, fixed: false, any: false })
    }
    #[cfg(not(target_os = "windows"))]
    {
    let ranges: Vec<(u32, u32)> = device
        .supported_output_configs()
        .map(|configs| configs.map(|c| (c.min_sample_rate(), c.max_sample_rate())).collect())
        .unwrap_or_default();
    if ranges.is_empty() {
        return None;
    }
    let max = max_supported_rate(device);
    #[cfg(target_os = "macos")]
    let caps = crate::state::RateCaps {
        ranges,
        max,
        fixed: coreaudio_device_id(device).is_some_and(macos_audio::is_bluetooth_device_by_id),
        any: false,
    };
    // Linux exclusive mode opens a raw hw: device, which takes exactly the
    // rates it lists (reopening at one IS the switch).
    #[cfg(target_os = "linux")]
    let caps = crate::state::RateCaps { ranges, max, fixed: false, any: false };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let caps = crate::state::RateCaps { ranges, max, fixed: false, any: false };
    Some(caps)
    }
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
        // Only exclusive mode calls this. WASAPI exclusive opens the device at
        // the stream's rate itself — reopening IS the switch — so the answer
        // is whether the device accepts `desired_rate` exclusively. (Shared
        // mode never switches: virtual devices like SteelSeries Sonar report
        // capabilities they do not have.)
        let id = device.id().map(|i| i.id().to_string()).unwrap_or_default();
        let ch = device.default_output_config().map(|c| c.channels()).unwrap_or(2);
        return if crate::wasapi_out::best_layout(&id, desired_rate, ch).is_some() {
            desired_rate
        } else {
            current_rate
        };
    }

    #[cfg(target_os = "linux")]
    {
        // Only exclusive mode calls this, on a raw hw: device: there is no
        // device-wide rate to set — the stream is reopened at the new rate —
        // so the answer is simply whether the device lists it.
        let supported = device
            .supported_output_configs()
            .map(|mut cs| cs.any(|c| c.min_sample_rate() <= desired_rate && desired_rate <= c.max_sample_rate()))
            .unwrap_or(false);
        return if supported { desired_rate } else { current_rate };
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
/// ALSA: the raw `hw:` PCM behind any PCM that names a card — `front:`,
/// `plughw:`, `sysdefault:`, `dmix:` … of the same card and device. Raw `hw:`
/// is the one ALSA PCM that plays exactly what it is sent (no plug
/// conversion, no mixing). None for PCMs that name no card: `default`,
/// `pipewire`, `pulse` route through a sound server, so there is nothing to
/// own. Plain string logic, compiled and tested on every platform.
pub(crate) fn hw_pcm_id_for(pcm_id: &str) -> Option<String> {
    let (_, rest) = pcm_id.split_once(':')?;
    let mut card = None;
    let mut dev = "0".to_string();
    let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
    for (i, part) in parts.iter().enumerate() {
        match part.split_once('=') {
            Some(("CARD", v)) => card = Some(v.to_string()),
            Some(("DEV", v)) => dev = v.to_string(),
            Some(_) => {}
            // Positional form: hw:1,0 (card, device).
            None if i == 0 => card = Some(part.to_string()),
            None if i == 1 => dev = part.to_string(),
            None => {}
        }
    }
    Some(format!("hw:CARD={},DEV={}", card?, dev))
}

/// One sample format a device lists, for choosing an output format.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FormatRange {
    pub format: cpal::SampleFormat,
    pub channels: u16,
    pub min: u32,
    pub max: u32,
}

/// The format a raw device should be opened with at `rate` and `channels`:
/// the one carrying the most bits exactly (the same ranking as macOS's
/// physical formats — see `pick_max_bit_format`). Returns it with its exact
/// bit count. None when the device lists nothing at that rate/channels.
pub(crate) fn choose_output_format(
    configs: &[FormatRange],
    rate: u32,
    channels: u16,
) -> Option<(cpal::SampleFormat, u32)> {
    use cpal::SampleFormat as F;
    let candidates: Vec<(F, PhysFormat)> = configs
        .iter()
        .filter_map(|c| {
            let (float, bits) = match c.format {
                F::I16 => (false, 16),
                F::I24 => (false, 24),
                F::I32 => (false, 32),
                F::F32 => (true, 32),
                F::F64 => (true, 64),
                _ => return None,
            };
            Some((c.format, PhysFormat {
                lpcm: true, float, nonmixable: false, bits,
                channels: c.channels as u32,
                rate_min: c.min as f64, rate_max: c.max as f64,
            }))
        })
        .collect();
    let phys: Vec<PhysFormat> = candidates.iter().map(|(_, p)| *p).collect();
    let i = pick_max_bit_format(&phys, rate, channels as u32)?;
    Some((candidates[i].0, usable_bits(&phys[i])))
}

/// The device exclusive mode should actually use, or why it cannot run.
/// macOS: the same device, pinned (see `pin_device`). Linux: the card's raw
/// `hw:` device (a `default`/`pipewire` route has no card to own). Elsewhere
/// exclusive mode is not available yet.
pub fn prepare_exclusive_device(host: &cpal::Host, device: &cpal::Device) -> Result<cpal::Device, String> {
    if cfg!(target_os = "macos") {
        return Ok(pin_device(host, device).unwrap_or_else(|| device.clone()));
    }
    if cfg!(target_os = "linux") {
        let id = device.id().map_err(|e| e.to_string())?;
        let hw = hw_pcm_id_for(id.id()).ok_or_else(|| {
            format!(
                "exclusive mode needs a hardware device, and '{}' is a sound-server route. \
                 Pass --device with a card's hw: id (see --list-devices), e.g. --device hw:CARD=0,DEV=0",
                id.id()
            )
        })?;
        return host
            .output_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.id().ok().is_some_and(|i| i.id() == hw))
            .ok_or_else(|| format!("no raw hardware device {hw} to open exclusively"));
    }
    if cfg!(target_os = "windows") {
        // WASAPI exclusive binds to this endpoint by id: already pinned.
        return Ok(device.clone());
    }
    Err("exclusive mode is not available on this platform yet".into())
}

/// Sample format (and the bits it carries exactly) to open the stream with.
/// Float everywhere, except Linux exclusive mode, where a raw `hw:` device
/// takes an integer format as-is. (bits 0 = not applicable / unknown.)
pub fn output_format(device: &cpal::Device, rate: u32, channels: u16, exclusive: bool) -> (cpal::SampleFormat, u32) {
    if !(exclusive && cfg!(target_os = "linux")) {
        return (cpal::SampleFormat::F32, 0);
    }
    let ranges: Vec<FormatRange> = device
        .supported_output_configs()
        .map(|cs| {
            cs.map(|c| FormatRange {
                format: c.sample_format(),
                channels: c.channels(),
                min: c.min_sample_rate(),
                max: c.max_sample_rate(),
            })
            .collect()
        })
        .unwrap_or_default();
    choose_output_format(&ranges, rate, channels).unwrap_or((cpal::SampleFormat::F32, 0))
}

/// A device held by another program, reported by an output Keet opens itself
/// (WASAPI exclusive: AUDCLNT_E_DEVICE_IN_USE).
#[derive(Debug)]
pub struct DeviceBusyError(pub String);

impl std::fmt::Display for DeviceBusyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DeviceBusyError {}

/// Whether a stream-build failure means the device is held by another
/// program: cpal's DeviceBusy (ALSA EBUSY/EAGAIN — `build_stream` boxes
/// cpal's own error, which keeps the kind recoverable) or `DeviceBusyError`.
pub fn is_device_busy(e: &(dyn std::error::Error + 'static)) -> bool {
    e.downcast_ref::<cpal::Error>().is_some_and(|c| c.kind() == cpal::ErrorKind::DeviceBusy)
        || e.downcast_ref::<DeviceBusyError>().is_some()
}

/// The output stream, whichever backend drives it: a cpal stream, or on
/// Windows in exclusive mode Keet's own WASAPI exclusive stream. main only
/// plays, pauses and drops it.
pub enum Output {
    Cpal(Stream),
    #[cfg(target_os = "windows")]
    Wasapi(crate::wasapi_out::WasapiOutput),
}

impl Output {
    pub fn play(&self) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Output::Cpal(s) => Ok(s.play()?),
            // Plays from the moment it opens.
            #[cfg(target_os = "windows")]
            Output::Wasapi(_) => Ok(()),
        }
    }

    pub fn pause(&self) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Output::Cpal(s) => Ok(s.pause()?),
            #[cfg(target_os = "windows")]
            Output::Wasapi(w) => {
                w.stop();
                Ok(())
            }
        }
    }
}

/// Take exclusive ownership of the output device. `Ok(Some(id))`: macOS hog
/// mode, held until `release_exclusive_mode(id)`. `Ok(None)`: exclusive by
/// nature — a Linux raw `hw:` device admits one client while open.
pub fn set_exclusive_mode(device: &cpal::Device) -> Result<Option<u32>, String> {
    #[cfg(target_os = "macos")]
    {
        let device_id = coreaudio_device_id(device)
            .ok_or_else(|| "Could not find CoreAudio device ID".to_string())?;

        macos_audio::set_hog_mode(device_id)?;
        Ok(Some(device_id))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let is_hw = device.id().is_ok_and(|i| i.id().starts_with("hw:"));
        if (cfg!(target_os = "linux") && is_hw) || cfg!(target_os = "windows") {
            return Ok(None);
        }
        Err("exclusive mode is not available on this device or platform".to_string())
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

/// Everything exclusive mode changed on output devices, put back on request
/// or, failing that, on drop. Drop covers the exits that skip the quit key's
/// orderly path: an error returned from `main` (a device busy at startup), a
/// panic on the main thread. Declare it BEFORE the stream: locals drop in
/// reverse order, so the stream is gone first — a format change under a live
/// stream is what StreamInvalidated reports, and releasing hog mode under
/// running IO buzzed.
///
/// It holds every device whose format was changed, not just the first: after
/// a recovery onto another device, that one's format used to be changed and
/// never put back.
#[derive(Default)]
pub struct DeviceRestore {
    formats: Vec<SavedFormat>,
    /// The hog-mode device to release (macOS).
    pub hog: Option<u32>,
}

impl DeviceRestore {
    /// Remember `device`'s format before exclusive mode first changes it. A
    /// device already captured keeps its ORIGINAL format.
    pub fn capture(&mut self, device: &cpal::Device) {
        let Some(saved) = capture_format(device) else { return };
        if !self.formats.iter().any(|f| f.same_device(&saved)) {
            self.formats.push(saved);
        }
    }

    /// Anything to put back.
    pub fn pending(&self) -> bool {
        !self.formats.is_empty() || self.hog.is_some()
    }

    /// Restore every captured format (devices that have gone away are left
    /// alone), then release hog mode — while it is still held, so no other app
    /// sees the in-between state. Call with no stream open.
    pub fn restore(&mut self) {
        for saved in self.formats.drain(..) {
            let _ = restore_format(&saved);
        }
        if let Some(id) = self.hog.take() {
            release_exclusive_mode(id);
        }
    }
}

impl Drop for DeviceRestore {
    fn drop(&mut self) {
        self.restore();
    }
}

impl SavedFormat {
    fn same_device(&self, other: &SavedFormat) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.uid == other.uid
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = other;
            true
        }
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

/// Sample types the audio output can deliver. `from_f32` is EXACT for any
/// source at most the target's depth: decode hands over `v / 2^(bits-1)`, and
/// multiplying back by the target's full scale lands on `v` (shifted into the
/// wider format) with nothing to round. That is what lets an integer device —
/// an ALSA `hw:` device, WASAPI exclusive — receive the file's own samples.
pub(crate) trait OutputSample: cpal::SizedSample + Send + 'static {
    fn from_f32(x: f32) -> Self;
}

impl OutputSample for f32 {
    fn from_f32(x: f32) -> f32 {
        x
    }
}

/// Scale to a `bits`-wide signed integer: round (exact for sources no deeper
/// than `bits`), clamp (+1.0 is full scale's one step past the top), NaN -> 0.
fn scale_to_int(x: f32, bits: u32) -> i64 {
    let full = (1i64 << (bits - 1)) as f64;
    (x as f64 * full).round().clamp(-full, full - 1.0) as i64
}

impl OutputSample for i16 {
    fn from_f32(x: f32) -> i16 {
        scale_to_int(x, 16) as i16
    }
}

impl OutputSample for cpal::I24 {
    fn from_f32(x: f32) -> cpal::I24 {
        cpal::I24::new_unchecked(scale_to_int(x, 24) as i32)
    }
}

impl OutputSample for i32 {
    fn from_f32(x: f32) -> i32 {
        scale_to_int(x, 32) as i32
    }
}

/// Pack full-scale i32 samples (from `OutputRenderer::render::<i32>`) into a
/// device buffer laid out as `store` bits per sample, `valid` of them used —
/// the WASAPI exclusive layouts: 32/32, 24-in-32 (left-justified, unused low
/// byte zero), packed 24 (three bytes), 16. Exact for any source no deeper
/// than `valid`: a 24-bit sample arrives as `v << 8` and every layout of 24+
/// bits carries it unchanged. `out` holds `samples.len() * store / 8` bytes.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn pack_samples(samples: &[i32], store: u16, valid: u16, out: &mut [u8]) {
    match (store, valid) {
        (32, 32) => {
            for (s, o) in samples.iter().zip(out.as_chunks_mut::<4>().0) {
                *o = s.to_le_bytes();
            }
        }
        (32, _) => {
            for (s, o) in samples.iter().zip(out.as_chunks_mut::<4>().0) {
                *o = (s & !0xFF).to_le_bytes();
            }
        }
        (24, _) => {
            for (s, o) in samples.iter().zip(out.as_chunks_mut::<3>().0) {
                o.copy_from_slice(&s.to_le_bytes()[1..4]);
            }
        }
        _ => {
            // 16-bit: round (exact for 16-bit sources, whose low 16 bits are 0).
            for (s, o) in samples.iter().zip(out.as_chunks_mut::<2>().0) {
                let v = ((*s as i64 + 0x8000) >> 16).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
                *o = v.to_le_bytes();
            }
        }
    }
}

/// Scratch size for integer rendering, in samples: a WASAPI period of 2048
/// frames at 8 channels. A larger callback is rendered in pieces.
const RENDER_SCRATCH: usize = 16_384;

/// Everything the audio callback does, independent of the backend and sample
/// format: drain requests, reading the stereo ring, volume and clamp, fanning
/// out to the device's channel count, position counting and the viz tap.
/// cpal's float callback calls `render_f32` directly; integer outputs call
/// `render`. Lock-free and allocation-free once built.
pub(crate) struct OutputRenderer {
    consumer: Consumer<f32>,
    viz_producer: Producer<f32>,
    state: Arc<PlayerState>,
    channels: usize,
    scratch: Vec<f32>,
}

impl OutputRenderer {
    pub(crate) fn new(
        consumer: Consumer<f32>,
        viz_producer: Producer<f32>,
        state: Arc<PlayerState>,
        channels: usize,
    ) -> Self {
        Self { consumer, viz_producer, state, channels, scratch: vec![0.0; RENDER_SCRATCH] }
    }

    pub(crate) fn render_f32(&mut self, data: &mut [f32]) {
        let channels = self.channels;
        let paused = self.state.is_paused();

        // Check if seek happened - drain buffer immediately for instant response
        if self.state.reset_consumer_counter.swap(false, Ordering::AcqRel) {
            // Drain all buffered samples instantly
            let to_drain = self.consumer.slots();
            if to_drain > 0 {
                if let Ok(chunk) = self.consumer.read_chunk(to_drain) {
                    chunk.commit_all(); // Discard without processing
                }
            }
            data.fill(0.0);
            return;
        }

        // Use chunk reads for efficiency
        let available = self.consumer.slots();

        // Update buffer level so main thread can detect track end
        self.state.buffer_level.store(available, Ordering::Relaxed);

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

        if let Ok(chunk) = self.consumer.read_chunk(samples_to_read) {
            let (first, second) = chunk.as_slices();
            let gain = self.state.volume_gain();

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
                // flags clipping (self.state.clipping) — can't scale there since
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
            self.state.samples_played.fetch_add(consumed_frames as u64, Ordering::Relaxed);

            // Tap played stereo samples into viz buffer (best-effort, drop if full)
            // Pre-fader mode: undo volume gain so viz shows raw signal levels
            let frames_written = out_idx.checked_div(channels).unwrap_or(0);
            let viz_samples = frames_written * 2; // stereo
            let pre_fader = self.state.is_pre_fader();
            let viz_scale = if pre_fader && gain > 0.0 { 1.0 / gain } else { 1.0 };
            if viz_samples > 0 {
                if channels == 2 && viz_scale == 1.0 && viz_samples <= data.len() {
                    // Fast path: post-fader stereo bulk copy
                    let _ = self.viz_producer.push_partial_slice(&data[..viz_samples]);
                } else if self.viz_producer.slots() >= viz_samples {
                    if let Ok(mut vchunk) = self.viz_producer.write_chunk(viz_samples) {
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
    }

    /// Render into any output sample type: through `render_f32` into the
    /// preallocated scratch, then an exact conversion per sample.
    pub(crate) fn render<T: OutputSample>(&mut self, data: &mut [T]) {
        let ch = self.channels.max(1);
        let step = (RENDER_SCRATCH / ch).max(1) * ch; // whole frames only
        // Swap the scratch out so render_f32 can borrow self; Vec::new() does
        // not allocate, and the buffer is put back below.
        let mut scratch = std::mem::take(&mut self.scratch);
        if scratch.len() < step {
            scratch.resize(step, 0.0); // only for a device wider than the scratch
        }
        for out in data.chunks_mut(step) {
            let buf = &mut scratch[..out.len()];
            self.render_f32(buf);
            for (o, &x) in out.iter_mut().zip(buf.iter()) {
                *o = T::from_f32(x);
            }
        }
        self.scratch = scratch;
    }
}

/// Build the output stream. `format` is the sample format handed to the device:
/// F32 everywhere today (CoreAudio and shared-mode mixers convert themselves);
/// the integer formats are for outputs that talk to hardware directly.
pub fn build_stream(
    device: &cpal::Device,
    config: &StreamConfig,
    format: cpal::SampleFormat,
    consumer: Consumer<f32>,
    viz_producer: Producer<f32>,
    state: Arc<PlayerState>,
) -> Result<Stream, Box<dyn std::error::Error>> {
    let channels = config.channels as usize;
    let err_state = Arc::clone(&state);
    let mut r = OutputRenderer::new(consumer, viz_producer, state, channels);
    let on_error = move |e: cpal::Error| {
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
            };
    let stream = match format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            *config, move |d: &mut [f32], _| r.render_f32(d), on_error, None)?,
        cpal::SampleFormat::I32 => device.build_output_stream(
            *config, move |d: &mut [i32], _| r.render(d), on_error, None)?,
        cpal::SampleFormat::I24 => device.build_output_stream(
            *config, move |d: &mut [cpal::I24], _| r.render(d), on_error, None)?,
        cpal::SampleFormat::I16 => device.build_output_stream(
            *config, move |d: &mut [i16], _| r.render(d), on_error, None)?,
        other => return Err(format!("unsupported output sample format {other:?}").into()),
    };

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

    #[test]
    fn integer_output_is_exact_for_every_source_depth() {
        // x = v / 2^(src-1) is what decode delivers for a src-bit integer v.
        // Into any integer format at least as deep, the conversion must land
        // on v exactly (shifted to the wider format) — bit-perfect needs it.
        let ints16 = [i16::MIN as i32, -12_345, -1, 0, 1, 12_345, i16::MAX as i32];
        for v in ints16 {
            let x = v as f32 / 32_768.0;
            assert_eq!(<i16 as OutputSample>::from_f32(x) as i32, v, "16 -> i16");
            assert_eq!(<cpal::I24 as OutputSample>::from_f32(x).inner(), v << 8, "16 -> i24");
            assert_eq!(<i32 as OutputSample>::from_f32(x), v << 16, "16 -> i32");
        }
        let ints24 = [-8_388_608, -5_000_001, -1, 0, 1, 5_000_001, 8_388_607];
        for v in ints24 {
            let x = v as f32 / 8_388_608.0;
            assert_eq!(<cpal::I24 as OutputSample>::from_f32(x).inner(), v, "24 -> i24");
            assert_eq!(<i32 as OutputSample>::from_f32(x), v << 8, "24 -> i32");
        }
        // Full scale (+1.0, reachable from float sources / DSP) clamps to max.
        assert_eq!(<i16 as OutputSample>::from_f32(1.0), i16::MAX);
        assert_eq!(<i32 as OutputSample>::from_f32(1.0), i32::MAX);
        assert_eq!(<cpal::I24 as OutputSample>::from_f32(1.0).inner(), 8_388_607);
        assert_eq!(<i16 as OutputSample>::from_f32(-1.0), i16::MIN);
    }

    #[test]
    fn integer_render_is_the_float_render_converted() {
        // The integer path must deliver exactly what the float path does —
        // same ring handling, same gain — only converted.
        let make = |chans: usize| {
            let st = Arc::new(PlayerState::new());
            st.volume.store(100, Ordering::Relaxed);
            let (mut p, c) = rtrb::RingBuffer::<f32>::new(4096);
            let (vp, _vc) = rtrb::RingBuffer::<f32>::new(4096);
            let src: Vec<f32> = (0i32..1024).map(|i| ((i * 7919 % 16_384) - 8_192) as f32 / 8_192.0 * 0.9).collect();
            p.push_entire_slice(&src).unwrap();
            OutputRenderer::new(c, vp, st, chans)
        };
        for chans in [1usize, 2, 4] {
            let mut a = make(chans);
            let mut b = make(chans);
            let mut f = vec![0.0f32; 300 * chans];
            let mut n = vec![0i32; 300 * chans];
            a.render_f32(&mut f);
            b.render(&mut n);
            for (i, (&x, &y)) in f.iter().zip(&n).enumerate() {
                assert_eq!(y, <i32 as OutputSample>::from_f32(x), "{chans}ch sample {i}");
            }
        }
    }

    #[test]
    fn the_output_callback_keeps_its_contract() {
        let setup = |chans: usize| {
            let st = Arc::new(PlayerState::new());
            st.volume.store(100, Ordering::Relaxed);
            let (mut p, c) = rtrb::RingBuffer::<f32>::new(64);
            let (vp, _vc) = rtrb::RingBuffer::<f32>::new(64);
            p.push_entire_slice(&[0.5, -0.25, 0.125, -0.0625]).unwrap(); // 2 stereo frames
            (Arc::clone(&st), OutputRenderer::new(c, vp, st, chans))
        };
        // Paused: silence, and nothing consumed.
        let (st, mut r) = setup(2);
        st.paused.store(true, Ordering::Relaxed);
        let mut out = [9.0f32; 4];
        r.render_f32(&mut out);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(st.samples_played.load(Ordering::Relaxed), 0);
        // Unpaused: the frames play, the clock advances by 2 frames.
        st.paused.store(false, Ordering::Relaxed);
        r.render_f32(&mut out);
        assert_eq!(out, [0.5, -0.25, 0.125, -0.0625]);
        assert_eq!(st.samples_played.load(Ordering::Relaxed), 2);
        // A drain request discards the ring and outputs silence.
        let (st, mut r) = setup(2);
        st.reset_consumer_counter.store(true, Ordering::Release);
        let mut out = [9.0f32; 4];
        r.render_f32(&mut out);
        assert_eq!(out, [0.0; 4]);
        assert!(!st.reset_consumer_counter.load(Ordering::Acquire), "drain acknowledged");
        r.render_f32(&mut out);
        assert_eq!(out, [0.0; 4], "ring was emptied");
        // 4-channel device: L/R in the first two, the rest silent.
        let (_st, mut r) = setup(4);
        let mut out = [9.0f32; 8];
        r.render_f32(&mut out);
        assert_eq!(out, [0.5, -0.25, 0.0, 0.0, 0.125, -0.0625, 0.0, 0.0]);
    }

    #[test]
    fn any_card_pcm_maps_to_its_raw_hw_device() {
        // Exclusive mode on Linux needs the raw hw: PCM — the one that plays
        // exactly what it is sent. The friendly name is shared by hw:, plughw:,
        // front:, sysdefault: ... of the same card.
        assert_eq!(hw_pcm_id_for("hw:CARD=KA17,DEV=0").as_deref(), Some("hw:CARD=KA17,DEV=0"));
        assert_eq!(hw_pcm_id_for("front:CARD=KA17,DEV=0").as_deref(), Some("hw:CARD=KA17,DEV=0"));
        assert_eq!(hw_pcm_id_for("plughw:CARD=0,DEV=1").as_deref(), Some("hw:CARD=0,DEV=1"));
        assert_eq!(hw_pcm_id_for("sysdefault:CARD=KA17").as_deref(), Some("hw:CARD=KA17,DEV=0"));
        assert_eq!(hw_pcm_id_for("hw:1,0").as_deref(), Some("hw:CARD=1,DEV=0"));
        // Sound servers and the default route name no card: nothing to own.
        for id in ["default", "pipewire", "pulse", "sysdefault"] {
            assert_eq!(hw_pcm_id_for(id), None, "{id}");
        }
    }

    #[test]
    fn a_hw_device_gets_its_deepest_format_at_the_rate() {
        use cpal::SampleFormat as F;
        let r = |format, min, max| FormatRange { format, channels: 2, min, max };
        let dac = [r(F::I16, 44_100, 192_000), r(F::I32, 44_100, 192_000), r(F::I24, 44_100, 96_000)];
        assert_eq!(choose_output_format(&dac, 96_000, 2), Some((F::I32, 32)));
        // Only packed 24 would beat 16 here, and cpal cannot open S24_3LE: 16
        // is what is left (the caller warns about it).
        let only16 = [r(F::I16, 44_100, 48_000)];
        assert_eq!(choose_output_format(&only16, 44_100, 2), Some((F::I16, 16)));
        // 32-bit float carries 24 exact bits: better than 16, ties 24 integer.
        let f_or_16 = [r(F::I16, 48_000, 48_000), r(F::F32, 48_000, 48_000)];
        assert_eq!(choose_output_format(&f_or_16, 48_000, 2), Some((F::F32, 24)));
        let f_or_24 = [r(F::F32, 48_000, 48_000), r(F::I24, 48_000, 48_000)];
        assert_eq!(choose_output_format(&f_or_24, 48_000, 2), Some((F::I24, 24)));
        // A rate or channel count the device does not list: no format.
        assert_eq!(choose_output_format(&dac, 352_800, 2), None);
        assert_eq!(choose_output_format(&dac, 44_100, 6), None);
    }

    #[test]
    fn a_busy_device_is_recognised_through_the_boxed_error() {
        let busy: Box<dyn std::error::Error> = Box::new(cpal::Error::new(cpal::ErrorKind::DeviceBusy));
        assert!(is_device_busy(busy.as_ref()));
        let other: Box<dyn std::error::Error> = Box::new(cpal::Error::new(cpal::ErrorKind::UnsupportedConfig));
        assert!(!is_device_busy(other.as_ref()));
        let text: Box<dyn std::error::Error> = "some other failure".into();
        assert!(!is_device_busy(text.as_ref()));
    }

    #[test]
    fn the_linux_device_list_shows_routes_and_one_line_per_card_output() {
        // A typical PipeWire laptop with a USB DAC: 20+ ALSA PCMs for two cards.
        let d = |id: &str, name: &str| ListedDevice { id: id.into(), name: name.into(), default: id == "default" };
        let devs = vec![
            d("default", "Default ALSA Output (currently PipeWire Media Server)"),
            d("pipewire", "PipeWire Sound Server"),
            d("pulse", "PulseAudio Sound Server"),
            d("null", "Discard all samples (playback) or generate zero samples (capture)"),
            d("samplerate", "Rate Converter Plugin Using Samplerate Library"),
            d("sysdefault:CARD=KA17", "FIIO KA17, USB Audio"),
            d("front:CARD=KA17,DEV=0", "FIIO KA17, USB Audio"),
            d("surround51:CARD=KA17,DEV=0", "FIIO KA17, USB Audio"),
            d("iec958:CARD=KA17,DEV=0", "FIIO KA17, USB Audio"),
            d("dmix:CARD=KA17,DEV=0", "FIIO KA17, USB Audio"),
            d("hw:CARD=KA17,DEV=0", "FIIO KA17, USB Audio"),
            d("plughw:CARD=KA17,DEV=0", "FIIO KA17, USB Audio"),
            d("hw:CARD=PCH,DEV=0", "HDA Intel PCH, ALC3246 Analog"),
            d("hw:CARD=PCH,DEV=3", "HDA Intel PCH, HDMI 0"),
            d("hdmi:CARD=PCH,DEV=0", "HDA Intel PCH, HDMI 0"),
            // Physical enumeration repeats the same hardware under numeric names.
            d("hw:CARD=1,DEV=0", "FIIO KA17, USB Audio"),
            d("plughw:CARD=1,DEV=0", "FIIO KA17, USB Audio"),
            d("hw:CARD=0,DEV=0", "HDA Intel PCH, ALC3246 Analog"),
        ];
        let lines = compact_device_list(&devs, true);
        let text = lines.join("\n");
        // Routes for normal mode, marked default; internal plugins hidden.
        assert!(text.contains("default") && text.contains("(default)") && text.contains("pipewire"));
        assert!(!text.contains("samplerate") && !text.contains("Discard all samples"));
        // One line per card output, by its hw: id; everything else of a card hidden.
        for id in ["hw:CARD=KA17,DEV=0", "hw:CARD=PCH,DEV=0", "hw:CARD=PCH,DEV=3"] {
            assert_eq!(lines.iter().filter(|l| l.contains(id)).count(), 1, "{id} once:\n{text}");
        }
        for hidden in ["front:", "surround51", "iec958", "dmix", "plughw", "sysdefault:CARD", "hdmi:CARD", "CARD=1,", "CARD=0,"] {
            assert!(!text.contains(hidden), "{hidden} should be hidden:\n{text}");
        }
        assert!(lines.len() <= 12, "compact list is {} lines:\n{text}", lines.len());
    }

    #[test]
    fn other_platforms_list_one_line_per_device() {
        let devs = vec![
            ListedDevice { id: "AppleUSBAudioEngine:FiiO:1".into(), name: "FIIO KA17".into(), default: true },
            ListedDevice { id: "BuiltInSpeakerDevice".into(), name: "MacBook Air Speakers".into(), default: false },
        ];
        let lines = compact_device_list(&devs, false);
        let text = lines.join("\n");
        assert!(text.contains("FIIO KA17") && text.contains("(default)") && text.contains("MacBook Air Speakers"));
        assert_eq!(lines.iter().filter(|l| l.contains("FIIO KA17")).count(), 1);
    }

    #[test]
    fn exclusive_output_packs_each_wasapi_layout_exactly() {
        // A 24-bit sample v arrives from the renderer as i32 v << 8. Each
        // layout must carry it exactly: 32/32 as-is, 24-in-32 with the unused
        // low byte zero, packed 24 as three bytes, and 16-bit rounded.
        let v24: i32 = -5_000_001;
        let full = [v24 << 8];
        let mut out = [0u8; 4];
        pack_samples(&full, 32, 32, &mut out);
        assert_eq!(i32::from_le_bytes(out), v24 << 8);
        pack_samples(&[(v24 << 8) | 0x7F], 32, 24, &mut out);
        assert_eq!(i32::from_le_bytes(out), v24 << 8, "low byte cleared for 24-in-32");
        let mut p3 = [0u8; 3];
        pack_samples(&full, 24, 24, &mut p3);
        let back = i32::from_le_bytes([0, p3[0], p3[1], p3[2]]) >> 8;
        assert_eq!(back, v24, "packed 24-bit");
        let v16: i32 = -12_345;
        let mut p2 = [0u8; 2];
        pack_samples(&[v16 << 16], 16, 16, &mut p2);
        assert_eq!(i16::from_le_bytes(p2) as i32, v16, "16-bit source exact");
        pack_samples(&[i32::MAX], 16, 16, &mut p2);
        assert_eq!(i16::from_le_bytes(p2), i16::MAX, "no wrap at full scale");
        // Several samples: byte stride follows the store width.
        let mut many = [0u8; 9];
        pack_samples(&[1 << 8, 2 << 8, 3 << 8], 24, 24, &mut many);
        assert_eq!(many, [1, 0, 0, 2, 0, 0, 3, 0, 0]);
    }
}
