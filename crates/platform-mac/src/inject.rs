//! Remote input as Quartz events: the state machine that decides exactly what to post, and the
//! poster that posts it.
//!
//! [`InputState`] is plain data. It turns remote input into fully described [`Synth`] events:
//! moves become drags while a button is held, every press keeps one click count and event number
//! from down to up, every event carries the remote's modifiers, and everything held can be
//! released at once. [`Poster`] creates and posts the matching `CGEvent`s. Tests (and a recording
//! backend) look at the `Synth` events without touching the system.
//!
//! What macOS needs, learned from Sunshine, RustDesk, Deskflow and Chrome Remote Desktop:
//! - A private event-source state, so the host user's held keys never leak into remote events
//!   (and the reverse), with the flags set explicitly on every event, device bits included.
//! - Local events not suppressed after posting, so the host's own mouse never goes dead.
//! - Mouse and scroll posted at the HID tap, keyboard at the session tap (keeps left/right).
//! - Modifier keys posted as their own key codes, which `CGEventCreateKeyboardEvent` turns into
//!   flags-changed events.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFilterMask, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventSuppressionState, CGEventTapLocation, CGEventType, CGMouseButton, CGScrollEventUnit,
};

use crate::keys::{self, CAPS_LOCK, FN, KEY_CAPS_LOCK, KEYBOARD_EVENT_BIT, MODIFIERS, NUMERIC_PAD};

/// High half of the tag stamped into `kCGEventSourceUserData` of every event LanKVM posts
/// ("LKVM"). Below it: 8 bits of relay depth (see `protocol::InputMsg::Relayed`), then 24 bits
/// random per host. A LanKVM viewer on the same Mac recognises its host's tag and ignores that
/// input instead of sending it right back; any viewer reads the depth to pass it on.
pub const INJECTED_TAG_PREFIX: i64 = 0x4C4B_564D << 32;
const RELAY_DEPTH_SHIFT: u32 = 24;
const RELAY_DEPTH_MASK: i64 = 0xFF << RELAY_DEPTH_SHIFT;

/// A tag for this process's injected events: [`INJECTED_TAG_PREFIX`] plus 24 random bits (and
/// depth 0).
pub fn new_injected_tag() -> i64 {
    use std::hash::{BuildHasher, RandomState};
    INJECTED_TAG_PREFIX | (RandomState::new().hash_one(std::process::id()) as i64 & 0x00FF_FFFF)
}

/// The tag stamped on an event a host injects for input relayed `depth` times.
pub fn tag_with_depth(tag: i64, depth: u8) -> i64 {
    (tag & !RELAY_DEPTH_MASK) | (i64::from(depth) << RELAY_DEPTH_SHIFT)
}

/// For an event some LanKVM host injected (by its source user data): the relay depth it was
/// injected at. None for anything else.
pub fn injected_depth(user_data: i64) -> Option<u8> {
    (user_data >> 32 == INJECTED_TAG_PREFIX >> 32).then(|| ((user_data & RELAY_DEPTH_MASK) >> RELAY_DEPTH_SHIFT) as u8)
}

/// `isDirectionInvertedFromDevice` lives in this undocumented scroll field.
const FIELD_SCROLL_INVERTED: CGEventField = CGEventField(137);

/// A point in global display coordinates (points, top-left origin of the main display).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// A display's bounds in global points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Bounds {
    pub fn of_display(display_id: u32) -> Self {
        let r = CGDisplayBounds(display_id);
        Self { x: r.origin.x, y: r.origin.y, width: r.size.width, height: r.size.height }
    }

    /// Whether the display still exists (an unplugged display reports empty bounds).
    pub fn is_usable(&self) -> bool {
        self.width >= 1.0 && self.height >= 1.0
    }

    /// Maps a position normalized to the display (0..=1 on each axis) to a point on it. The
    /// result stays on the display, at most at its last point, so the menu bar, the Dock and hot
    /// corners at the very edge stay reachable.
    pub fn point_at(&self, nx: f64, ny: f64) -> Point {
        let nx = if nx.is_finite() { nx.clamp(0.0, 1.0) } else { 0.0 };
        let ny = if ny.is_finite() { ny.clamp(0.0, 1.0) } else { 0.0 };
        Point {
            x: (self.x + nx * self.width).min(self.x + (self.width - 1.0).max(0.0)),
            y: (self.y + ny * self.height).min(self.y + (self.height - 1.0).max(0.0)),
        }
    }
}

/// One scroll event, field for field as the viewer's Mac produced it, so both phases (trackpad
/// gesture and momentum) and precise deltas survive the trip. Axis `y` is vertical.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Scroll {
    /// Line deltas (`kCGScrollWheelEventDeltaAxis1/2`).
    pub lines_y: i32,
    pub lines_x: i32,
    /// Fractional line deltas (`FixedPtDeltaAxis1/2`).
    pub fixed_y: f64,
    pub fixed_x: f64,
    /// Pixel deltas (`PointDeltaAxis1/2`).
    pub pixels_y: i32,
    pub pixels_x: i32,
    /// Trackpad or Magic Mouse (precise) rather than a wheel.
    pub continuous: bool,
    /// `CGScrollPhase`: 0, Began 1, Changed 2, Ended 4, Cancelled 8, MayBegin 128.
    pub phase: u8,
    /// `CGMomentumScrollPhase`: 0, Begin 1, Continue 2, End 3.
    pub momentum: u8,
    /// Natural scrolling: the deltas are already inverted.
    pub inverted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Moved,
    Down,
    Up,
    Dragged,
}

