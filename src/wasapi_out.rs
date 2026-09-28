//! WASAPI exclusive-mode output (Windows).
//!
//! cpal's WASAPI backend only ever opens SHARED mode, which runs everything
//! through the Windows audio engine (mixed and resampled to one system
//! format), so it can never be bit-perfect. Exclusive mode hands the stream
//! straight to the driver: the device runs at the track's rate and format.
//!
//! The render thread owns every COM object (they are created and used on it,
//! in the multithreaded apartment) and feeds the device from the same
//! `OutputRenderer` the cpal callback uses — ring buffer, drain protocol,
//! volume, position counting and viz tap are one implementation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rtrb::{Consumer, Producer};
use wasapi::{
    calculate_period_100ns, initialize_mta, AudioClient, AudioRenderClient, DeviceEnumerator,
    Direction, Handle, SampleType, StreamMode, WasapiError, WaveFormat,
};

use crate::audio::{pack_samples, DeviceBusyError, OutputRenderer};
use crate::state::PlayerState;

// AUDCLNT_E_* HRESULTs (values from windows-rs 0.62, Win32::Media::Audio).
const E_DEVICE_INVALIDATED: u32 = 0x8889_0004;
const E_DEVICE_IN_USE: u32 = 0x8889_000A;
const E_BUFFER_SIZE_NOT_ALIGNED: u32 = 0x8889_0019;

/// Sample layouts tried, most precise first: (bits stored, bits used).
/// 24-in-32 is the common USB DAC layout; packed 24 and 16 are fallbacks.
const LAYOUTS: [(u16, u16); 4] = [(32, 32), (32, 24), (24, 24), (16, 16)];

/// Rates probed for exclusive-mode support.
const RATES: [u32; 10] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000, 705_600, 768_000,
];

fn hresult(e: &WasapiError) -> Option<u32> {
    match e {
        WasapiError::Windows(w) => Some(w.code().0 as u32),
        _ => None,
    }
}

/// Run `f` on a fresh thread in the multithreaded COM apartment. The UI
/// thread's apartment is not ours to choose (media keys may have made it an
/// STA), so capability probes get a thread of their own.
fn with_mta<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    std::thread::spawn(move || {
        let _ = initialize_mta();
        f()
    })
    .join()
    .ok()
}

fn format(layout: (u16, u16), rate: u32, channels: u16) -> WaveFormat {
    WaveFormat::new(
        layout.0 as usize,
        layout.1 as usize,
        &SampleType::Int,
        rate as usize,
        channels as usize,
        None,
    )
}

fn accepts(client: &AudioClient, layout: (u16, u16), rate: u32, channels: u16) -> bool {
    client
        .is_supported_exclusive_with_quirks(&format(layout, rate, channels))
        .is_ok()
}

/// The most precise layout the device accepts in exclusive mode at `rate`
/// with `channels` channels. Must be called with no exclusive stream open on
/// the device (it would answer "in use").
pub fn best_layout(device_id: &str, rate: u32, channels: u16) -> Option<(u16, u16)> {
    let id = device_id.to_string();
    with_mta(move || {
        let client = DeviceEnumerator::new().ok()?.get_device(&id).ok()?.get_iaudioclient().ok()?;
        LAYOUTS.iter().copied().find(|&l| accepts(&client, l, rate, channels))
    })
    .flatten()
}

/// Every probed rate the device accepts in exclusive mode (in any layout).
pub fn supported_rates(device_id: &str, channels: u16) -> Vec<u32> {
    let id = device_id.to_string();
    with_mta(move || {
        let Some(client) = DeviceEnumerator::new()
            .ok()
            .and_then(|e| e.get_device(&id).ok())
            .and_then(|d| d.get_iaudioclient().ok())
        else {
            return Vec::new();
        };
        RATES
            .iter()
            .copied()
            .filter(|&r| LAYOUTS.iter().any(|&l| accepts(&client, l, r, channels)))
            .collect()
    })
    .unwrap_or_default()
}

