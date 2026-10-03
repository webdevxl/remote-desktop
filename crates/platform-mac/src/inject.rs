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
//!
//! Trackpad gestures are held state like buttons: one at a time, ended as cancelled by every
//! release, so the Dock is never left mid-transition. How their events are built is in
//! [`crate::gesture`].

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFilterMask, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventSuppressionState, CGEventTapLocation, CGEventType, CGMouseButton, CGScrollEventUnit,
};
/// The wire's gesture phases and Dock axes, which are plain data, used here as they are.
pub use protocol::{DockAxis, GesturePhase};

use crate::gesture::{self, DockModes, DockRecipe, DockSample, PROGRESS_EPSILON};
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
    /// A swipe the Dock acts on, in this Mac's own direction convention (the wire's values times
    /// `gesture::dock_direction`).
    DockSwipe { axis: DockAxis, phase: GesturePhase, progress: f64, velocity_x: f64, velocity_y: f64, inverted: bool },
    /// A pinch or rotation for the window it began over: `value` is the magnification or the
    /// degrees since the previous event.
    AppGesture { at: Point, phase: GesturePhase, kind: AppGestureKind, value: f64, flags: u64 },
    /// A two-finger double tap.
    SmartMagnify { at: Point, flags: u64 },
    /// A swipe between pages: -1, 0 or 1 per axis, as `NSEvent.deltaX/Y`.
    NavigationSwipe { at: Point, dx: i8, dy: i8, flags: u64 },
    /// Something done to the Mac as a whole rather than posted as an event.
    System(SystemAction),
}

/// A gesture with phases, as [`InputState::gesture`] takes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gesture {
    /// Values in this Mac's own direction convention (the wire's times
    /// `gesture::dock_direction`).
    DockSwipe { axis: DockAxis, progress: f64, velocity_x: f64, velocity_y: f64, inverted: bool },
    /// `delta` as `NSEvent.magnification`, since the previous event.
    Magnify { delta: f64 },
    /// `degrees` as `NSEvent.rotation` (counterclockwise positive), since the previous event.
    Rotate { degrees: f64 },
}

/// A gesture with phases that goes to the window under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppGestureKind {
    Magnify,
    Rotate,
}

/// Something to do on the Mac as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAction {
    MissionControl,
    /// The front app's windows.
    AppExpose,
    ShowDesktop,
    /// Launchpad, or Apps from macOS 26 on.
    Launchpad,
    /// One Space (or full-screen app) to the left.
    PreviousSpace,
    /// One Space (or full-screen app) to the right.
    NextSpace,
}

impl SystemAction {
    /// The action a wire code names; None for a code this version doesn't know.
    pub fn from_wire(action: protocol::SystemAction) -> Option<Self> {
        Some(match action {
            protocol::SystemAction::MISSION_CONTROL => Self::MissionControl,
            protocol::SystemAction::APP_EXPOSE => Self::AppExpose,
            protocol::SystemAction::SHOW_DESKTOP => Self::ShowDesktop,
            protocol::SystemAction::LAUNCHPAD => Self::Launchpad,
            protocol::SystemAction::PREVIOUS_SPACE => Self::PreviousSpace,
            protocol::SystemAction::NEXT_SPACE => Self::NextSpace,
            _ => return None,
        })
    }
}

/// A gesture in progress.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HeldGesture {
    kind: HeldKind,
    /// Where a pinch or rotation began: AppKit sends all of it to the view under that point.
    at: Point,
    /// A Dock swipe's latest progress and flag, which its cancel repeats.
    progress: f64,
    inverted: bool,
    /// Swallowed here and recognised when it ends: its axis has no working Dock swipe recipe.
    discrete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeldKind {
    Dock(DockAxis),
    Magnify,
    Rotate,
}

impl HeldKind {
    fn of(gesture: &Gesture) -> Self {
        match *gesture {
            Gesture::DockSwipe { axis, .. } => Self::Dock(axis),
            Gesture::Magnify { .. } => Self::Magnify,
            Gesture::Rotate { .. } => Self::Rotate,
        }
    }
}