/// A fully described event, ready to post.
#[derive(Debug, Clone, PartialEq)]
pub enum Synth {
    Mouse {
        kind: MouseKind,
        at: Point,
        /// 0 left, 1 right, 2 middle, 3+ other buttons.
        button: u8,
        click_state: i64,
        event_number: i64,
        dx: i64,
        dy: i64,
        flags: u64,
    },
    /// A key press or release. Modifier key codes post as flags-changed events.
    Key { code: u16, down: bool, autorepeat: bool, flags: u64 },
    Scroll { at: Point, scroll: Scroll, flags: u64 },
}

/// What has been pressed on the host on behalf of one remote viewer.
#[derive(Debug)]
pub struct InputState {
    /// Held modifiers, Caps Lock and fn, as `keys::normalize` describes them.
    mods: u64,
    keys: BTreeSet<u16>,
    /// Bit n set while button n is held.
    buttons: u32,
    press_click_state: i64,
    event_number: i64,
    pos: Point,
    /// Fractional pixels left over from integer mouse deltas.
    delta_rem: (f64, f64),
}

impl InputState {
    /// `pos` is where the cursor is now. `caps_lock` is the host's own Caps Lock state.
    /// `event_number` seeds the per-press mouse event numbers (macOS 27 needs them to rise).
    pub fn new(pos: Point, caps_lock: bool, event_number: i64) -> Self {
        Self {
            mods: if caps_lock { CAPS_LOCK } else { 0 },
            keys: BTreeSet::new(),
            buttons: 0,
            press_click_state: 1,
            event_number,
            pos,
            delta_rem: (0.0, 0.0),
        }
    }

    pub fn modifiers(&self) -> u64 {
        self.mods
    }

    pub fn position(&self) -> Point {
        self.pos
    }

    /// Whether any key, modifier or button is held (Caps Lock is a state, not a held key).
    pub fn holds_anything(&self) -> bool {
        !self.keys.is_empty() || self.buttons != 0 || self.mods & !CAPS_LOCK != 0
    }

    /// Brings the held modifiers, Caps Lock and fn to `target` (any flags; normalized here),
    /// posting a flags-changed event per modifier key that differs. Releases come first.
    pub fn set_modifiers(&mut self, target: u64, out: &mut Vec<Synth>) {
        let target = keys::normalize(target);
        for m in MODIFIERS.iter().rev() {
            if keys::is_down(*m, self.mods) && !keys::is_down(*m, target) {
                self.mods &= !m.device;
                let family_left = MODIFIERS.iter().any(|o| o.flag == m.flag && o.device != 0 && self.mods & o.device != 0);
                if m.device == 0 || !family_left {
                    self.mods &= !m.flag;
                }
                out.push(Synth::Key { code: m.key_code, down: false, autorepeat: false, flags: self.mods });
            }
        }
        if (self.mods ^ target) & CAPS_LOCK != 0 {
            self.mods ^= CAPS_LOCK;
            out.push(Synth::Key { code: KEY_CAPS_LOCK, down: self.mods & CAPS_LOCK != 0, autorepeat: false, flags: self.mods });
        }
        for m in MODIFIERS {
            if !keys::is_down(m, self.mods) && keys::is_down(m, target) {
                self.mods |= m.flag | m.device;
                out.push(Synth::Key { code: m.key_code, down: true, autorepeat: false, flags: self.mods });
            }
        }
    }

    /// A non-modifier key. Modifier and Caps Lock codes are ignored here: their state arrives
    /// through [`set_modifiers`](Self::set_modifiers). Unmatched releases and repeats of keys
    /// that aren't held are dropped, so a lost press can never leave a key stuck.
    pub fn key(&mut self, code: u16, down: bool, autorepeat: bool, out: &mut Vec<Synth>) {
        if code > keys::MAX_KEY_CODE || keys::is_modifier_or_caps(code) {
            return;
        }
        if down {
            // A repeat of a key that isn't held (it was released here, e.g. after the viewer went
            // silent) must not press it again: its release might never come.
            if autorepeat && !self.keys.contains(&code) {
                return;
            }
            let fresh = self.keys.insert(code);
            out.push(self.key_event(code, true, autorepeat && !fresh));
        } else if self.keys.remove(&code) {
            out.push(self.key_event(code, false, false));
        }
    }

    fn key_event(&self, code: u16, down: bool, autorepeat: bool) -> Synth {
        let mut intrinsic = keys::intrinsic_flags(code);
        // Releasing an arrow or F-key with its built-in fn bit can leave fn "stuck" in the
        // session's flags (RustDesk/rdev workaround). Keep it only when fn is really held.
        if !down && self.mods & FN == 0 {
            intrinsic &= !(FN | NUMERIC_PAD);
        }
        Synth::Key { code, down, autorepeat, flags: self.mods | intrinsic }
    }