/// Diagnostic for `--list-devices --verbose`: for every probed rate, what the
/// driver answers to the exclusive format check (per layout, with the HRESULT
/// when it refuses) and whether an exclusive open at that rate actually
/// succeeds. The two can disagree — a driver may refuse the check yet open, or
/// lock itself to its control-panel rate — and the rate logic rests on the check.
pub fn exclusive_report(device_id: &str, channels: u16) -> Vec<String> {
    let id = device_id.to_string();
    with_mta(move || {
        let Ok(device) = DeviceEnumerator::new().and_then(|e| e.get_device(&id)) else {
            return vec!["exclusive: <device not found by WASAPI>".to_string()];
        };
        let code = |e: &WasapiError| match hresult(e) {
            Some(h) => format!("{h:#010x}"),
            None => e.to_string(),
        };
        let mut out = vec![format!("exclusive ({channels} ch): rate  check[32/32 32/24 24/24 16/16]  open")];
        for &rate in &RATES {
            let Ok(client) = device.get_iaudioclient() else {
                out.push(format!("  {rate}: <no audio client>"));
                continue;
            };
            let checks: Vec<String> = LAYOUTS
                .iter()
                .map(|&l| match client.is_supported(&format(l, rate, channels), &wasapi::ShareMode::Exclusive) {
                    Ok(_) => "ok".to_string(),
                    Err(e) => match client.is_supported_exclusive_with_quirks(&format(l, rate, channels)) {
                        Ok(_) => "ok*".to_string(),
                        Err(_) => code(&e),
                    },
                })
                .collect();
            let layout = LAYOUTS
                .iter()
                .copied()
                .find(|&l| accepts(&client, l, rate, channels))
                .unwrap_or((32, 24));
            drop(client);
            let opened = match open(&id, rate, channels, layout, false) {
                Ok(_) => format!("ok ({}/{})", layout.0, layout.1),
                Err((e, _)) => e.trim_start_matches("exclusive mode: ").to_string(),
            };
            out.push(format!("  {rate}: {}  {opened}", checks.join(" ")));
        }
        out
    })
    .unwrap_or_default()
}

/// A running exclusive-mode stream. Stopped (and its thread joined) by
/// `stop` or on drop.
pub struct WasapiOutput {
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct Opened {
    client: AudioClient,
    render: AudioRenderClient,
    event: Handle,
    layout: (u16, u16),
    channels: usize,
}

impl WasapiOutput {
    /// Open `device_id` exclusively at `rate` in `layout` and start playing
    /// from `consumer`. Returns only once the device is open (or failed to
    /// open): a busy device comes back as `DeviceBusyError`.
    pub fn start(
        device_id: &str,
        rate: u32,
        channels: u16,
        layout: (u16, u16),
        consumer: Consumer<f32>,
        viz_producer: Producer<f32>,
        state: Arc<PlayerState>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let id = device_id.to_string();
        let busy = Arc::new(AtomicBool::new(false));
        let busy_thread = Arc::clone(&busy);
        let handle = std::thread::Builder::new()
            .name("keet-wasapi".into())
            .spawn(move || {
                let _ = initialize_mta();
                let mut renderer =
                    OutputRenderer::new(consumer, viz_producer, Arc::clone(&state), channels as usize);
                match open(&id, rate, channels, layout, true) {
                    Err((e, in_use)) => {
                        busy_thread.store(in_use, Ordering::Relaxed);
                        let _ = tx.send(Err(e));
                    }
                    Ok(o) => {
                        let _ = tx.send(Ok(()));
                        run(o, &mut renderer, &stop_thread, &state);
                    }
                }
            })?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { stop, thread: Mutex::new(Some(handle)) }),
            Ok(Err(e)) => {
                let _ = handle.join();
                if busy.load(Ordering::Relaxed) {
                    Err(Box::new(DeviceBusyError(e)))
                } else {
                    Err(e.into())
                }
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("timed out opening the device in exclusive mode".into())
            }
        }
    }

    /// Stop the stream and wait for the render thread to finish.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = h.join();
        }
    }
}

