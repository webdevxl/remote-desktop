//! C ABI for the SwiftUI app. Mirrors `macos/Sources/CLanKVM/include/lankvm.h`.
//!
//! Strings returned by `lk_*` functions are JSON, owned by the caller, and must be released
//! with [`lk_string_free`]. Events arrive as JSON on the callback, on arbitrary threads.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, OnceLock};

use protocol::{Arrangement, DisplayChoice, DockAxis, GestureInput, GesturePhase, InputMsg, POS_MAX, ScrollInput, SystemAction, VirtualDisplaySpec};
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

/// Connects to `target` (IP, `ip:port` or hostname; `lankvm:<fingerprint>` for a paired host over
/// the internet). `max_width`/`max_height` are the viewer's screen size in pixels and `max_fps`
/// its refresh rate. Returns a session id (0 if the core isn't running).
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

// MARK: Host displays (viewer side)

/// Shows the host's own (main) screen. Returns the request id the answering `display` event carries.
#[unsafe(no_mangle)]
pub extern "C" fn lk_show_main_display(session: u64) -> u32 {
    core().map_or(0, |c| c.set_display(session, DisplayChoice::Main))
}

/// Asks the host for a virtual display of `width` × `height` pixels, drawn at 2x if `hidpi`,
/// arranged as `LK_ARRANGE_*` says. Returns the request id the answering `display` event carries.
#[unsafe(no_mangle)]
pub extern "C" fn lk_show_virtual_display(session: u64, width: u32, height: u32, hidpi: bool, refresh_hz: u32, arrangement: u8) -> u32 {
    let spec = VirtualDisplaySpec { width, height, hidpi, refresh_hz, arrangement: Arrangement(arrangement) };
    core().map_or(0, |c| c.set_display(session, DisplayChoice::Virtual(spec)))
}

// MARK: Remote control (viewer side). Called for every input event, so no JSON here.

/// Asks the host for control (`on`) or to only view it; `take_over` takes control from another
/// device that has it. The answer arrives as a `control` event carrying the returned request id.
#[unsafe(no_mangle)]
pub extern "C" fn lk_set_control(session: u64, on: bool, take_over: bool) -> u32 {
    core().map_or(0, |c| c.set_control(session, on, take_over))
}

/// Whether this Mac shares its clipboard with the Macs it controls (what is copied on either can
/// be pasted on the other), for every session. On until told otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn lk_set_share_clipboard(on: bool) {
    if let Some(c) = core() {
        c.set_share_clipboard(on);
    }
}

/// Whether to share this Mac's microphone with the session's host: its apps then hear it as
/// "LanKVM Microphone". Off on connecting. A `microphone` event says whether the host plays it.
#[unsafe(no_mangle)]
pub extern "C" fn lk_set_microphone(session: u64, on: bool) {
    if let Some(c) = core() {
        c.set_microphone(session, on);
    }
}

