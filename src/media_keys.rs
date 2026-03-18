use std::sync::Arc;
use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata,
    MediaPlayback, MediaPosition, PlatformConfig,
};

use crate::state::PlayerState;

pub fn setup(state: Arc<PlayerState>) -> Option<MediaControls> {
    let config = PlatformConfig {
        dbus_name: "keet",
        display_name: "Keet",
        hwnd: platform_hwnd(),
    };

    let mut controls = MediaControls::new(config).ok()?;

    controls.attach(move |event: MediaControlEvent| {
        match event {
            MediaControlEvent::Play => {
                if state.is_paused() { state.toggle_pause(); }
            }
            MediaControlEvent::Pause => {
                if !state.is_paused() { state.toggle_pause(); }
            }
            MediaControlEvent::Toggle => state.toggle_pause(),
            MediaControlEvent::Next => state.next(),
            MediaControlEvent::Previous => state.prev(),
            MediaControlEvent::Stop => state.quit(),
            _ => {}
        }
    }).ok()?;

    Some(controls)
}

pub fn update_metadata(controls: &mut MediaControls, title: &str, duration_secs: f64) {
    let _ = controls.set_metadata(MediaMetadata {
        title: Some(title),
        artist: None,
        album: None,
        cover_url: None,
        duration: if duration_secs > 0.0 {
            Some(Duration::from_secs_f64(duration_secs))
        } else {
            None
        },
    });
}

pub fn update_playback(controls: &mut MediaControls, paused: bool, position_secs: f64) {
    let progress = Some(MediaPosition(Duration::from_secs_f64(position_secs.max(0.0))));
    let playback = if paused {
        MediaPlayback::Paused { progress }
    } else {
        MediaPlayback::Playing { progress }
    };
    let _ = controls.set_playback(playback);
}

/// Pump the platform event loop so media control callbacks get dispatched.
/// Must be called periodically from the main thread.
#[cfg(target_os = "macos")]
pub fn poll() {
    use std::ffi::c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRunLoopGetMain() -> *mut c_void;
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source_handled: u8) -> i32;
    }
    // kCFRunLoopDefaultMode
    extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
    }
    unsafe {
        let _main_loop = CFRunLoopGetMain();
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.001, 1);
    }
}

#[cfg(target_os = "windows")]
pub fn poll() {
    use std::ffi::c_void;
    #[repr(C)]
    struct MSG {
        hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
        time: u32,
        pt_x: i32,
        pt_y: i32,
    }
    extern "system" {
        fn PeekMessageW(msg: *mut MSG, hwnd: *mut c_void, filter_min: u32, filter_max: u32, remove: u32) -> i32;
        fn TranslateMessage(msg: *const MSG) -> i32;
        fn DispatchMessageW(msg: *const MSG) -> isize;
    }
    const PM_REMOVE: u32 = 0x0001;
    let mut msg = std::mem::MaybeUninit::<MSG>::zeroed();
    unsafe {
        while PeekMessageW(msg.as_mut_ptr(), std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            let msg = msg.assume_init_ref();
            TranslateMessage(msg);
            DispatchMessageW(msg);
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn poll() {
    // No-op on Linux — MPRIS uses D-Bus background threads
}

#[cfg(target_os = "windows")]
fn platform_hwnd() -> Option<*mut std::ffi::c_void> {
    // SMTC requires a window owned by our process. Console windows belong to
    // conhost.exe/Windows Terminal, so GetConsoleWindow() doesn't work.
    // Create a hidden message-only window instead.
    use std::ffi::c_void;
    use std::ptr;

    #[repr(C)]
    struct WndClassExW {
        cb_size: u32,
        style: u32,
        wnd_proc: unsafe extern "system" fn(*mut c_void, u32, usize, isize) -> isize,
        cls_extra: i32,
        wnd_extra: i32,
        instance: *mut c_void,
        icon: *mut c_void,
        cursor: *mut c_void,
        background: *mut c_void,
        menu_name: *const u16,
        class_name: *const u16,
        icon_sm: *mut c_void,
    }

    extern "system" {
        fn RegisterClassExW(lpwcx: *const WndClassExW) -> u16;
        fn CreateWindowExW(
            ex_style: u32, class_name: *const u16, window_name: *const u16,
            style: u32, x: i32, y: i32, w: i32, h: i32,
            parent: *mut c_void, menu: *mut c_void, instance: *mut c_void, param: *mut c_void,
        ) -> *mut c_void;
        fn DefWindowProcW(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize;
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
    }

    unsafe extern "system" fn wnd_proc(hwnd: *mut c_void, msg: u32, wp: usize, lp: isize) -> isize {
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    // Encode "KeetSMTC" as UTF-16
    let class_name: Vec<u16> = "KeetSMTC\0".encode_utf16().collect();
    let instance = unsafe { GetModuleHandleW(ptr::null()) };

    let wc = WndClassExW {
        cb_size: std::mem::size_of::<WndClassExW>() as u32,
        style: 0,
        wnd_proc,
        cls_extra: 0,
        wnd_extra: 0,
        instance,
        icon: ptr::null_mut(),
        cursor: ptr::null_mut(),
        background: ptr::null_mut(),
        menu_name: ptr::null(),
        class_name: class_name.as_ptr(),
        icon_sm: ptr::null_mut(),
    };

    unsafe { RegisterClassExW(&wc) };

    extern "system" {
        fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
    }

    // Create a real top-level window (SMTC rejects message-only windows).
    // WS_OVERLAPPEDWINDOW = 0x00CF0000, but never shown — invisible to user.
    const WS_OVERLAPPEDWINDOW: u32 = 0x00CF0000;
    let hwnd = unsafe {
        CreateWindowExW(
            0, class_name.as_ptr(), class_name.as_ptr(),
            WS_OVERLAPPEDWINDOW, 0, 0, 0, 0,
            ptr::null_mut(), ptr::null_mut(), instance, ptr::null_mut(),
        )
    };

    // Ensure it stays hidden (SW_HIDE = 0)
    if !hwnd.is_null() {
        unsafe { ShowWindow(hwnd, 0); }
    }

    if hwnd.is_null() { None } else { Some(hwnd) }
}

#[cfg(not(target_os = "windows"))]
fn platform_hwnd() -> Option<*mut std::ffi::c_void> {
    None
}