impl Drop for WasapiOutput {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Open the device exclusively (event-driven). Err carries a message and
/// whether the device was in use by another program. `checked: false` skips
/// the format check and goes straight to initialize (the diagnostic uses it to
/// see whether the driver's answer to the check matches what it will open).
fn open(id: &str, rate: u32, channels: u16, layout: (u16, u16), checked: bool) -> Result<Opened, (String, bool)> {
    let fail = |what: &str, e: WasapiError| {
        let in_use = hresult(&e) == Some(E_DEVICE_IN_USE);
        // AUDCLNT codes often have no system message text, so keep the code.
        let msg = match hresult(&e) {
            Some(h) => format!("{what}: {h:#010x} {e}"),
            None => format!("{what}: {e}"),
        };
        (msg.trim_end().to_string(), in_use)
    };
    let device = DeviceEnumerator::new()
        .and_then(|en| en.get_device(id))
        .map_err(|e| fail("exclusive mode: device", e))?;
    let mut client = device.get_iaudioclient().map_err(|e| fail("exclusive mode: client", e))?;
    let fmt = if checked {
        client
            .is_supported_exclusive_with_quirks(&format(layout, rate, channels))
            .map_err(|e| fail("exclusive mode: format", e))?
    } else {
        format(layout, rate, channels)
    };
    let (default_period, _) = client.get_device_period().map_err(|e| fail("exclusive mode: period", e))?;
    // 128-byte alignment keeps devices such as Intel HDA happy.
    let period = client
        .calculate_aligned_period_near(default_period, Some(128), &fmt)
        .map_err(|e| fail("exclusive mode: period", e))?;
    if let Err(e) = client.initialize_client(&fmt, &Direction::Render, &StreamMode::EventsExclusive { period_hns: period }) {
        if hresult(&e) != Some(E_BUFFER_SIZE_NOT_ALIGNED) {
            return Err(fail("exclusive mode: open", e));
        }
        // Documented recovery: re-create the client with the aligned buffer's period.
        let frames = client.get_buffer_size().map_err(|e| fail("exclusive mode: buffer", e))?;
        let aligned = calculate_period_100ns(frames as i64, rate as i64);
        client = device.get_iaudioclient().map_err(|e| fail("exclusive mode: client", e))?;
        client
            .initialize_client(&fmt, &Direction::Render, &StreamMode::EventsExclusive { period_hns: aligned })
            .map_err(|e| fail("exclusive mode: open", e))?;
    }
    let event = client.set_get_eventhandle().map_err(|e| fail("exclusive mode: event", e))?;
    let render = client.get_audiorenderclient().map_err(|e| fail("exclusive mode: render client", e))?;
    Ok(Opened { client, render, event, layout, channels: channels as usize })
}

/// The render loop: wait for the device's event, render, pack, write. Ends on
/// `stop`, or on device loss (which raises `stream_error`, so main's recovery
/// takes over exactly as for a cpal stream).
fn run(o: Opened, renderer: &mut OutputRenderer, stop: &AtomicBool, state: &PlayerState) {
    let bytes_per_sample = (o.layout.0 / 8) as usize;
    let max_frames = o.client.get_buffer_size().unwrap_or(8192) as usize;
    let mut samples = vec![0i32; max_frames * o.channels];
    let mut bytes = vec![0u8; max_frames * o.channels * bytes_per_sample];
    let lost = |e: &WasapiError| hresult(e) == Some(E_DEVICE_INVALIDATED);

    let mut write = |renderer: &mut OutputRenderer| -> Result<(), WasapiError> {
        let frames = (o.client.get_available_space_in_frames()? as usize).min(max_frames);
        if frames == 0 {
            return Ok(());
        }
        let n = frames * o.channels;
        renderer.render(&mut samples[..n]);
        pack_samples(&samples[..n], o.layout.0, o.layout.1, &mut bytes[..n * bytes_per_sample]);
        o.render.write_to_device(frames, &bytes[..n * bytes_per_sample], None)
    };

    // Fill the whole buffer before starting, so playback begins with audio.
    if let Err(e) = write(renderer).and_then(|_| o.client.start_stream()) {
        if lost(&e) {
            state.stream_error.store(true, Ordering::Relaxed);
        }
        return;
    }
    while !stop.load(Ordering::Relaxed) {
        if o.event.wait_for_event(500).is_err() {
            // No event: a pause in the device's clock or the device going
            // away. Asking the client tells the two apart.
            if let Err(e) = o.client.get_available_space_in_frames() {
                if lost(&e) {
                    state.stream_error.store(true, Ordering::Relaxed);
                    break;
                }
            }
            continue;
        }
        if let Err(e) = write(renderer) {
            if lost(&e) {
                state.stream_error.store(true, Ordering::Relaxed);
            }
            break;
        }
    }
    let _ = o.client.stop_stream();
}
