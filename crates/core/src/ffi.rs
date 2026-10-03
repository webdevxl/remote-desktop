//! C ABI for the SwiftUI app. Mirrors `macos/Sources/CLanKVM/include/lankvm.h`.
//!
//! Strings returned by `lk_*` functions are JSON, owned by the caller, and must be released
//! with [`lk_string_free`]. Events arrive as JSON on the callback, on arbitrary threads.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, OnceLock};

use protocol::{InputMsg, POS_MAX, ScrollInput};
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
/// screen size in pixels and `max_fps` its refresh rate. Returns a session id (0 if the core
/// isn't running).
///
/// # Safety
/// `target` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_connect(target: *const c_char, max_width: u32, max_height: u32, max_fps: u32) -> u64 {
    let target = unsafe { arg(target) };
    core().map_or(0, |c| c.connect(&target, (max_width, max_height), max_fps))
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

// MARK: Remote control (viewer side). Called for every input event, so no JSON here.

/// Asks the host for control (`on`) or to only view it; `take_over` takes control from another
/// device that has it. The answer arrives as a `control` event carrying the returned request id.
#[unsafe(no_mangle)]
pub extern "C" fn lk_set_control(session: u64, on: bool, take_over: bool) -> u32 {
    core().map_or(0, |c| c.set_control(session, on, take_over))
}

/// While controlling: whether the viewer window has the focus and forwards input. The host puts
/// its cursor back into the video while it doesn't.
#[unsafe(no_mangle)]
pub extern "C" fn lk_set_focus(session: u64, forwarding: bool) {
    if let Some(c) = core() {
        c.set_focus(session, forwarding);
    }
}

/// A position on the remote screen, 0...1 from the left/top edge, to the wire format.
fn pos(v: f64) -> u16 {
    if v.is_finite() { (v.clamp(0.0, 1.0) * f64::from(POS_MAX)).round() as u16 } else { 0 }
}

fn send(session: u64, msg: InputMsg) {
    if let Some(c) = core() {
        c.send_input(session, msg);
    }
}

/// `x`, `y`: position on the remote screen, 0...1 from its left/top edge.
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_mouse_move(session: u64, x: f64, y: f64) {
    send(session, InputMsg::MouseMove { x: pos(x), y: pos(y) });
}

/// `button`: 0 left, 1 right, 2 middle, 3+ others. `clicks`: the event's click count.
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_mouse_button(session: u64, button: u8, down: bool, clicks: u8, x: f64, y: f64) {
    send(session, InputMsg::MouseButton { button, down, clicks, x: pos(x), y: pos(y) });
}

/// One scroll event, copied from the NSEvent's CGEvent fields. Mirrors `lk_scroll` in lankvm.h.
#[repr(C)]
pub struct LkScroll {
    pub x: f64,
    pub y: f64,
    pub lines_y: i32,
    pub lines_x: i32,
    pub fixed_y: f64,
    pub fixed_x: f64,
    pub pixels_y: i32,
    pub pixels_x: i32,
    pub continuous: bool,
    pub phase: u8,
    pub momentum: u8,
    pub inverted: bool,
}

/// # Safety
/// `scroll` must point to a valid `lk_scroll`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_input_scroll(session: u64, scroll: *const LkScroll) {
    let Some(s) = (unsafe { scroll.as_ref() }) else { return };
    send(
        session,
        InputMsg::Scroll(ScrollInput {
            x: pos(s.x),
            y: pos(s.y),
            lines_y: s.lines_y,
            lines_x: s.lines_x,
            fixed_y: s.fixed_y as f32,
            fixed_x: s.fixed_x as f32,
            pixels_y: s.pixels_y,
            pixels_x: s.pixels_x,
            continuous: s.continuous,
            phase: s.phase,
            momentum: s.momentum,
            inverted: s.inverted,
        }),
    );
}

/// A non-modifier key by macOS virtual key code. Modifiers go through `lk_input_modifiers`.
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_key(session: u64, code: u16, down: bool, repeat: bool) {
    send(session, InputMsg::Key { code, down, repeat });
}

/// The modifiers held now, as CGEventFlags with the left/right device bits.
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_modifiers(session: u64, flags: u64) {
    send(session, InputMsg::Modifiers { flags: (flags & platform_mac::keys::MODIFIER_MASK) as u32 });
}

/// Releases every key and button on the host (focus left the remote screen).
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_release_all(session: u64) {
    send(session, InputMsg::ReleaseAll);
}

/// "Still here": call from the UI thread every 250 ms while anything is held, so the host can
/// let go if this app hangs or disappears.
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_heartbeat(session: u64) {
    send(session, InputMsg::Heartbeat);
}

/// How many LanKVM hosts the input sent next has passed through (0: made on this Mac). Send it
/// when it changes; the host drops input that went around a loop of Macs.
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_relayed(session: u64, depth: u8) {
    send(session, InputMsg::Relayed { depth });
}

/// Before quitting: releases everything held on remote Macs and on this one. Blocks briefly.
#[unsafe(no_mangle)]
pub extern "C" fn lk_shutdown() {
    if let Some(c) = core() {
        c.shutdown();
    }
}

// MARK: Remote control (host side)

/// Takes control back from a viewer of this Mac; it keeps viewing.
#[unsafe(no_mangle)]
pub extern "C" fn lk_stop_control(viewer: u64) {
    if let Some(c) = core() {
        c.stop_control(viewer);
    }
}

/// Takes control back from whoever controls this Mac (menu, stop hotkey).
#[unsafe(no_mangle)]
pub extern "C" fn lk_stop_all_control() {
    if let Some(c) = core() {
        c.stop_all_control();
    }
}

/// Whether paired Macs may control this one (saved).
#[unsafe(no_mangle)]
pub extern "C" fn lk_set_allow_control(allow: bool) {
    if let Some(c) = core() {
        c.set_allow_control(allow);
    }
}

/// Whether macOS lets LanKVM post input (Privacy & Security → Accessibility).
#[unsafe(no_mangle)]
pub extern "C" fn lk_control_permission() -> bool {
    core().is_some_and(|c| c.control_permission())
}

#[cfg(test)]
mod tests {
    use super::pos;

    #[test]
    fn positions_quantize_and_clamp() {
        assert_eq!(pos(0.0), 0);
        assert_eq!(pos(1.0), u16::MAX);
        assert_eq!(pos(0.5), 32768);
        assert_eq!(pos(-1.0), 0);
        assert_eq!(pos(7.0), u16::MAX);
        assert_eq!(pos(f64::NAN), 0);
    }
}