/// A discrete Dock swipe acts where the Dock would commit: past half a transition, or flung
/// the same way after some travel. The fling numbers are guesses (progress per second is
/// assumed), pending real trackpad recordings.
const DISCRETE_COMMIT_PROGRESS: f64 = 0.5;
const DISCRETE_FLING_PROGRESS: f64 = 0.1;
const DISCRETE_FLING_VELOCITY: f64 = 2.0;

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
    /// The gesture in progress (one at a time, like a trackpad).
    gesture: Option<HeldGesture>,
    /// How this Mac's Dock takes swipes (see [`set_dock_modes`](Self::set_dock_modes)).
    dock: DockModes,
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
            gesture: None,
            dock: DockModes::default(),
        }
    }

    pub fn modifiers(&self) -> u64 {
        self.mods
    }

    /// Starts over from `pos` (the pointer is now there, e.g. on another display): the next move
    /// is measured from it.
    pub fn rebase(&mut self, pos: Point) {
        self.pos = pos;
        self.delta_rem = (0.0, 0.0);
    }

    pub fn position(&self) -> Point {
        self.pos
    }

    /// Whether any key, modifier or button is held, or a gesture is in progress (Caps Lock is a
    /// state, not a held key). A viewer that goes silent while this holds gets everything
    /// released: that is also what cancels a Dock swipe left hanging.
    pub fn holds_anything(&self) -> bool {
        !self.keys.is_empty() || self.buttons != 0 || self.mods & !CAPS_LOCK != 0 || self.gesture.is_some()
    }

    /// How Dock swipes are handled here: which axes are recognised rather than replayed, and the
    /// direction factors behind the Space swipes [`system`](Self::system) makes. Defaults to
    /// `DockModes::default()`, the same on any Mac (for recordings); a host posting real events
    /// sets `gesture::dock_modes()`.
    pub fn set_dock_modes(&mut self, modes: DockModes) {
        self.dock = modes;
    }

    pub fn dock_modes(&self) -> DockModes {
        self.dock
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

    /// One phase of a gesture. Began ends any gesture in progress first (as cancelled) and, for
    /// a pinch or rotation, moves to `at` first: AppKit sends the whole gesture to the view under
    /// the pointer where it began, so its later events go there too, wherever `at` says. Changed,
    /// Ended and Cancelled apply only to the gesture in progress (same kind, same Dock axis) and
    /// are dropped otherwise, like the release of a key that isn't held.
    ///
    /// On an axis whose Dock swipes are discrete (see [`set_dock_modes`](Self::set_dock_modes)),
    /// the swipe posts nothing while it lasts; when it ends past the Dock's commit point it
    /// becomes the matching [`SystemAction`].
    pub fn gesture(&mut self, phase: GesturePhase, gesture: Gesture, at: Option<Point>, out: &mut Vec<Synth>) {
        let kind = HeldKind::of(&gesture);
        if phase == GesturePhase::Began {
            self.cancel_gesture(out);
            let at = match (kind, at) {
                (HeldKind::Magnify | HeldKind::Rotate, Some(at)) => {
                    if at != self.pos {
                        self.move_to(at, out);
                    }
                    at
                }
                _ => self.pos,
            };
            let discrete = matches!(kind, HeldKind::Dock(axis) if self.dock.recipe(axis) == DockRecipe::Discrete);
            self.gesture = Some(HeldGesture { kind, at, progress: 0.0, inverted: false, discrete });
        }
        let Some(held) = self.gesture.as_mut().filter(|h| h.kind == kind) else { return };
        if let Gesture::DockSwipe { progress, inverted, .. } = gesture {
            (held.progress, held.inverted) = (progress, inverted);
        }
        let held = *held;
        if phase.ends() {
            self.gesture = None;
        }
        if held.discrete {
            if phase == GesturePhase::Ended
                && let Some(action) = self.recognise(gesture)
            {
                out.push(Synth::System(action));
            }
            return;
        }
        out.push(self.gesture_event(phase, gesture, held.at));
    }

    /// Ends the gesture in progress, if any, as cancelled: a Dock swipe at its latest progress
    /// with no velocity, which makes the Dock snap back. Nothing to end for a discrete swipe.
    pub fn cancel_gesture(&mut self, out: &mut Vec<Synth>) {
        let Some(held) = self.gesture.take() else { return };
        if held.discrete {
            return;
        }
        let cancel = match held.kind {
            HeldKind::Dock(axis) => Gesture::DockSwipe { axis, progress: held.progress, velocity_x: 0.0, velocity_y: 0.0, inverted: held.inverted },
            HeldKind::Magnify => Gesture::Magnify { delta: 0.0 },
            HeldKind::Rotate => Gesture::Rotate { degrees: 0.0 },
        };
        out.push(self.gesture_event(GesturePhase::Cancelled, cancel, held.at));
    }

    /// Cancels a pinch or rotation in progress but not a Dock swipe (e.g. when the display it
    /// was on went away; a Dock swipe isn't tied to a display).
    pub fn cancel_positioned_gesture(&mut self, out: &mut Vec<Synth>) {
        if self.gesture.is_some_and(|h| !matches!(h.kind, HeldKind::Dock(_))) {
            self.cancel_gesture(out);
        }
    }

    /// A two-finger double tap at `at` (moving there first if needed). Not held.
    pub fn smart_magnify(&mut self, at: Point, out: &mut Vec<Synth>) {
        if at != self.pos {
            self.move_to(at, out);
        }
        out.push(Synth::SmartMagnify { at, flags: self.mods });
    }

    /// A swipe between pages at `at` (moving there first if needed): -1, 0 or 1 per axis. Not
    /// held; one without a direction is dropped.
    pub fn navigation_swipe(&mut self, at: Point, dx: i8, dy: i8, out: &mut Vec<Synth>) {
        let (dx, dy) = (dx.signum(), dy.signum());
        if dx == 0 && dy == 0 {
            return;
        }
        if at != self.pos {
            self.move_to(at, out);
        }
        out.push(Synth::NavigationSwipe { at, dx, dy, flags: self.mods });
    }

    /// Does `action` on the Mac, after ending any gesture in progress. Previous and Next Space
    /// become a one-Space Dock swipe (Space Rabbit's: began at ±ε, changed to ±1, ended at ±1
    /// with a fling), so they are posted, recorded and tested like any swipe. The rest, and
    /// Spaces when horizontal swipes are discrete, are [`Synth::System`].
    pub fn system(&mut self, action: SystemAction, out: &mut Vec<Synth>) {
        self.cancel_gesture(out);
        let right = match action {
            SystemAction::NextSpace => 1.0,
            SystemAction::PreviousSpace => -1.0,
            _ => {
                out.push(Synth::System(action));
                return;
            }
        };
        if self.dock.recipe(DockAxis::Horizontal) == DockRecipe::Discrete {
            out.push(Synth::System(action));
            return;
        }
        let sign = right * self.dock.direction(DockAxis::Horizontal);
        let swipe = |phase, progress: f64, velocity: f64| Synth::DockSwipe {
            axis: DockAxis::Horizontal,
            phase,
            progress: sign * progress,
            velocity_x: sign * velocity,
            velocity_y: 0.0,
            inverted: false,
        };
        out.push(swipe(GesturePhase::Began, PROGRESS_EPSILON, 0.0));
        out.push(swipe(GesturePhase::Changed, 1.0, 0.0));
        out.push(swipe(GesturePhase::Ended, 1.0, self.dock.hop_velocity));
    }

    fn gesture_event(&self, phase: GesturePhase, gesture: Gesture, at: Point) -> Synth {
        match gesture {
            Gesture::DockSwipe { axis, progress, velocity_x, velocity_y, inverted } => {
                Synth::DockSwipe { axis, phase, progress, velocity_x, velocity_y, inverted }
            }
            Gesture::Magnify { delta } => Synth::AppGesture { at, phase, kind: AppGestureKind::Magnify, value: delta, flags: self.mods },
            Gesture::Rotate { degrees } => Synth::AppGesture { at, phase, kind: AppGestureKind::Rotate, value: degrees, flags: self.mods },
        }
    }

    /// The action a discrete Dock swipe that just ended asks for, if it went far or fast enough.
    /// Directions are the wire's (this Mac's values times its direction factor, which is its own
    /// inverse): + is right, up (Mission Control) or apart. Unverified: that the "inverted" flag
    /// flips the direction, and the fling threshold.
    fn recognise(&self, gesture: Gesture) -> Option<SystemAction> {
        let Gesture::DockSwipe { axis, progress, velocity_x, velocity_y, inverted } = gesture else { return None };
        let sign = self.dock.direction(axis) * if inverted { -1.0 } else { 1.0 };
        let progress = progress * sign;
        let velocity = sign
            * match axis {
                DockAxis::Horizontal => velocity_x,
                DockAxis::Vertical => velocity_y,
                // Which one a pinch fills is unknown: take the larger.
                DockAxis::Pinch if velocity_x.abs() >= velocity_y.abs() => velocity_x,
                DockAxis::Pinch => velocity_y,
            };
        let flung = progress.abs() >= DISCRETE_FLING_PROGRESS && velocity * progress > 0.0 && velocity.abs() >= DISCRETE_FLING_VELOCITY;
        if progress.abs() < DISCRETE_COMMIT_PROGRESS && !flung {
            return None;
        }
        Some(match (axis, progress > 0.0) {
            (DockAxis::Horizontal, true) => SystemAction::NextSpace,
            (DockAxis::Horizontal, false) => SystemAction::PreviousSpace,
            (DockAxis::Vertical, true) => SystemAction::MissionControl,
            (DockAxis::Vertical, false) => SystemAction::AppExpose,
            (DockAxis::Pinch, true) => SystemAction::ShowDesktop,
            (DockAxis::Pinch, false) => SystemAction::Launchpad,
        })
    }

    /// Ends any gesture in progress (as cancelled), then releases every key, button and modifier,
    /// and puts Caps Lock back to the host's own state. The gesture ends first, under the flags
    /// it began with. Buttons go up while the modifiers are still down, so a drag in progress
    /// (⌥-drag to copy) drops the way it was meant to.
    pub fn release_all(&mut self, host_caps_lock: bool, out: &mut Vec<Synth>) {
        self.cancel_gesture(out);
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
    /// How long after the end of a Dock swipe it goes out once more (see [`TERMINAL_RESEND`]).
    resend_delay: Option<Duration>,
    dock_unavailable_warned: bool,
}

/// Mac Mouse Fix's fix for a Dock that misses the end of a swipe and stays stuck mid-transition:
/// the end goes out once more a little later (`gesture::terminal_resend_delay`), unless another
/// swipe began meanwhile.
#[derive(Debug, Default)]
struct TerminalResend {
    pending: Option<(Instant, Synth, u8)>,
}

/// The end of the last Dock swipe posted in this process, due to go out once more. One for all
/// posters, as there is one Dock: a swipe begun since through any of them (another session's,
/// after a take-over) drops it, so a stale end never cuts into that swipe. A swipe the host's
/// own user begins on the trackpad can't (posters don't see local input), so one begun within
/// the delay may still get the old end.
static TERMINAL_RESEND: Mutex<TerminalResend> = Mutex::new(TerminalResend { pending: None });

fn terminal_resend() -> MutexGuard<'static, TerminalResend> {
    TERMINAL_RESEND.lock().unwrap_or_else(|e| e.into_inner())
}