    /// Moves the cursor; a drag while a button is held.
    pub fn move_to(&mut self, to: Point, out: &mut Vec<Synth>) {
        let dx = to.x - self.pos.x + self.delta_rem.0;
        let dy = to.y - self.pos.y + self.delta_rem.1;
        let (ix, iy) = (dx.round(), dy.round());
        self.delta_rem = (dx - ix, dy - iy);
        let (kind, button) = match self.lowest_button() {
            Some(b) => (MouseKind::Dragged, b),
            None => (MouseKind::Moved, 0),
        };
        let (click_state, event_number) =
            if kind == MouseKind::Dragged { (self.press_click_state, self.event_number) } else { (0, 0) };
        out.push(Synth::Mouse {
            kind,
            at: to,
            button,
            click_state,
            event_number,
            dx: ix as i64,
            dy: iy as i64,
            flags: self.mods,
        });
        self.pos = to;
    }

    /// Presses or releases `button` at `at` (moving there first if needed). `click_count` is the
    /// viewer's own count (1 single, 2 double...), kept for the whole press. A press of a held
    /// button or a release of one that isn't held is ignored.
    pub fn button(&mut self, button: u8, down: bool, click_count: u8, at: Point, out: &mut Vec<Synth>) {
        if button >= 32 {
            return;
        }
        let bit = 1u32 << button;
        if down == (self.buttons & bit != 0) {
            return;
        }
        if at != self.pos {
            self.move_to(at, out);
        }
        if down {
            self.event_number += 1;
            self.press_click_state = i64::from(click_count.max(1));
            self.buttons |= bit;
        } else {
            self.buttons &= !bit;
        }
        out.push(Synth::Mouse {
            kind: if down { MouseKind::Down } else { MouseKind::Up },
            at: self.pos,
            button,
            click_state: self.press_click_state,
            event_number: self.event_number,
            dx: 0,
            dy: 0,
            flags: self.mods,
        });
    }

    /// Scrolls at `at` (moving there first if needed): scroll goes to the window under the cursor.
    pub fn scroll(&mut self, scroll: Scroll, at: Point, out: &mut Vec<Synth>) {
        if at != self.pos {
            self.move_to(at, out);
        }
        out.push(Synth::Scroll { at: self.pos, scroll, flags: self.mods });
    }

    /// Releases held mouse buttons only (e.g. when the display they were pressed on went away).
    pub fn release_buttons(&mut self, out: &mut Vec<Synth>) {
        while let Some(button) = self.lowest_button() {
            self.button(button, false, 0, self.pos, out);
        }
    }

    /// Releases every key, button and modifier, and puts Caps Lock back to the host's own state.
    /// Buttons go up while the modifiers are still down, so a drag in progress (⌥-drag to copy)
    /// drops the way it was meant to.
    pub fn release_all(&mut self, host_caps_lock: bool, out: &mut Vec<Synth>) {
        for code in std::mem::take(&mut self.keys) {
            out.push(self.key_event(code, false, false));
        }
        while let Some(button) = self.lowest_button() {
            self.button(button, false, 0, self.pos, out);
        }
        self.set_modifiers(if host_caps_lock { CAPS_LOCK } else { 0 }, out);
    }

    fn lowest_button(&self) -> Option<u8> {
        (self.buttons != 0).then(|| self.buttons.trailing_zeros() as u8)
    }
}

/// Posts [`Synth`] events as tagged Quartz events from a private event source.
pub struct Poster {
    source: CFRetained<CGEventSource>,
    tag: i64,
    /// Post to this process only (tests), instead of to the whole session.
    pid: Option<i32>,
}

// SAFETY: a CGEventSource is a thread-safe CoreFoundation object; the poster is used from one
// thread at a time.
unsafe impl Send for Poster {}

impl Poster {
    /// Events are stamped with `tag` (see [`new_injected_tag`]).
    pub fn new(tag: i64) -> Result<Self> {
        let source = CGEventSource::new(CGEventSourceStateID::Private).context("create event source")?;
        let src = Some(&*source);
        CGEventSource::set_user_data(src, tag);
        // Never mute the host's own mouse and keyboard after we post (default: 0.25 s).
        CGEventSource::set_local_events_suppression_interval(src, 0.0);
        let all = CGEventFilterMask::PermitLocalMouseEvents
            | CGEventFilterMask::PermitLocalKeyboardEvents
            | CGEventFilterMask::PermitSystemDefinedEvents;
        CGEventSource::set_local_events_filter_during_suppression_state(
            src,
            all,
            CGEventSuppressionState::EventSuppressionStateSuppressionInterval,
        );
        CGEventSource::set_local_events_filter_during_suppression_state(
            src,
            all,
            CGEventSuppressionState::EventSuppressionStateRemoteMouseDrag,
        );
        Ok(Self { source, tag, pid: None })
    }

    /// Like [`new`](Self::new), but delivers every event to process `pid` only, wherever the
    /// cursor and focus are (for tests: control one app without touching the rest of the Mac).
    pub fn for_pid(tag: i64, pid: i32) -> Result<Self> {
        Ok(Self { pid: Some(pid), ..Self::new(tag)? })
    }

    fn send(&self, tap: CGEventTapLocation, event: &CGEvent, depth: u8) {
        if depth != 0 {
            CGEvent::set_integer_value_field(Some(event), CGEventField::EventSourceUserData, tag_with_depth(self.tag, depth));
        }
        match self.pid {
            Some(pid) => CGEvent::post_to_pid(pid, Some(event)),
            None => CGEvent::post(tap, Some(event)),
        }
    }