/// The app installed or removed the LanKVM Microphone driver on this Mac: viewers learn whether
/// they can share their microphones here.
#[unsafe(no_mangle)]
pub extern "C" fn lk_microphone_driver_changed() {
    if let Some(c) = core() {
        c.microphone_driver_changed();
    }
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

/// A three- or four-finger swipe or pinch the Dock acts on, as this Mac's trackpad reported it
/// (gesture event fields): `axis` 1 horizontal, 2 vertical, 3 pinch (field 123); `phase`
/// 1 began, 2 changed, 4 ended, 8 cancelled (field 132); `progress` since it began (124); exit
/// velocities (129, 130); `inverted` (136). Anything else is ignored.
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_dock_swipe(session: u64, axis: u8, phase: u8, progress: f64, velocity_x: f64, velocity_y: f64, inverted: bool) {
    if let Some(msg) = dock_swipe(axis, phase, progress, velocity_x, velocity_y, inverted) {
        send(session, msg);
    }
}

/// `lk_input_dock_swipe`'s message; None for an axis or phase code it doesn't know.
fn dock_swipe(axis: u8, phase: u8, progress: f64, velocity_x: f64, velocity_y: f64, inverted: bool) -> Option<InputMsg> {
    let (axis, phase) = (DockAxis::from_motion(axis)?, GesturePhase::from_bits(phase)?);
    // To the wire's direction convention (see `GestureInput::DockSwipe`).
    let direction = platform_mac::gesture::dock_direction(axis);
    Some(InputMsg::Gesture(GestureInput::DockSwipe {
        axis,
        phase,
        progress: (progress * direction) as f32,
        velocity_x: (velocity_x * direction) as f32,
        velocity_y: (velocity_y * direction) as f32,
        inverted,
    }))
}

/// Pinch to zoom at `x`, `y`: `delta` as `NSEvent.magnification`. `phase` as for
/// `lk_input_dock_swipe` (map from `NSEvent.Phase` first).
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_magnify(session: u64, x: f64, y: f64, phase: u8, delta: f64) {
    let Some(phase) = GesturePhase::from_bits(phase) else { return };
    send(session, InputMsg::Gesture(GestureInput::Magnify { x: pos(x), y: pos(y), phase, delta: delta as f32 }));
}

/// Two-finger rotation at `x`, `y`: `degrees` as `NSEvent.rotation`.
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_rotate(session: u64, x: f64, y: f64, phase: u8, degrees: f64) {
    let Some(phase) = GesturePhase::from_bits(phase) else { return };
    send(session, InputMsg::Gesture(GestureInput::Rotate { x: pos(x), y: pos(y), phase, degrees: degrees as f32 }));
}

/// Two-finger double tap (smart zoom) at `x`, `y`.
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_smart_magnify(session: u64, x: f64, y: f64) {
    send(session, InputMsg::Gesture(GestureInput::SmartMagnify { x: pos(x), y: pos(y) }));
}

/// Swipe between pages at `x`, `y`: a swipe event's `deltaX`, `deltaY` (-1, 0 or 1).
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_navigation_swipe(session: u64, x: f64, y: f64, dx: i8, dy: i8) {
    send(session, InputMsg::Gesture(GestureInput::NavigationSwipe { x: pos(x), y: pos(y), dx: dx.signum(), dy: dy.signum() }));
}

/// Mission Control, Show Desktop, a Space left or right... on the remote Mac (`LK_SYSTEM_*` in
/// lankvm.h). A host that doesn't know the action ignores it.
#[unsafe(no_mangle)]
pub extern "C" fn lk_input_system_action(session: u64, action: u16) {
    send(session, InputMsg::System(SystemAction(action)));
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

/// "Still here": call from the UI thread every 250 ms while anything is held or a gesture is in
/// progress, so the host can let go if this app hangs or disappears.
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

/// Whether paired Macs may connect over the internet (saved). On asks the router to forward the
/// port; off ends the sessions that came over the internet. Returns at once.
#[unsafe(no_mangle)]
pub extern "C" fn lk_set_internet_access(on: bool) {
    if let Some(c) = core() {
        c.set_internet_access(on);
    }
}

/// The address paired Macs use over the internet (a dynamic DNS name or an IP, with or without
/// a port; saved). "" (or null) clears it.
///
/// # Safety
/// `address` must be null or a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_set_public_address(address: *const c_char) {
    let address = unsafe { arg(address) };
    if let Some(c) = core() {
        c.set_public_address(&address);
    }
}

/// The LanKVM server ("host:port") that introduces paired Macs to this one over the internet,
/// with no router setup (saved). "" (or null) turns that off.
///
/// # Safety
/// `address` must be null or a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lk_set_rendezvous_server(address: *const c_char) {
    let address = unsafe { arg(address) };
    if let Some(c) = core() {
        c.set_rendezvous_server(&address);
    }
}

/// Whether macOS lets LanKVM post input (Privacy & Security → Accessibility).
#[unsafe(no_mangle)]
pub extern "C" fn lk_control_permission() -> bool {
    core().is_some_and(|c| c.control_permission())
}

/// Whether this user's session has the screen (fast user switching): while it doesn't, viewers
/// lose their virtual displays and can't add one. Returns at once.
#[unsafe(no_mangle)]
pub extern "C" fn lk_set_console_active(active: bool) {
    if let Some(c) = core() {
        c.set_console_active(active);
    }
}

