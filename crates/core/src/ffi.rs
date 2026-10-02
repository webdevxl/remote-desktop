//! C ABI for the SwiftUI app. Mirrors `macos/Sources/CLanKVM/include/lankvm.h`.
//!
//! Strings returned by `lk_*` functions are JSON, owned by the caller, and must be released
//! with [`lk_string_free`]. Events arrive as JSON on the callback, on arbitrary threads.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, OnceLock};

use serde::Serialize;

use crate::{Core, Event};

static CORE: OnceLock<Arc<Core>> = OnceLock::new();

pub type EventCallback = extern "C" fn(json: *const c_char, ctx: *mut c_void);

struct CallbackCtx(*mut c_void);
// SAFETY: the context pointer is opaque to us; the app guarantees it may be used from any thread.
unsafe impl Send for CallbackCtx {}
unsafe impl Sync for CallbackCtx {}

fn core() -> Option<&'static Arc<Core>> {
    CORE.get()
}

fn to_c(s: String) -> *mut c_char {
    CString::new(s).unwrap_or_default().into_raw()
}

fn json<T: Serialize>(value: &T) -> *mut c_char {
    to_c(serde_json::to_string(value).unwrap_or_else(|_| "null".into()))
}

/// # Safety
/// `s` must be null or a valid NUL-terminated string.
unsafe fn arg(s: *const c_char) -> String {
    if s.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
}

/// Starts the core: the listener for incoming viewers and the session machinery.
/// Returns null on success, or an error message (free with `lk_string_free`).
#[unsafe(no_mangle)]
pub extern "C" fn lk_start(callback: EventCallback, ctx: *mut c_void) -> *mut c_char {
    if CORE.get().is_some() {
        return std::ptr::null_mut();
    }
    let ctx = CallbackCtx(ctx);
    let sink = Arc::new(move |event: Event| {
        let ctx = &ctx;
        if let Ok(text) = serde_json::to_string(&event)
            && let Ok(text) = CString::new(text)
        {
            callback(text.as_ptr(), ctx.0);
        }
    });
    match Core::start(sink) {
        Ok(core) => {
            let _ = CORE.set(core);
            std::ptr::null_mut()
        }
        Err(e) => {
            tracing::error!("start failed: {e:#}");
            to_c(format!("{e:#}"))
        }
    }
}

/// # Safety
/// `s` must come from an `lk_*` function and not be freed twice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn lk_this_mac() -> *mut c_char {
    core().map_or(std::ptr::null_mut(), |c| json(c.this_mac()))
}

#[unsafe(no_mangle)]
pub extern "C" fn lk_host_status() -> *mut c_char {
    core().map_or(std::ptr::null_mut(), |c| json(&c.host_status()))
}

#[unsafe(no_mangle)]
pub extern "C" fn lk_paired_devices() -> *mut c_char {
    core().map_or(std::ptr::null_mut(), |c| json(&c.paired_devices()))
}

#[unsafe(no_mangle)]
pub extern "C" fn lk_recent_hosts() -> *mut c_char {
    core().map_or(std::ptr::null_mut(), |c| json(&c.recent_hosts()))
}

/// Connects to `target` (IP, `ip:port` or hostname). `max_width`/`max_height` are the viewer's
/// screen size in pixels. Returns a session id (0 if the core isn't running).
///
/// # Safety
/// `target` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_connect(target: *const c_char, max_width: u32, max_height: u32) -> u64 {
    let target = unsafe { arg(target) };
    core().map_or(0, |c| c.connect(&target, (max_width, max_height)))
}

/// # Safety
/// `pin` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_submit_pin(session: u64, pin: *const c_char) {
    let pin = unsafe { arg(pin) };
    if let Some(c) = core() {
        c.submit_pin(session, &pin);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn lk_disconnect(session: u64) {
    if let Some(c) = core() {
        c.disconnect(session);
    }
}

/// Starts rendering the session into `metal_layer` (a `CAMetalLayer`), sized in pixels.
///
/// # Safety
/// `metal_layer` must be a valid `CAMetalLayer`; it is retained until `lk_detach_view`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_attach_view(session: u64, metal_layer: *mut c_void, width: u32, height: u32) {
    if let Some(c) = core() {
        unsafe { c.attach_view(session, metal_layer, width, height) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn lk_resize_view(session: u64, width: u32, height: u32) {
    if let Some(c) = core() {
        c.resize_view(session, width, height);
    }
}

/// Stops rendering; when this returns the layer is no longer used.
#[unsafe(no_mangle)]
pub extern "C" fn lk_detach_view(session: u64) {
    if let Some(c) = core() {
        c.detach_view(session);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn lk_session_stats(session: u64) -> *mut c_char {
    core().and_then(|c| c.session_stats(session)).map_or(std::ptr::null_mut(), |s| json(&s))
}

/// Disconnects a device that is viewing this Mac.
#[unsafe(no_mangle)]
pub extern "C" fn lk_kick_viewer(viewer: u64) {
    if let Some(c) = core() {
        c.kick_viewer(viewer);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn lk_deny_pairing(request: u64) {
    if let Some(c) = core() {
        c.deny_pairing(request);
    }
}

/// `kind` is "viewer" (a device allowed to control this Mac) or "host".
///
/// # Safety
/// Both arguments must be valid NUL-terminated strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_forget_device(kind: *const c_char, fingerprint: *const c_char) {
    let (kind, fingerprint) = unsafe { (arg(kind), arg(fingerprint)) };
    if let Some(c) = core() {
        c.forget_device(&kind, &fingerprint);
    }
}

/// Authoritative Screen Recording check (asks ScreenCaptureKit; may take a moment).
#[unsafe(no_mangle)]
pub extern "C" fn lk_verify_screen_capture() -> bool {
    core().is_some_and(|c| c.verify_screen_capture())
}

/// Shows the system Screen Recording prompt (macOS shows it at most once per app).
#[unsafe(no_mangle)]
pub extern "C" fn lk_request_screen_capture() -> bool {
    platform_mac::permissions::request_screen_capture()
}

#[unsafe(no_mangle)]
pub extern "C" fn lk_screen_capture_allowed() -> bool {
    core().is_some_and(|c| c.screen_capture_allowed())
}