impl TerminalResend {
    /// Notes a posted Dock swipe event.
    fn note(&mut self, synth: &Synth, depth: u8, now: Instant, delay: Option<Duration>) {
        let Synth::DockSwipe { phase, .. } = *synth else { return };
        if phase == GesturePhase::Began {
            self.pending = None;
        } else if phase.ends() {
            self.pending = delay.map(|delay| (now + delay, synth.clone(), depth));
        }
    }

    /// The end to post again, once it is due.
    fn take_due(&mut self, now: Instant) -> Option<(Synth, u8)> {
        self.pending.take_if(|(at, ..)| now >= *at).map(|(_, synth, depth)| (synth, depth))
    }

    /// When the pending end is due, if there is one.
    fn due(&self) -> Option<Instant> {
        self.pending.as_ref().map(|(at, ..)| *at)
    }
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
        Ok(Self {
            source,
            tag,
            pid: None,
            resend_delay: gesture::terminal_resend_delay(),
            dock_unavailable_warned: false,
        })
    }

    /// Like [`new`](Self::new), but delivers every event to process `pid` only, wherever the
    /// cursor and focus are (for tests: control one app without touching the rest of the Mac).
    /// Dock swipes and system actions are skipped: they act on the whole Mac.
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
    pub fn post(&mut self, synth: &Synth, depth: u8) {
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
            Synth::DockSwipe { .. } => {
                if self.post_dock_swipe(synth, depth) {
                    terminal_resend().note(synth, depth, Instant::now(), self.resend_delay);
                }
            }
            // Gestures go where mouse events go, so they can't overtake the move before them.
            Synth::AppGesture { at, phase, kind, value, flags } => {
                let e = match kind {
                    AppGestureKind::Magnify => gesture::magnify_event(src, at, phase, value, flags),
                    AppGestureKind::Rotate => gesture::rotate_event(src, at, phase, value, flags),
                };
                let Some(e) = e else { return };
                self.send(CGEventTapLocation::HIDEventTap, &e, depth);
            }
            Synth::SmartMagnify { at, flags } => {
                let Some(e) = gesture::smart_magnify_event(src, at, flags) else { return };
                self.send(CGEventTapLocation::HIDEventTap, &e, depth);
            }
            Synth::NavigationSwipe { at, dx, dy, flags } => {
                let Some(events) = gesture::navigation_swipe_events(src, at, dx, dy, flags) else { return };
                for e in &events {
                    self.send(CGEventTapLocation::HIDEventTap, e, depth);
                }
            }
            // They act on the whole Mac, which pid mode promises not to touch.
            Synth::System(action) => {
                if self.pid.is_none() {
                    gesture::perform(action);
                }
            }
        }
    }

    /// Posts what has come due: the end of the last Dock swipe, once more (whichever poster polls
    /// first). Call it regularly (the input thread's 100 ms tick); nothing happens on its own, so
    /// recordings stay deterministic.
    pub fn poll(&mut self, now: Instant) {
        // One that posts no swipes (pid mode) would lose it.
        if self.pid.is_some() {
            return;
        }
        let due = terminal_resend().take_due(now);
        if let Some((synth, depth)) = due {
            self.post_dock_swipe(&synth, depth);
        }
    }

    /// Like [`poll`](Self::poll), but waits until the pending end is due (at most the delay; not
    /// at all if none is): for when nothing will poll any more, as the input thread or the app is
    /// ending.
    pub fn post_pending(&mut self) {
        let Some(due) = terminal_resend().due() else { return };
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        self.poll(Instant::now());
    }

    /// Posts a [`Synth::DockSwipe`] with this Mac's recipe for its axis, at the session tap where
    /// the Dock reads it. Whether it was posted.
    fn post_dock_swipe(&mut self, synth: &Synth, depth: u8) -> bool {
        let Synth::DockSwipe { axis, phase, progress, velocity_x, velocity_y, inverted } = *synth else { return false };
        // Swipes act on the whole Mac, which pid mode promises not to touch.
        if self.pid.is_some() {
            return false;
        }
        let recipe = gesture::dock_recipe(axis);
        let sample = DockSample { axis, phase, progress, velocity_x, velocity_y, inverted };
        // Made without a source (as in every recipe known to work), so tagged here, every time.
        let Some(events) = gesture::dock_swipe_events(recipe, gesture::os_major(), &sample, tag_with_depth(self.tag, depth)) else {
            if !self.dock_unavailable_warned {
                tracing::warn!(?axis, ?recipe, "dock swipe not posted: no working recipe for it on this mac");
                self.dock_unavailable_warned = true;
            }
            return false;
        };
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&events.dock));
        if let Some(companion) = &events.companion {
            CGEvent::post(CGEventTapLocation::SessionEventTap, Some(companion));
        }
        true
    }
}