    /// Posts `synth`, stamped with the relay depth of the input it came from.
    pub fn post(&self, synth: &Synth, depth: u8) {
        let src = Some(&*self.source);
        match *synth {
            Synth::Mouse { kind, at, button, click_state, event_number, dx, dy, flags } => {
                let ty = mouse_type(kind, button);
                let Some(e) = CGEvent::new_mouse_event(src, ty, cg_point(at), CGMouseButton(u32::from(button))) else {
                    return;
                };
                let e = Some(&*e);
                CGEvent::set_integer_value_field(e, CGEventField::MouseEventButtonNumber, i64::from(button));
                CGEvent::set_integer_value_field(e, CGEventField::MouseEventDeltaX, dx);
                CGEvent::set_integer_value_field(e, CGEventField::MouseEventDeltaY, dy);
                if kind != MouseKind::Moved {
                    CGEvent::set_integer_value_field(e, CGEventField::MouseEventClickState, click_state);
                    CGEvent::set_integer_value_field(e, CGEventField::MouseEventNumber, event_number);
                }
                CGEvent::set_flags(e, CGEventFlags(flags));
                self.send(CGEventTapLocation::HIDEventTap, e.expect("created above"), depth);
            }
            Synth::Key { code, down, autorepeat, flags } => {
                let Some(e) = CGEvent::new_keyboard_event(src, code, down) else { return };
                let e = Some(&*e);
                CGEvent::set_flags(e, CGEventFlags(flags | KEYBOARD_EVENT_BIT));
                if autorepeat {
                    CGEvent::set_integer_value_field(e, CGEventField::KeyboardEventAutorepeat, 1);
                }
                self.send(CGEventTapLocation::SessionEventTap, e.expect("created above"), depth);
            }
            Synth::Scroll { at, scroll: s, flags } => {
                let unit = if s.continuous { CGScrollEventUnit::Pixel } else { CGScrollEventUnit::Line };
                let Some(e) = CGEvent::new_scroll_wheel_event2(src, unit, 2, s.lines_y, s.lines_x, 0) else { return };
                let e = Some(&*e);
                CGEvent::set_integer_value_field(e, CGEventField::ScrollWheelEventDeltaAxis1, i64::from(s.lines_y));
                CGEvent::set_integer_value_field(e, CGEventField::ScrollWheelEventDeltaAxis2, i64::from(s.lines_x));
                CGEvent::set_double_value_field(e, CGEventField::ScrollWheelEventFixedPtDeltaAxis1, s.fixed_y);
                CGEvent::set_double_value_field(e, CGEventField::ScrollWheelEventFixedPtDeltaAxis2, s.fixed_x);
                CGEvent::set_integer_value_field(e, CGEventField::ScrollWheelEventPointDeltaAxis1, i64::from(s.pixels_y));
                CGEvent::set_integer_value_field(e, CGEventField::ScrollWheelEventPointDeltaAxis2, i64::from(s.pixels_x));
                CGEvent::set_integer_value_field(e, CGEventField::ScrollWheelEventIsContinuous, i64::from(s.continuous));
                CGEvent::set_integer_value_field(e, CGEventField::ScrollWheelEventScrollPhase, i64::from(s.phase));
                CGEvent::set_integer_value_field(e, CGEventField::ScrollWheelEventMomentumPhase, i64::from(s.momentum));
                if s.inverted {
                    CGEvent::set_integer_value_field(e, FIELD_SCROLL_INVERTED, 1);
                }
                CGEvent::set_location(e, cg_point(at));
                CGEvent::set_flags(e, CGEventFlags(flags));
                self.send(CGEventTapLocation::HIDEventTap, e.expect("created above"), depth);
            }
        }
    }
}

/// A safety net for tests that inject real input on a Mac someone uses: lets events through only
/// where they would reach one process. Pointer events must land on a window of that process that
/// is front-most at that point; key presses need that process's window in front. The release of
/// a delivered press always passes (nothing may stay held); that of a dropped press is dropped too.
pub struct InjectGuard {
    pid: i32,
    windows: Vec<WindowInfo>,
    refreshed: Option<std::time::Instant>,
    /// Keys whose press got through, so their release must too.
    delivered_keys: BTreeSet<u16>,
    dropped_buttons: u32,
    pub dropped: u64,
}

/// An on-screen window: owner and bounds (global points), front to back.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowInfo {
    pub pid: i32,
    pub layer: i64,
    pub bounds: Bounds,
}

impl InjectGuard {
    pub fn new(pid: i32) -> Self {
        Self { pid, windows: Vec::new(), refreshed: None, delivered_keys: BTreeSet::new(), dropped_buttons: 0, dropped: 0 }
    }

    pub fn allows(&mut self, synth: &Synth) -> bool {
        if self.refreshed.is_none_or(|t| t.elapsed() > std::time::Duration::from_millis(200)) {
            self.windows = on_screen_windows();
            self.refreshed = Some(std::time::Instant::now());
        }
        let ok = self.judge(synth);
        if !ok {
            self.dropped += 1;
        }
        ok
    }