/// Removes a virtual display made for a viewer (0: all of them). Returns at once.
#[unsafe(no_mangle)]
pub extern "C" fn lk_remove_virtual_display(display_id: u32) {
    if let Some(c) = core() {
        c.remove_virtual_display((display_id != 0).then_some(display_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::DisplayReason;

    #[test]
    fn positions_quantize_and_clamp() {
        assert_eq!(pos(0.0), 0);
        assert_eq!(pos(1.0), u16::MAX);
        assert_eq!(pos(0.5), 32768);
        assert_eq!(pos(-1.0), 0);
        assert_eq!(pos(7.0), u16::MAX);
        assert_eq!(pos(f64::NAN), 0);
    }

    /// The value of `#define name` in lankvm.h, which Swift passes through as it is.
    fn header_code(name: &str) -> u16 {
        u16::try_from(header_value(name)).unwrap_or_else(|_| panic!("{name} isn't a u16"))
    }

    fn header_value(name: &str) -> u64 {
        let header = include_str!("../../../macos/Sources/CLanKVM/include/lankvm.h");
        let line = header.lines().find(|l| l.split_whitespace().nth(1) == Some(name)).unwrap_or_else(|| panic!("no {name} in lankvm.h"));
        line.split_whitespace().nth(2).and_then(|v| v.parse().ok()).unwrap_or_else(|| panic!("bad {line:?}"))
    }

    #[test]
    fn header_display_codes_and_limits_match_the_protocol() {
        for (name, arrangement) in [("LK_ARRANGE_EXTEND", Arrangement::EXTEND), ("LK_ARRANGE_MAIN", Arrangement::MAIN), ("LK_ARRANGE_ONLY", Arrangement::ONLY)] {
            assert_eq!(Arrangement(header_code(name) as u8), arrangement, "{name}");
        }
        let reasons = [
            ("LK_DISPLAY_NONE", DisplayReason::NONE),
            ("LK_DISPLAY_INVALID", DisplayReason::INVALID),
            ("LK_DISPLAY_NOT_ALLOWED", DisplayReason::NOT_ALLOWED),
            ("LK_DISPLAY_UNSUPPORTED", DisplayReason::UNSUPPORTED),
            ("LK_DISPLAY_FAILED", DisplayReason::FAILED),
            ("LK_DISPLAY_REMOVED_BY_HOST", DisplayReason::REMOVED_BY_HOST),
            ("LK_DISPLAY_GONE", DisplayReason::DISPLAY_GONE),
            ("LK_DISPLAY_SAME_MAC", DisplayReason::SAME_MAC),
            ("LK_DISPLAY_IN_USE", DisplayReason::IN_USE),
            ("LK_DISPLAY_NO_VIDEO", DisplayReason::NO_VIDEO),
            ("LK_DISPLAY_TOO_MANY", DisplayReason::TOO_MANY),
        ];
        for (name, reason) in reasons {
            assert_eq!(DisplayReason(header_code(name)), reason, "{name}");
        }
        assert_eq!(header_value("LK_DISPLAY_MIN_WIDTH"), u64::from(VirtualDisplaySpec::MIN_WIDTH));
        assert_eq!(header_value("LK_DISPLAY_MIN_HEIGHT"), u64::from(VirtualDisplaySpec::MIN_HEIGHT));
        assert_eq!(header_value("LK_DISPLAY_MAX_SIDE"), u64::from(VirtualDisplaySpec::MAX_SIDE));
        assert_eq!(header_value("LK_DISPLAY_MAX_PIXELS"), VirtualDisplaySpec::MAX_PIXELS);
        assert_eq!(header_value("LK_DISPLAY_MAX_ASPECT"), u64::from(VirtualDisplaySpec::MAX_ASPECT));
    }

    #[test]
    fn header_codes_mean_what_the_core_reads() {
        let phases = [
            ("LK_PHASE_BEGAN", GesturePhase::Began),
            ("LK_PHASE_CHANGED", GesturePhase::Changed),
            ("LK_PHASE_ENDED", GesturePhase::Ended),
            ("LK_PHASE_CANCELLED", GesturePhase::Cancelled),
        ];
        for (name, phase) in phases {
            assert_eq!(GesturePhase::from_bits(header_code(name) as u8), Some(phase), "{name}");
        }
        let axes = [("LK_DOCK_HORIZONTAL", DockAxis::Horizontal), ("LK_DOCK_VERTICAL", DockAxis::Vertical), ("LK_DOCK_PINCH", DockAxis::Pinch)];
        for (name, axis) in axes {
            assert_eq!(DockAxis::from_motion(header_code(name) as u8), Some(axis), "{name}");
        }
        let actions = [
            ("LK_SYSTEM_MISSION_CONTROL", SystemAction::MISSION_CONTROL),
            ("LK_SYSTEM_APP_EXPOSE", SystemAction::APP_EXPOSE),
            ("LK_SYSTEM_SHOW_DESKTOP", SystemAction::SHOW_DESKTOP),
            ("LK_SYSTEM_LAUNCHPAD", SystemAction::LAUNCHPAD),
            ("LK_SYSTEM_PREVIOUS_SPACE", SystemAction::PREVIOUS_SPACE),
            ("LK_SYSTEM_NEXT_SPACE", SystemAction::NEXT_SPACE),
        ];
        for (name, action) in actions {
            assert_eq!(SystemAction(header_code(name)), action, "{name}");
        }
    }

    #[test]
    fn dock_swipes_take_trackpad_codes_and_values() {
        let factor = platform_mac::gesture::dock_direction(DockAxis::Vertical);
        let Some(InputMsg::Gesture(GestureInput::DockSwipe { axis, phase, progress, velocity_x, velocity_y, inverted })) =
            dock_swipe(2, 4, 0.5, 0.25, -3.0, true)
        else {
            panic!("not a dock swipe")
        };
        assert_eq!((axis, phase, inverted), (DockAxis::Vertical, GesturePhase::Ended, true));
        assert_eq!((progress, velocity_x, velocity_y), ((0.5 * factor) as f32, (0.25 * factor) as f32, (-3.0 * factor) as f32));
        assert_eq!(dock_swipe(0, 1, 0.0, 0.0, 0.0, false), None, "no such axis");
        assert_eq!(dock_swipe(4, 1, 0.0, 0.0, 0.0, false), None, "no such axis");
        // NSEvent.Phase raw values that aren't IOHID phase bits: mayBegin (32) and cancelled (16).
        assert_eq!(dock_swipe(1, 32, 0.0, 0.0, 0.0, false), None);
        assert_eq!(dock_swipe(1, 16, 0.0, 0.0, 0.0, false), None);
        assert_eq!(dock_swipe(1, 0, 0.0, 0.0, 0.0, false), None);
    }
}