/// A safety net for tests that inject real input on a Mac someone uses: lets events through only
/// where they would reach one process. Pointer events must land on a window of that process that
/// is front-most at that point; key presses need that process's window in front. The release of
/// a delivered press always passes (nothing may stay held); that of a dropped press is dropped too.
/// Gestures follow the same rules (the rest of a pinch goes where its start went); Dock swipes
/// and system actions never pass, since they change the whole Mac.
pub struct InjectGuard {
    pid: i32,
    windows: Vec<WindowInfo>,
    refreshed: Option<std::time::Instant>,
    /// Keys whose press got through, so their release must too.
    delivered_keys: BTreeSet<u16>,
    dropped_buttons: u32,
    /// Whether the pinch or rotation in progress began over the target.
    delivered_gesture: bool,
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
        Self {
            pid,
            windows: Vec::new(),
            refreshed: None,
            delivered_keys: BTreeSet::new(),
            dropped_buttons: 0,
            delivered_gesture: false,
            dropped: 0,
        }
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
            Synth::DockSwipe { .. } | Synth::System(_) => false,
            Synth::AppGesture { at, phase: GesturePhase::Began, .. } => {
                self.delivered_gesture = self.owner_at(at) == Some(self.pid);
                self.delivered_gesture
            }
            // No new check of the position: the gesture stays with its window, and one that
            // began there always ends.
            Synth::AppGesture { phase, .. } => {
                let delivered = self.delivered_gesture;
                if phase.ends() {
                    self.delivered_gesture = false;
                }
                delivered
            }
            Synth::SmartMagnify { at, .. } | Synth::NavigationSwipe { at, .. } => self.owner_at(at) == Some(self.pid),
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