    /// The decision, against the current window list.
    pub fn judge(&mut self, synth: &Synth) -> bool {
        match *synth {
            Synth::Key { code, .. } if keys::modifier(code).is_some() || code == KEY_CAPS_LOCK => true,
            Synth::Key { code, down: true, autorepeat, .. } => {
                let front = self.windows.iter().find(|w| w.layer == 0).map(|w| w.pid);
                let ok = front == Some(self.pid);
                if autorepeat {
                    // A repeat goes only where its press went, and only while the target is
                    // still in front. Either way the release follows the press.
                    ok && self.delivered_keys.contains(&code)
                } else {
                    if ok {
                        self.delivered_keys.insert(code);
                    }
                    ok
                }
            }
            Synth::Key { code, down: false, .. } => self.delivered_keys.remove(&code),
            Synth::Mouse { kind: MouseKind::Up, button, .. } => {
                let bit = 1u32 << button.min(31);
                let was_dropped = self.dropped_buttons & bit != 0;
                self.dropped_buttons &= !bit;
                !was_dropped
            }
            Synth::Mouse { kind, at, button, .. } => {
                let ok = self.owner_at(at) == Some(self.pid);
                if !ok && kind == MouseKind::Down {
                    self.dropped_buttons |= 1u32 << button.min(31);
                }
                ok
            }
            Synth::Scroll { at, .. } => self.owner_at(at) == Some(self.pid),
        }
    }

    /// Which process's window gets a click at `at`: the front-most window containing it. Only
    /// ordinary layers count (documents, panels, floating windows): the Dock, the menu bar and
    /// screen overlays (layer 20 and up) cover the whole screen with windows clicks pass through.
    fn owner_at(&self, at: Point) -> Option<i32> {
        self.windows
            .iter()
            .filter(|w| (0..Self::OVERLAY_LAYER).contains(&w.layer))
            .find(|w| at.x >= w.bounds.x && at.x < w.bounds.x + w.bounds.width && at.y >= w.bounds.y && at.y < w.bounds.y + w.bounds.height)
            .map(|w| w.pid)
    }

    /// `kCGDockWindowLevel`: the Dock and everything above it (menu bar, overlays).
    const OVERLAY_LAYER: i64 = 20;

    pub fn set_windows(&mut self, windows: Vec<WindowInfo>) {
        self.windows = windows;
        self.refreshed = Some(std::time::Instant::now());
    }
}

/// On-screen windows, front to back.
pub fn on_screen_windows() -> Vec<WindowInfo> {
    use objc2_core_graphics::{CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID};
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};
    let Some(list) = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
        return Vec::new();
    };
    // SAFETY: CFArray of CFDictionary is toll-free bridged to NSArray of NSDictionary.
    let list: &NSArray<NSDictionary<NSString, objc2::runtime::AnyObject>> =
        unsafe { &*(&*list as *const _ as *const NSArray<NSDictionary<NSString, objc2::runtime::AnyObject>>) };
    let number = |d: &NSDictionary<NSString, objc2::runtime::AnyObject>, key: &str| -> Option<f64> {
        let v = d.objectForKey(&NSString::from_str(key))?;
        v.downcast_ref::<NSNumber>().map(|n| n.doubleValue())
    };
    list.iter()
        .filter_map(|w| {
            let pid = number(&w, "kCGWindowOwnerPID")? as i32;
            let layer = number(&w, "kCGWindowLayer")? as i64;
            let b = w.objectForKey(&NSString::from_str("kCGWindowBounds"))?;
            let b = b.downcast_ref::<NSDictionary>()?;
            // SAFETY: the bounds dictionary has string keys (X, Y, Width, Height).
            let b: &NSDictionary<NSString, objc2::runtime::AnyObject> = unsafe { &*(b as *const NSDictionary as *const _) };
            let bounds = Bounds { x: number(b, "X")?, y: number(b, "Y")?, width: number(b, "Width")?, height: number(b, "Height")? };
            Some(WindowInfo { pid, layer, bounds })
        })
        .collect()
}

fn mouse_type(kind: MouseKind, button: u8) -> CGEventType {
    match (kind, button) {
        (MouseKind::Moved, _) => CGEventType::MouseMoved,
        (MouseKind::Down, 0) => CGEventType::LeftMouseDown,
        (MouseKind::Up, 0) => CGEventType::LeftMouseUp,
        (MouseKind::Dragged, 0) => CGEventType::LeftMouseDragged,
        (MouseKind::Down, 1) => CGEventType::RightMouseDown,
        (MouseKind::Up, 1) => CGEventType::RightMouseUp,
        (MouseKind::Dragged, 1) => CGEventType::RightMouseDragged,
        (MouseKind::Down, _) => CGEventType::OtherMouseDown,
        (MouseKind::Up, _) => CGEventType::OtherMouseUp,
        (MouseKind::Dragged, _) => CGEventType::OtherMouseDragged,
    }
}

fn cg_point(p: Point) -> CGPoint {
    CGPoint { x: p.x, y: p.y }
}

/// Where the cursor is now, in global points.
pub fn cursor_position() -> Point {
    let p = CGEvent::new(None).map(|e| CGEvent::location(Some(&e))).unwrap_or(CGPoint { x: 0.0, y: 0.0 });
    Point { x: p.x, y: p.y }
}