    fn dock(axis: DockAxis, progress: f64) -> Gesture {
        Gesture::DockSwipe { axis, progress, velocity_x: 0.0, velocity_y: 0.0, inverted: false }
    }

    fn dock_end(axis: DockAxis, progress: f64, velocity_x: f64, velocity_y: f64) -> Gesture {
        Gesture::DockSwipe { axis, progress, velocity_x, velocity_y, inverted: false }
    }

    /// The phases of the gestures in `out`, with their progress or value.
    fn gesture_events(out: &[Synth]) -> Vec<(GesturePhase, f64)> {
        out.iter()
            .filter_map(|e| match *e {
                Synth::DockSwipe { phase, progress, .. } => Some((phase, progress)),
                Synth::AppGesture { phase, value, .. } => Some((phase, value)),
                _ => None,
            })
            .collect()
    }

    fn with_discrete(axis: DockAxis) -> InputState {
        let mut s = state();
        let defaults = DockModes::default();
        let recipes = [DockAxis::Horizontal, DockAxis::Vertical, DockAxis::Pinch].map(|a| if a == axis { DockRecipe::Discrete } else { defaults.recipe(a) });
        s.set_dock_modes(DockModes { recipes, ..defaults });
        s
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
        s.gesture(GesturePhase::Began, dock(DockAxis::Vertical, 0.0), None, &mut out);
        s.gesture(GesturePhase::Changed, dock(DockAxis::Vertical, 0.3), None, &mut out);
        assert!(s.holds_anything());
        out.clear();
        s.release_all(false, &mut out);
        assert!(!s.holds_anything());
        let ups: Vec<_> = out
            .iter()
            .map(|e| match e {
                Synth::Key { code, down: false, .. } => format!("key{code}"),
                Synth::Mouse { kind: MouseKind::Up, button, .. } => format!("button{button}"),
                Synth::DockSwipe { phase: GesturePhase::Cancelled, .. } => "dock".to_string(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            ups,
            ["dock", "key0", "key1", "button0", "button2", "key63", "key55"],
            "the gesture first, then buttons before modifiers, which go in reverse press order"
        );
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

    #[test]
    fn a_dock_swipe_goes_out_phase_by_phase_while_it_is_held() {
        use GesturePhase::*;
        let mut s = state();
        let mut out = Vec::new();
        s.gesture(Began, dock(DockAxis::Vertical, 0.0), None, &mut out);
        assert!(s.holds_anything(), "a swipe in progress is held, so silence cancels it");
        s.gesture(Changed, dock(DockAxis::Vertical, 0.4), None, &mut out);
        s.gesture(Ended, dock_end(DockAxis::Vertical, 0.9, 0.0, 3.5), None, &mut out);
        assert!(!s.holds_anything());
        assert_eq!(gesture_events(&out), [(Began, 0.0), (Changed, 0.4), (Ended, 0.9)]);
        assert_eq!(
            out[2],
            Synth::DockSwipe { axis: DockAxis::Vertical, phase: Ended, progress: 0.9, velocity_x: 0.0, velocity_y: 3.5, inverted: false }
        );
        assert_eq!(s.position(), p(100.0, 100.0), "a Dock swipe doesn't move the pointer");
    }

    #[test]
    fn gesture_updates_without_their_start_are_dropped() {
        use GesturePhase::*;
        let mut s = state();
        let mut out = Vec::new();
        s.gesture(Changed, dock(DockAxis::Horizontal, 0.2), None, &mut out);
        s.gesture(Ended, dock(DockAxis::Horizontal, 0.2), None, &mut out);
        s.gesture(Cancelled, Gesture::Magnify { delta: 0.0 }, Some(p(1.0, 1.0)), &mut out);
        assert!(out.is_empty(), "{out:#?}");
        s.gesture(Began, dock(DockAxis::Horizontal, 0.0), None, &mut out);
        s.gesture(Changed, dock(DockAxis::Vertical, 0.5), None, &mut out);
        s.gesture(Changed, Gesture::Rotate { degrees: 3.0 }, Some(p(1.0, 1.0)), &mut out);
        assert_eq!(gesture_events(&out), [(Began, 0.0)], "another axis or kind is not the swipe in progress");
        s.gesture(Ended, dock(DockAxis::Horizontal, 0.7), None, &mut out);
        s.gesture(Ended, dock(DockAxis::Horizontal, 0.7), None, &mut out);
        assert_eq!(gesture_events(&out), [(Began, 0.0), (Ended, 0.7)], "a second end is dropped");
    }

    #[test]
    fn a_new_gesture_cancels_the_one_in_progress() {
        use GesturePhase::*;
        let mut s = state();
        let mut out = Vec::new();
        s.gesture(Began, dock(DockAxis::Vertical, 0.0), None, &mut out);
        s.gesture(Changed, Gesture::DockSwipe { axis: DockAxis::Vertical, progress: -0.35, velocity_x: 1.0, velocity_y: 2.0, inverted: true }, None, &mut out);
        out.clear();
        s.gesture(Began, Gesture::Magnify { delta: 0.0 }, Some(p(300.0, 200.0)), &mut out);
        assert_eq!(
            out[0],
            Synth::DockSwipe { axis: DockAxis::Vertical, phase: Cancelled, progress: -0.35, velocity_x: 0.0, velocity_y: 0.0, inverted: true },
            "cancelled where it was, without a fling"
        );
        assert!(matches!(out[1], Synth::Mouse { kind: MouseKind::Moved, at, .. } if at == p(300.0, 200.0)), "{out:#?}");
        assert!(matches!(out[2], Synth::AppGesture { phase: Began, kind: AppGestureKind::Magnify, .. }));
        // A Dock swipe on another axis also replaces one.
        out.clear();
        s.gesture(Began, dock(DockAxis::Horizontal, 0.0), None, &mut out);
        assert_eq!(gesture_events(&out), [(Cancelled, 0.0), (Began, 0.0)]);
    }

    #[test]
    fn a_pinch_stays_where_it_began_and_carries_the_modifiers() {
        use GesturePhase::*;
        let mut s = state();
        let mut out = Vec::new();
        s.set_modifiers(COMMAND | DEVICE_LCMD, &mut out);
        out.clear();
        s.gesture(Began, Gesture::Rotate { degrees: 0.0 }, Some(p(50.0, 60.0)), &mut out);
        s.gesture(Changed, Gesture::Rotate { degrees: 4.5 }, Some(p(51.0, 60.0)), &mut out);
        s.gesture(Ended, Gesture::Rotate { degrees: 0.0 }, Some(p(52.0, 61.0)), &mut out);
        assert_eq!(out.len(), 4, "one move, then the three phases: {out:#?}");
        for e in &out[1..] {
            let Synth::AppGesture { at, kind, flags, .. } = *e else { panic!("{e:?}") };
            assert_eq!((at, kind, flags), (p(50.0, 60.0), AppGestureKind::Rotate, COMMAND | DEVICE_LCMD));
        }
        assert_eq!(gesture_events(&out), [(Began, 0.0), (Changed, 4.5), (Ended, 0.0)]);
    }

    #[test]
    fn cancelling_keeps_the_last_progress_and_needs_a_gesture() {
        use GesturePhase::*;
        let mut s = state();
        let mut out = Vec::new();
        s.cancel_gesture(&mut out);
        assert!(out.is_empty());
        s.gesture(Began, dock(DockAxis::Pinch, 0.0), None, &mut out);
        s.gesture(Changed, dock(DockAxis::Pinch, 0.62), None, &mut out);
        s.cancel_gesture(&mut out);
        assert_eq!(gesture_events(&out), [(Began, 0.0), (Changed, 0.62), (Cancelled, 0.62)]);
        assert!(!s.holds_anything());
        s.gesture(Changed, dock(DockAxis::Pinch, 0.7), None, &mut out);
        assert_eq!(out.len(), 3, "a late update after the cancel is dropped");
        // A pinch cancels with no further zoom.
        s.gesture(Began, Gesture::Magnify { delta: 0.0 }, Some(p(100.0, 100.0)), &mut out);
        s.gesture(Changed, Gesture::Magnify { delta: 0.1 }, Some(p(100.0, 100.0)), &mut out);
        s.cancel_gesture(&mut out);
        assert!(matches!(out.last(), Some(Synth::AppGesture { phase: Cancelled, value: 0.0, .. })));
    }

    #[test]
    fn a_lost_display_cancels_a_pinch_but_not_a_dock_swipe() {
        use GesturePhase::*;
        let mut s = state();
        let mut out = Vec::new();
        s.gesture(Began, dock(DockAxis::Horizontal, 0.0), None, &mut out);
        s.cancel_positioned_gesture(&mut out);
        assert!(s.holds_anything());
        s.gesture(Began, Gesture::Magnify { delta: 0.0 }, Some(p(100.0, 100.0)), &mut out);
        out.clear();
        s.cancel_positioned_gesture(&mut out);
        assert!(matches!(out[..], [Synth::AppGesture { phase: Cancelled, .. }]), "{out:#?}");
        assert!(!s.holds_anything());
    }

    #[test]
    fn taps_are_not_held() {
        let mut s = state();
        let mut out = Vec::new();
        s.smart_magnify(p(10.0, 20.0), &mut out);
        s.navigation_swipe(p(10.0, 20.0), -3, 0, &mut out);
        s.navigation_swipe(p(10.0, 20.0), 0, 0, &mut out);
        assert!(!s.holds_anything());
        assert!(matches!(out[0], Synth::Mouse { kind: MouseKind::Moved, .. }));
        assert_eq!(out[1], Synth::SmartMagnify { at: p(10.0, 20.0), flags: 0 });
        assert_eq!(out[2], Synth::NavigationSwipe { at: p(10.0, 20.0), dx: -1, dy: 0, flags: 0 });
        assert_eq!(out.len(), 3, "a swipe without a direction is dropped");
    }

    #[test]
    fn spaces_are_one_space_dock_swipes() {
        use GesturePhase::*;
        let mut s = state();
        let mut out = Vec::new();
        s.system(SystemAction::NextSpace, &mut out);
        assert_eq!(gesture_events(&out), [(Began, PROGRESS_EPSILON), (Changed, 1.0), (Ended, 1.0)]);
        assert!(matches!(out[2], Synth::DockSwipe { axis: DockAxis::Horizontal, velocity_x: 8.0, velocity_y: 0.0, .. }));
        assert!(!s.holds_anything(), "the swipe is complete");
        out.clear();
        s.system(SystemAction::PreviousSpace, &mut out);
        assert_eq!(gesture_events(&out), [(Began, -PROGRESS_EPSILON), (Changed, -1.0), (Ended, -1.0)]);
        // Where this Mac's Dock reads horizontal swipes the other way round (macOS 27 with Natural
        // scrolling), and flings harder.
        s.set_dock_modes(DockModes { directions: [-1.0, 1.0, 1.0], hop_velocity: 9999.0, ..DockModes::default() });
        out.clear();
        s.system(SystemAction::NextSpace, &mut out);
        assert!(matches!(out[2], Synth::DockSwipe { progress: -1.0, velocity_x: -9999.0, .. }), "{out:#?}");
        // The others are done directly, after ending a gesture in progress.
        out.clear();
        s.gesture(Began, dock(DockAxis::Vertical, 0.0), None, &mut out);
        s.system(SystemAction::MissionControl, &mut out);
        assert!(matches!(out[1..], [Synth::DockSwipe { phase: Cancelled, .. }, Synth::System(SystemAction::MissionControl)]), "{out:#?}");
    }

    #[test]
    fn discrete_dock_swipes_become_actions_when_they_end() {
        use GesturePhase::*;
        let swipe = |s: &mut InputState, axis, end: Gesture, out: &mut Vec<Synth>| {
            s.gesture(Began, dock(axis, 0.0), None, out);
            s.gesture(Changed, dock(axis, 0.2), None, out);
            assert!(s.holds_anything(), "held (heartbeats keep it) but not posted");
            s.gesture(Ended, end, None, out);
        };
        let cases = [
            (DockAxis::Vertical, dock(DockAxis::Vertical, 0.6), Some(SystemAction::MissionControl)),
            (DockAxis::Vertical, dock(DockAxis::Vertical, -0.6), Some(SystemAction::AppExpose)),
            (DockAxis::Vertical, dock(DockAxis::Vertical, 0.3), None),
            (DockAxis::Vertical, dock_end(DockAxis::Vertical, 0.3, 0.0, 5.0), Some(SystemAction::MissionControl)),
            (DockAxis::Vertical, dock_end(DockAxis::Vertical, 0.3, 5.0, 0.0), None),
            (DockAxis::Vertical, dock_end(DockAxis::Vertical, 0.3, 0.0, -5.0), None),
            (DockAxis::Vertical, dock_end(DockAxis::Vertical, 0.05, 0.0, 5.0), None),
            (DockAxis::Horizontal, dock(DockAxis::Horizontal, 1.2), Some(SystemAction::NextSpace)),
            (DockAxis::Horizontal, dock_end(DockAxis::Horizontal, -0.2, -3.0, 0.0), Some(SystemAction::PreviousSpace)),
            (DockAxis::Pinch, dock(DockAxis::Pinch, 0.8), Some(SystemAction::ShowDesktop)),
            (DockAxis::Pinch, dock_end(DockAxis::Pinch, -0.4, 0.0, -3.0), Some(SystemAction::Launchpad)),
            (
                DockAxis::Vertical,
                Gesture::DockSwipe { axis: DockAxis::Vertical, progress: 0.6, velocity_x: 0.0, velocity_y: 0.0, inverted: true },
                Some(SystemAction::AppExpose),
            ),
        ];
        for (axis, end, want) in cases {
            let mut s = with_discrete(axis);
            let mut out = Vec::new();
            swipe(&mut s, axis, end, &mut out);
            let got: Vec<_> = out.iter().map(|e| if let Synth::System(a) = e { *a } else { panic!("posted {e:?}") }).collect();
            assert_eq!(got, want.into_iter().collect::<Vec<_>>(), "{end:?}");
            assert!(!s.holds_anything());
        }
        // Read in the wire's direction: where this Mac's factor is -1, -0.6 is up.
        let mut s = with_discrete(DockAxis::Vertical);
        s.set_dock_modes(DockModes { directions: [1.0, -1.0, 1.0], ..s.dock_modes() });
        let mut out = Vec::new();
        swipe(&mut s, DockAxis::Vertical, dock(DockAxis::Vertical, -0.6), &mut out);
        assert_eq!(out, [Synth::System(SystemAction::MissionControl)]);
        // Cancelled or released, a discrete swipe does nothing; other axes still replay.
        let mut s = with_discrete(DockAxis::Vertical);
        out.clear();
        s.gesture(Began, dock(DockAxis::Vertical, 0.0), None, &mut out);
        s.gesture(Changed, dock(DockAxis::Vertical, 0.9), None, &mut out);
        s.release_all(false, &mut out);
        s.gesture(Began, dock(DockAxis::Vertical, 0.0), None, &mut out);
        s.gesture(Cancelled, dock(DockAxis::Vertical, 0.9), None, &mut out);
        assert!(out.is_empty(), "{out:#?}");
        s.gesture(Began, dock(DockAxis::Horizontal, 0.0), None, &mut out);
        assert_eq!(gesture_events(&out), [(Began, 0.0)]);
        // Spaces too, when horizontal swipes are discrete.
        let mut s = with_discrete(DockAxis::Horizontal);
        out.clear();
        s.system(SystemAction::NextSpace, &mut out);
        assert_eq!(out, [Synth::System(SystemAction::NextSpace)]);
    }

    #[test]
    fn system_actions_map_from_the_wire() {
        assert_eq!(SystemAction::from_wire(protocol::SystemAction::MISSION_CONTROL), Some(SystemAction::MissionControl));
        assert_eq!(SystemAction::from_wire(protocol::SystemAction::NEXT_SPACE), Some(SystemAction::NextSpace));
        assert_eq!(SystemAction::from_wire(protocol::SystemAction(0)), None);
        assert_eq!(SystemAction::from_wire(protocol::SystemAction(7)), None);
        for code in 1..=6 {
            assert!(SystemAction::from_wire(protocol::SystemAction(code)).is_some(), "{code}");
        }
    }

    #[test]
    fn guard_keeps_gestures_with_their_window_and_drops_whole_mac_ones() {
        use GesturePhase::*;
        let target = 4242;
        let mut g = InjectGuard::new(target);
        let win = |pid, x, w| WindowInfo { pid, layer: 0, bounds: Bounds { x, y: 0.0, width: w, height: 500.0 } };
        g.set_windows(vec![win(target, 0.0, 500.0), win(2, 0.0, 2000.0)]);
        let pinch = |phase, x| Synth::AppGesture { at: p(x, 10.0), phase, kind: AppGestureKind::Magnify, value: 0.1, flags: 0 };
        assert!(g.judge(&pinch(Began, 10.0)));
        assert!(g.judge(&pinch(Changed, 900.0)), "the rest of a delivered pinch follows it");
        assert!(g.judge(&pinch(Ended, 900.0)));
        assert!(!g.judge(&pinch(Changed, 10.0)), "nothing in progress");
        assert!(!g.judge(&pinch(Began, 900.0)));
        assert!(!g.judge(&pinch(Changed, 10.0)), "the rest of a dropped pinch is dropped");
        assert!(!g.judge(&pinch(Cancelled, 10.0)));
        assert!(g.judge(&Synth::SmartMagnify { at: p(10.0, 10.0), flags: 0 }));
        assert!(!g.judge(&Synth::NavigationSwipe { at: p(900.0, 10.0), dx: 1, dy: 0, flags: 0 }));
        let dock = Synth::DockSwipe { axis: DockAxis::Vertical, phase: Began, progress: 0.0, velocity_x: 0.0, velocity_y: 0.0, inverted: false };
        assert!(!g.judge(&dock), "Dock swipes change the whole Mac");
        assert!(!g.judge(&Synth::System(SystemAction::ShowDesktop)));
    }

    #[test]
    fn the_end_of_a_dock_swipe_is_posted_again_once_unless_another_began() {
        use GesturePhase::*;
        let swipe = |phase| Synth::DockSwipe { axis: DockAxis::Horizontal, phase, progress: 1.0, velocity_x: 8.0, velocity_y: 0.0, inverted: false };
        let delay = Some(Duration::from_millis(200));
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut r = TerminalResend::default();
        r.note(&swipe(Began), 0, t0, delay);
        r.note(&swipe(Ended), 2, at(10), delay);
        assert_eq!(r.take_due(at(100)), None);
        assert_eq!(r.due(), Some(at(210)), "what an ending thread waits for");
        assert_eq!(r.take_due(at(210)), Some((swipe(Ended), 2)), "with its relay depth");
        assert_eq!((r.take_due(at(500)), r.due()), (None, None), "once");
        r.note(&swipe(Cancelled), 0, at(600), delay);
        r.note(&swipe(Began), 0, at(700), delay);
        assert_eq!(r.take_due(at(900)), None, "a new swipe began");
        r.note(&swipe(Changed), 0, at(950), delay);
        r.note(&Synth::System(SystemAction::MissionControl), 0, at(950), delay);
        assert_eq!(r.take_due(at(2000)), None);
        r.note(&swipe(Ended), 0, at(3000), None);
        assert_eq!(r.take_due(at(9000)), None, "not on this macOS");
    }
}