/// What the whole login session holds right now (all sources, local and injected): its modifier
/// flags and which mouse buttons are down. For tests checking that nothing was left stuck.
pub fn session_input_state() -> (u64, Vec<u8>) {
    let flags = CGEventSource::flags_state(CGEventSourceStateID::CombinedSessionState).0;
    let buttons = (0..8u8)
        .filter(|b| CGEventSource::button_state(CGEventSourceStateID::CombinedSessionState, CGMouseButton(u32::from(*b))))
        .collect();
    (flags, buttons)
}

/// Whether the host's own Caps Lock is on.
pub fn host_caps_lock() -> bool {
    CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState).0 & CAPS_LOCK != 0
}

/// A starting point for per-press mouse event numbers that continues the system's own count.
pub fn mouse_event_number_seed() -> i64 {
    [CGEventType::LeftMouseDown, CGEventType::RightMouseDown, CGEventType::OtherMouseDown]
        .into_iter()
        .map(|t| i64::from(CGEventSource::counter_for_event_type(CGEventSourceStateID::HIDSystemState, t)))
        .sum()
}

/// The first keyboard event a process creates loads the keyboard layout, and only works on the
/// main thread; created elsewhere, every later key event lacks its characters and is ~100x
/// slower to create. Call once on the main thread before injecting.
pub fn warm_up() {
    let _ = CGEvent::new_keyboard_event(None, 0, true);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{COMMAND, DEVICE_LCMD, DEVICE_LSHIFT, DEVICE_RSHIFT, SHIFT};

    fn p(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    fn state() -> InputState {
        InputState::new(p(100.0, 100.0), false, 1000)
    }

    #[test]
    fn guard_lets_input_reach_only_the_target() {
        let target = 4242;
        let mut g = InjectGuard::new(target);
        let win = |pid, layer, x, y, w, h| WindowInfo { pid, layer, bounds: Bounds { x, y, width: w, height: h } };
        // A floating window of another app covers part of the target's window; full-screen
        // click-through overlays (Dock, screen overlays) are above everything.
        g.set_windows(vec![
            win(9, 1000, 0.0, 0.0, 2000.0, 1000.0),
            win(8, 20, 0.0, 0.0, 2000.0, 1000.0),
            win(1, 3, 100.0, 100.0, 50.0, 50.0),
            win(target, 0, 0.0, 0.0, 500.0, 500.0),
            win(2, 0, 0.0, 0.0, 2000.0, 1000.0),
        ]);
        let mouse = |kind, x, y, button| Synth::Mouse { kind, at: p(x, y), button, click_state: 1, event_number: 1, dx: 0, dy: 0, flags: 0 };
        assert!(g.judge(&mouse(MouseKind::Moved, 10.0, 10.0, 0)));
        assert!(!g.judge(&mouse(MouseKind::Moved, 120.0, 120.0, 0)), "covered by another app's window");
        assert!(!g.judge(&mouse(MouseKind::Down, 900.0, 10.0, 0)), "outside the target");
        assert!(!g.judge(&mouse(MouseKind::Up, 10.0, 10.0, 0)), "the up of a dropped down is dropped");
        assert!(g.judge(&mouse(MouseKind::Down, 10.0, 10.0, 1)));
        assert!(g.judge(&mouse(MouseKind::Up, 900.0, 10.0, 1)), "ups always pass");
        let scroll = Synth::Scroll { at: p(10.0, 10.0), scroll: Scroll::default(), flags: 0 };
        assert!(g.judge(&scroll));
        // Keys: only while the target's window is in front.
        let key = |code, down| Synth::Key { code, down, autorepeat: false, flags: 0 };
        assert!(g.judge(&key(0, true)));
        assert!(g.judge(&key(0, false)));
        g.set_windows(vec![win(2, 0, 0.0, 0.0, 2000.0, 1000.0), win(target, 0, 0.0, 0.0, 500.0, 500.0)]);
        assert!(!g.judge(&key(1, true)));
        assert!(!g.judge(&key(1, false)), "release of a dropped press is dropped");
        assert!(g.judge(&key(55, true)), "modifier state always passes");
        assert!(g.judge(&key(55, false)));
        // A key held while windows change: the release follows the press, not the repeats.
        let repeat = |code| Synth::Key { code, down: true, autorepeat: true, flags: 0 };
        let target_front = vec![win(target, 0, 0.0, 0.0, 500.0, 500.0)];
        let other_front = vec![win(2, 0, 0.0, 0.0, 2000.0, 1000.0), win(target, 0, 0.0, 0.0, 500.0, 500.0)];
        g.set_windows(target_front.clone());
        assert!(g.judge(&key(2, true)));
        g.set_windows(other_front.clone());
        assert!(!g.judge(&repeat(2)));
        assert!(g.judge(&key(2, false)), "a delivered press is always released");
        assert!(!g.judge(&key(3, true)));
        g.set_windows(target_front);
        assert!(!g.judge(&repeat(3)), "no repeats of a dropped press");
        assert!(!g.judge(&key(3, false)));
    }

    #[test]
    fn lists_on_screen_windows() {
        // Needs no permission for owners and bounds.
        let windows = on_screen_windows();
        assert!(windows.iter().all(|w| w.bounds.width >= 0.0 && w.bounds.height >= 0.0));
    }

    #[test]
    fn injected_tags_are_recognisable_and_differ() {
        let (a, b) = (new_injected_tag(), new_injected_tag());
        assert_eq!(a >> 32, INJECTED_TAG_PREFIX >> 32);
        assert_ne!(a, b);
        assert_eq!(injected_depth(a), Some(0));
        let deep = tag_with_depth(a, 3);
        assert_eq!(injected_depth(deep), Some(3));
        assert_eq!(tag_with_depth(deep, 0), a, "the depth doesn't touch the host's own bits");
        assert_eq!(injected_depth(0), None);
        assert_eq!(injected_depth(12345), None);
    }

    #[test]
    fn maps_normalized_positions_onto_the_display() {
        let b = Bounds { x: 0.0, y: 0.0, width: 2056.0, height: 1329.0 };
        assert_eq!(b.point_at(0.0, 0.0), p(0.0, 0.0));
        assert_eq!(b.point_at(0.5, 0.5), p(1028.0, 664.5));
        assert_eq!(b.point_at(1.0, 1.0), p(2055.0, 1328.0));
        assert_eq!(b.point_at(-3.0, 7.0), p(0.0, 1328.0));
        assert_eq!(b.point_at(f64::NAN, f64::INFINITY), p(0.0, 0.0));
        let secondary = Bounds { x: -1920.0, y: -200.0, width: 1920.0, height: 1080.0 };
        assert_eq!(secondary.point_at(0.0, 1.0), p(-1920.0, 879.0));
    }

    #[test]
    fn moves_become_drags_while_a_button_is_held() {
        let mut s = state();
        let mut out = Vec::new();
        s.move_to(p(110.0, 100.0), &mut out);
        s.button(0, true, 1, p(110.0, 100.0), &mut out);
        s.move_to(p(120.5, 101.0), &mut out);
        s.button(0, false, 1, p(130.0, 101.0), &mut out);
        let kinds: Vec<_> = out
            .iter()
            .map(|e| match e {
                Synth::Mouse { kind, .. } => *kind,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(kinds, [MouseKind::Moved, MouseKind::Down, MouseKind::Dragged, MouseKind::Dragged, MouseKind::Up]);
        // The up lands where the viewer released, after a final drag there.
        assert!(matches!(out[4], Synth::Mouse { at, .. } if at == p(130.0, 101.0)));
    }

    #[test]
    fn a_press_keeps_its_click_count_and_event_number() {
        let mut s = state();
        let mut out = Vec::new();
        s.button(0, true, 2, p(100.0, 100.0), &mut out);
        s.move_to(p(150.0, 100.0), &mut out);
        s.button(0, false, 2, p(150.0, 100.0), &mut out);
        for e in &out {
            let Synth::Mouse { click_state, event_number, .. } = e else { panic!() };
            assert_eq!((*click_state, *event_number), (2, 1001));
        }
        s.button(0, true, 3, p(150.0, 100.0), &mut out);
        assert!(matches!(out.last(), Some(Synth::Mouse { click_state: 3, event_number: 1002, .. })));
    }

    #[test]
    fn right_and_other_drags_use_their_button() {
        let mut s = state();
        let mut out = Vec::new();
        s.button(1, true, 1, p(100.0, 100.0), &mut out);
        s.move_to(p(101.0, 100.0), &mut out);
        assert!(matches!(out.last(), Some(Synth::Mouse { kind: MouseKind::Dragged, button: 1, .. })));
        assert_eq!(mouse_type(MouseKind::Dragged, 1), CGEventType::RightMouseDragged);
        s.button(1, false, 1, p(101.0, 100.0), &mut out);
        s.button(3, true, 1, p(101.0, 100.0), &mut out);
        s.move_to(p(102.0, 100.0), &mut out);
        assert!(matches!(out.last(), Some(Synth::Mouse { kind: MouseKind::Dragged, button: 3, .. })));
        assert_eq!(mouse_type(MouseKind::Dragged, 3), CGEventType::OtherMouseDragged);
    }

    #[test]
    fn duplicate_and_unmatched_buttons_are_ignored() {
        let mut s = state();
        let mut out = Vec::new();
        s.button(0, false, 1, p(100.0, 100.0), &mut out);
        assert!(out.is_empty());
        s.button(0, true, 1, p(100.0, 100.0), &mut out);
        s.button(0, true, 1, p(100.0, 100.0), &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn integer_deltas_carry_the_fraction() {
        let mut s = state();
        let mut out = Vec::new();
        for _ in 0..4 {
            s.move_to(Point { x: s.position().x + 0.5, y: 100.0 }, &mut out);
        }
        let total: i64 = out.iter().map(|e| if let Synth::Mouse { dx, .. } = e { *dx } else { 0 }).sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn modifiers_sync_left_and_right_and_reach_every_event() {
        let mut s = state();
        let mut out = Vec::new();
        s.set_modifiers(COMMAND | DEVICE_LCMD | SHIFT | DEVICE_RSHIFT, &mut out);
        let codes: Vec<_> = out.iter().map(|e| if let Synth::Key { code, down, .. } = e { (*code, *down) } else { panic!() }).collect();
        assert_eq!(codes, [(60, true), (55, true)]);
        out.clear();
        s.key(0, true, false, &mut out);
        s.button(0, true, 1, p(100.0, 100.0), &mut out);
        let want = COMMAND | DEVICE_LCMD | SHIFT | DEVICE_RSHIFT;
        assert!(matches!(out[0], Synth::Key { code: 0, down: true, flags, .. } if flags == want));
        assert!(matches!(out[1], Synth::Mouse { flags, .. } if flags == want));
        // Releasing right shift while left shift goes down keeps the shift flag.
        out.clear();
        s.set_modifiers(COMMAND | DEVICE_LCMD | SHIFT | DEVICE_LSHIFT, &mut out);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], Synth::Key { code: 60, down: false, flags, .. } if flags == COMMAND | DEVICE_LCMD));
        assert!(matches!(out[1], Synth::Key { code: 56, down: true, .. }));
        out.clear();
        s.set_modifiers(COMMAND | DEVICE_LCMD | SHIFT | DEVICE_LSHIFT, &mut out);
        assert!(out.is_empty(), "no change, no events");
    }

    #[test]
    fn caps_lock_is_a_state() {
        let mut s = state();
        let mut out = Vec::new();
        s.set_modifiers(CAPS_LOCK, &mut out);
        assert_eq!(out, [Synth::Key { code: KEY_CAPS_LOCK, down: true, autorepeat: false, flags: CAPS_LOCK }]);
        out.clear();
        s.key(0, true, false, &mut out);
        assert!(matches!(out[0], Synth::Key { flags: CAPS_LOCK, .. }));
        s.key(0, false, false, &mut out);
        assert!(!s.holds_anything(), "caps lock on is not something held");
        out.clear();
        s.release_all(false, &mut out);
        assert!(out.contains(&Synth::Key { code: KEY_CAPS_LOCK, down: false, autorepeat: false, flags: 0 }));
    }

    #[test]
    fn modifier_codes_in_key_events_are_ignored() {
        let mut s = state();
        let mut out = Vec::new();
        s.key(55, true, false, &mut out);
        s.key(KEY_CAPS_LOCK, true, false, &mut out);
        s.key(200, true, false, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn repeats_and_releases_need_a_held_key() {
        let mut s = state();
        let mut out = Vec::new();
        s.key(0, false, false, &mut out);
        assert!(out.is_empty(), "unmatched release dropped");
        s.key(0, true, true, &mut out);
        assert!(out.is_empty(), "a repeat of a key that isn't held is dropped");
        s.key(0, true, false, &mut out);
        s.key(0, true, true, &mut out);
        assert!(matches!(out[1], Synth::Key { down: true, autorepeat: true, .. }));
        s.key(0, false, false, &mut out);
        s.key(0, false, false, &mut out);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn arrow_keys_carry_their_own_flags_but_do_not_stick_fn() {
        let mut s = state();
        let mut out = Vec::new();
        s.key(123, true, false, &mut out);
        s.key(123, false, false, &mut out);
        assert!(matches!(out[0], Synth::Key { flags, .. } if flags == NUMERIC_PAD | FN));
        assert!(matches!(out[1], Synth::Key { flags: 0, .. }));
    }

    #[test]
    fn release_all_lets_go_of_everything() {
        let mut s = state();
        let mut out = Vec::new();
        s.set_modifiers(COMMAND | DEVICE_LCMD | FN, &mut out);
        s.key(0, true, false, &mut out);
        s.key(1, true, false, &mut out);
        s.button(0, true, 1, p(10.0, 10.0), &mut out);
        s.button(2, true, 1, p(10.0, 10.0), &mut out);
        assert!(s.holds_anything());
        out.clear();
        s.release_all(false, &mut out);
        assert!(!s.holds_anything());
        let ups: Vec<_> = out
            .iter()
            .map(|e| match e {
                Synth::Key { code, down: false, .. } => format!("key{code}"),
                Synth::Mouse { kind: MouseKind::Up, button, .. } => format!("button{button}"),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(ups, ["key0", "key1", "button0", "button2", "key63", "key55"], "buttons before modifiers, which go in reverse press order");
        out.clear();
        s.release_all(false, &mut out);
        assert!(out.is_empty(), "nothing left to release");
    }

    #[test]
    fn release_all_restores_the_host_caps_lock() {
        let mut s = InputState::new(p(0.0, 0.0), true, 0);
        let mut out = Vec::new();
        s.set_modifiers(0, &mut out);
        assert_eq!(out, [Synth::Key { code: KEY_CAPS_LOCK, down: false, autorepeat: false, flags: 0 }]);
        out.clear();
        s.release_all(true, &mut out);
        assert_eq!(out, [Synth::Key { code: KEY_CAPS_LOCK, down: true, autorepeat: false, flags: CAPS_LOCK }]);
    }

    #[test]
    fn scroll_moves_to_its_position_first() {
        let mut s = state();
        let mut out = Vec::new();
        let scroll = Scroll { pixels_y: -12, continuous: true, phase: 2, ..Default::default() };
        s.scroll(scroll, p(300.0, 200.0), &mut out);
        assert!(matches!(out[0], Synth::Mouse { kind: MouseKind::Moved, .. }));
        assert_eq!(out[1], Synth::Scroll { at: p(300.0, 200.0), scroll, flags: 0 });
        s.scroll(scroll, p(300.0, 200.0), &mut out);
        assert_eq!(out.len(), 3);
    }
}
