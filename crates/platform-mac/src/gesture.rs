//! Trackpad gestures: the direction conventions of this Mac's Dock gestures, the Quartz events
//! that replay a viewer's gestures here, and the Dock's own actions.
//!
//! There is no public API for any of it. Gestures are Quartz events of private types and fields,
//! named in WebKit's `CoreGraphicsTestSPI.h` and posted by Mac Mouse Fix, Space Rabbit,
//! FasterSwiper, InstantSpaceSwitcher, yabai and Hammerspoon. Everything built here was checked
//! in memory (never posted) against AppKit's and Apple's own decoders on macOS 26.5:
//! - Pinch, rotation, smart zoom and page swipes are gesture events (type 29) that AppKit turns
//!   into `NSEvent`s for the view under the pointer. They are posted at the HID tap, like mouse
//!   events, so they can't overtake the move before them.
//! - Swipes the Dock acts on (Spaces, Mission Control, App Exposé, Show Desktop, Launchpad) are
//!   Dock control events (type 30), posted at the session tap. What the Dock reads from them
//!   depends on the macOS version (see [`DockRecipe`]): before 27 the plain fields (though 26
//!   rejects bare vertical ones), from 27 on only an IOHID event serialized into the event.
//! - Mission Control, App Exposé, Show Desktop and Launchpad as actions go through the Dock's
//!   own notifications (`CoreDockSendNotification`, as yabai sends them), or open the app that
//!   does it.
//!
//! Nothing here writes through pointer offsets into Apple's structures. A recipe that fails its
//! in-memory self-test is not used: swipes on that axis become discrete actions instead.

use std::ffi::{CStr, c_void};
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFData, CFRetained, CFString, CFType, CGPoint};
use objc2_core_graphics::{CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventType};
use protocol::{DockAxis, GesturePhase};

use crate::inject::{Point, SystemAction};

// Quartz event types (WebKit CoreGraphicsTestSPI.h).

/// `kCGSEventGesture`: pinch, rotation, smart zoom and page swipes, and the bare "companion"
/// some Dock swipe recipes post after each Dock event.
const EVENT_GESTURE: u32 = 29;
/// `kCGSEventDockControl`: a swipe the Dock acts on.
const EVENT_DOCK_CONTROL: u32 = 30;

// Gesture event fields (WebKit CoreGraphicsTestSPI.h names). Several share storage, and what a
// setter writes depends on the subtype (field 110), so the subtype is set first and only the
// fields of that subtype are written (experiments k/alias, k/order). On a Dock swipe, 165 is
// 123, 119 and 139 are 123's float bits, and 135 is 124's float bits: writing 135 after 124
// overwrites the progress. So 119, 135, 139 and 165 are never written here, although Mac Mouse
// Fix writes them. Fields 41, 42 and 43 are the usual source pid, source user data (the LanKVM
// tag, which `CGEventCreateFromData` drops) and source uid.

/// `kCGSEventTypeField`: the event type, the same storage as `CGEventGetType`.
const FIELD_EVENT_TYPE: u32 = 55;
/// `kCGEventGestureHIDType`: the IOHID event the gesture stands for (`HID_*`).
const FIELD_SUBTYPE: u32 = 110;
/// `kCGEventGestureZoomValue`: `NSEvent.magnification`.
const FIELD_ZOOM_VALUE: u32 = 113;
/// `kCGEventGestureRotationValue`: `NSEvent.rotation`, in degrees.
const FIELD_ROTATION_VALUE: u32 = 114;
/// `kCGEventGestureSwipeValue`: a page swipe's direction (`SWIPE_*`); on a macOS 27 Dock swipe,
/// its swipe mask.
const FIELD_SWIPE_VALUE: u32 = 115;
/// `kCGEventGestureSwipeMotion`: which Dock gesture (`DockAxis::motion`).
const FIELD_SWIPE_MOTION: u32 = 123;
/// `kCGEventGestureSwipeProgress`: distance since the swipe began (stored as a float).
const FIELD_SWIPE_PROGRESS: u32 = 124;
const FIELD_SWIPE_POSITION_X: u32 = 125;
const FIELD_SWIPE_POSITION_Y: u32 = 126;
/// `kCGEventGestureSwipeVelocityX/Y`: exit velocity, on ended or cancelled.
const FIELD_SWIPE_VELOCITY_X: u32 = 129;
const FIELD_SWIPE_VELOCITY_Y: u32 = 130;
/// `kCGEventGesturePhase`: `GesturePhase::bits`.
const FIELD_PHASE: u32 = 132;
/// `kCGEventGestureSwipeMask`: separate storage, which Mac Mouse Fix and Space Rabbit set to the
/// phase ("not sure if necessary").
const FIELD_PHASE_MIRROR: u32 = 134;
/// `kCGEventSwipeGestureFlagBits`: a Dock swipe's "inverted from device" flag.
const FIELD_SWIPE_INVERTED: u32 = 136;
/// `kCGEventGestureFlavor`.
const FIELD_FLAVOR: u32 = 138;
/// A gesture timestamp the macOS 27 Dock checks (Space Rabbit).
const FIELD_GESTURE_TIMESTAMP: u32 = 169;
/// A record that exists only in the serialized event: an IOHID event queue element, which the
/// macOS 27 Dock reads instead of the fields (FasterSwiper, Space Rabbit; Hammerspoon appends
/// one too).
const RECORD_IOHID_EVENT: u16 = 4205;

// IOHID event types (field 110, and the serialized IOHID event).
const HID_ROTATION: i64 = 5;
const HID_ZOOM: i64 = 8;
const HID_NAVIGATION_SWIPE: i64 = 16;
/// Smart zoom ("ZoomToggle"; Hammerspoon's `kTLInfoSubtypeSmartMagnify` 0x16).
const HID_SMART_ZOOM: i64 = 22;
/// `kIOHIDEventTypeDockSwipe`. Space Rabbit calls it "FluidTouchGesture"; same number.
const HID_DOCK_SWIPE: i64 = 23;
const HID_VELOCITY: u32 = 9;
/// `kIOHIDGestureFlavorDockPrimary`.
const FLAVOR_DOCK_PRIMARY: u16 = 3;

/// Page swipe directions (`IOHIDSwipeMask`), which AppKit turns into `NSEvent.deltaX/Y`: up is
/// deltaY +1, down -1, left deltaX +1, right -1 (checked in memory).
const SWIPE_UP: i64 = 1;
const SWIPE_DOWN: i64 = 2;
const SWIPE_LEFT: i64 = 4;
const SWIPE_RIGHT: i64 = 8;

/// `NX_NONCOALSESCEDMASK`, which Hammerspoon sets on the gestures it posts.
const FLAG_NON_COALESCED: u64 = 0x100;

// IOHID event fields for Apple's decoder (type << 16 | index; found in memory on macOS 26.5).
const HID_FIELD_DOCK_SWIPE_MOTION: u32 = ((HID_DOCK_SWIPE as u32) << 16) | 1;
const HID_FIELD_DOCK_SWIPE_PROGRESS: u32 = ((HID_DOCK_SWIPE as u32) << 16) | 2;
const HID_FIELD_DOCK_SWIPE_FLAVOR: u32 = ((HID_DOCK_SWIPE as u32) << 16) | 5;
const HID_FIELD_VELOCITY_X: u32 = HID_VELOCITY << 16;
const HID_FIELD_VELOCITY_Y: u32 = (HID_VELOCITY << 16) | 1;

/// The smallest 16.16 step: the progress of a synthesized swipe's start, which must not be 0 or
/// the direction is lost (Space Rabbit).
pub const PROGRESS_EPSILON: f64 = 1.0 / 65536.0;

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
}

// MARK: Direction conventions

/// How an axis's values on some macOS version relate to the wire's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sign {
    Same,
    /// Opposite while Natural scrolling is on.
    AgainstNaturalScrolling,
}

/// The direction table (horizontal, vertical, pinch), by the macOS major version each row
/// starts at. Up to 26 the Dock's posted and reported values agree with the wire's on every axis
/// (yabai, ISS, Space Rabbit). From 27 on, the Dock reads a posted horizontal swipe through the
/// Natural scrolling setting (Space Rabbit, measured on 26A5416b, assumed for 27.0). Vertical and
/// pinch on 27 are pending calibration with a real trackpad: the sources disagree.
const DIRECTIONS: &[(u64, [Sign; 3])] = &[(0, [Sign::Same; 3]), (27, [Sign::AgainstNaturalScrolling, Sign::Same, Sign::Same])];

/// How often [`dock_direction`] reads the Natural scrolling setting again.
const NATURAL_SCROLLING_REFRESH: Duration = Duration::from_secs(2);

/// The factor between this Mac's own Dock gesture values (progress and velocities, as its
/// trackpad reports them and as its Dock expects them posted) and the wire's. The same factor
/// is used both ways, so two Macs set up alike replay each other's gestures exactly.
///
/// `LANKVM_DOCK_DIRECTION` overrides it for calibration, e.g. `h=-1,v=1,p=1` (axes not named
/// keep the table's). Cheap: called for every gesture event on the viewer's main thread.
pub fn dock_direction(axis: DockAxis) -> f64 {
    if let Some(forced) = direction_override()[axis_index(axis)] {
        return forced;
    }
    direction_for(axis, os_major(), natural_scrolling())
}

fn direction_for(axis: DockAxis, os_major: u64, natural_scrolling: bool) -> f64 {
    match (row(DIRECTIONS, os_major)[axis_index(axis)], natural_scrolling) {
        (Sign::AgainstNaturalScrolling, true) => -1.0,
        _ => 1.0,
    }
}

fn direction_override() -> &'static [Option<f64>; 3] {
    static OVERRIDE: OnceLock<[Option<f64>; 3]> = OnceLock::new();
    OVERRIDE.get_or_init(|| {
        let Ok(value) = std::env::var("LANKVM_DOCK_DIRECTION") else { return [None; 3] };
        parse_direction_override(&value).unwrap_or_else(|| {
            tracing::warn!(value, "ignoring LANKVM_DOCK_DIRECTION (expected e.g. h=-1,v=1,p=1)");
            [None; 3]
        })
    })
}

/// `h=-1,v=1,p=1` (any subset, in any order): per-axis factors of ±1.
fn parse_direction_override(value: &str) -> Option<[Option<f64>; 3]> {
    let mut out = [None; 3];
    for part in value.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (axis, factor) = part.split_once('=')?;
        let index = match axis.trim() {
            "h" => 0,
            "v" => 1,
            "p" => 2,
            _ => return None,
        };
        out[index] = Some(match factor.trim() {
            "1" | "+1" => 1.0,
            "-1" => -1.0,
            _ => return None,
        });
    }
    Some(out)
}

/// Whether "Natural scrolling" is on: the global `com.apple.swipescrolldirection`, which is
/// missing while it has its default (on). Read again at most every couple of seconds. The Dock
/// itself samples the setting when it starts, so a change takes full effect after it restarts.
fn natural_scrolling() -> bool {
    static CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((read, natural)) = *cache
        && read.elapsed() < NATURAL_SCROLLING_REFRESH
    {
        return natural;
    }
    let natural = read_natural_scrolling();
    *cache = Some((Instant::now(), natural));
    natural
}

fn read_natural_scrolling() -> bool {
    use objc2_core_foundation::{CFPreferencesAppSynchronize, CFPreferencesGetAppBooleanValue, kCFPreferencesAnyApplication};
    // SAFETY: a constant CFString exported by CoreFoundation.
    let any_app = unsafe { kCFPreferencesAnyApplication };
    // Picks up a change made in System Settings since the last read.
    let _ = CFPreferencesAppSynchronize(any_app);
    let key = CFString::from_static_str("com.apple.swipescrolldirection");
    let mut exists = 0;
    // SAFETY: `exists` is a valid out pointer.
    let natural = unsafe { CFPreferencesGetAppBooleanValue(&key, any_app, &mut exists) };
    exists == 0 || natural
}

/// This Mac's macOS major version (26 for Tahoe).
pub fn os_major() -> u64 {
    static MAJOR: OnceLock<u64> = OnceLock::new();
    *MAJOR.get_or_init(|| {
        let version = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
        u64::try_from(version.majorVersion).unwrap_or(0)
    })
}

fn axis_index(axis: DockAxis) -> usize {
    match axis {
        DockAxis::Horizontal => 0,
        DockAxis::Vertical => 1,
        DockAxis::Pinch => 2,
    }
}

const AXES: [DockAxis; 3] = [DockAxis::Horizontal, DockAxis::Vertical, DockAxis::Pinch];

/// The last row of `table` that applies to `os_major`.
fn row<T: Copy>(table: &[(u64, T)], os_major: u64) -> T {
    table.iter().rev().find(|(from, _)| os_major >= *from).unwrap_or(&table[0]).1
}

// MARK: Dock swipe recipes

/// How Dock swipes are posted on this Mac.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockRecipe {
    /// Plain fields (Mac Mouse Fix before macOS 27, yabai, ISS): enough for horizontal swipes up
    /// to 26.
    Legacy,
    /// A few fields plus the swipe as an IOHID event, serialized into the event (Space Rabbit,
    /// FasterSwiper): needed for vertical swipes on 26 and for everything from 27 on.
    Payload,
    /// No fluid swipes: a swipe is recognised when it ends and becomes the matching action
    /// (see `InputState::set_dock_modes`).
    Discrete,
}

/// Which recipe each axis (horizontal, vertical, pinch) uses, by the macOS major version each
/// row starts at (K §4.2 H1). Data, so test results on real trackpads can update it. Up to 26
/// bare vertical swipes are rejected (Space Rabbit, on 26) while horizontal ones work; 27
/// ignores the plain fields (Mac Mouse Fix, ISS, Space Rabbit).
const RECIPES: &[(u64, [DockRecipe; 3])] = &[
    (0, [DockRecipe::Legacy, DockRecipe::Payload, DockRecipe::Payload]),
    (27, [DockRecipe::Payload; 3]),
];

/// `LANKVM_DOCK_RECIPE`: `auto` (the table), or one recipe for every axis.
fn parse_recipe_override(value: &str) -> Option<Option<DockRecipe>> {
    Some(match value.trim() {
        "" | "auto" => None,
        "legacy" => Some(DockRecipe::Legacy),
        "payload" => Some(DockRecipe::Payload),
        "discrete" => Some(DockRecipe::Discrete),
        _ => return None,
    })
}

/// Exit velocity of a synthesized one-Space swipe, by macOS major version: Mac Mouse Fix-like
/// below 27 (as the gesture poster experiment posts it), Space Rabbit's on 27.
fn hop_velocity(os_major: u64) -> f64 {
    if os_major >= 27 { 9999.0 } else { 8.0 }
}

/// How long after the end of a Dock swipe it is posted once more, if at all. The Dock sometimes
/// misses an end under load and stays stuck mid-transition; "sending the event again with a
/// delay of 200ms gets it unstuck almost always" (Mac Mouse Fix, before 27; untested on 27).
pub fn terminal_resend_delay() -> Option<Duration> {
    (os_major() <= 26).then_some(Duration::from_millis(200))
}

/// How this Mac's Dock takes swipes: per axis, the recipe and the direction factor
/// ([`dock_direction`]), plus the exit velocity of a synthesized one-Space swipe. Plain data, so
/// `InputState` stays pure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DockModes {
    /// Horizontal, vertical, pinch.
    pub recipes: [DockRecipe; 3],
    /// Horizontal, vertical, pinch: ±1.
    pub directions: [f64; 3],
    pub hop_velocity: f64,
}

impl DockModes {
    pub fn recipe(&self, axis: DockAxis) -> DockRecipe {
        self.recipes[axis_index(axis)]
    }

    pub fn direction(&self, axis: DockAxis) -> f64 {
        self.directions[axis_index(axis)]
    }
}

/// The table's choice for macOS 26 with every factor +1, the same on any Mac (recordings).
impl Default for DockModes {
    fn default() -> Self {
        Self { recipes: row(RECIPES, 26), directions: [1.0; 3], hop_velocity: hop_velocity(26) }
    }
}

/// This Mac's [`DockModes`]. The first call chooses the recipes and self-tests them (in memory);
/// later calls are cheap.
pub fn dock_modes() -> DockModes {
    DockModes { recipes: dock_recipes(), directions: AXES.map(dock_direction), hop_velocity: hop_velocity(os_major()) }
}

/// This Mac's recipe for swipes on `axis` (see [`dock_recipes`]).
pub fn dock_recipe(axis: DockAxis) -> DockRecipe {
    dock_recipes()[axis_index(axis)]
}

/// This Mac's recipes (horizontal, vertical, pinch), chosen and self-tested on first use.
pub fn dock_recipes() -> [DockRecipe; 3] {
    static RECIPES_HERE: OnceLock<[DockRecipe; 3]> = OnceLock::new();
    *RECIPES_HERE.get_or_init(|| {
        let value = std::env::var("LANKVM_DOCK_RECIPE").ok();
        let forced = value.as_deref().and_then(|v| {
            let forced = parse_recipe_override(v);
            if forced.is_none() {
                tracing::warn!(value = v, "ignoring LANKVM_DOCK_RECIPE (expected auto, legacy, payload or discrete)");
            }
            forced.flatten()
        });
        let recipes = choose_recipes(os_major(), forced);
        tracing::info!(horizontal = ?recipes[0], vertical = ?recipes[1], pinch = ?recipes[2], macos = os_major(), "dock swipe recipes");
        recipes
    })
}

/// The table's (or the forced) recipes, with any that fail the self-test replaced by Discrete.
fn choose_recipes(os_major: u64, forced: Option<DockRecipe>) -> [DockRecipe; 3] {
    let mut recipes = forced.map_or_else(|| row(RECIPES, os_major), |r| [r; 3]);
    for (axis, recipe) in AXES.into_iter().zip(recipes.iter_mut()) {
        if *recipe == DockRecipe::Discrete {
            continue;
        }
        if let Err(why) = self_test(*recipe, os_major, axis) {
            tracing::warn!(?axis, ?recipe, why, "dock swipe recipe failed its self-test; using discrete actions");
            *recipe = DockRecipe::Discrete;
        }
    }
    recipes
}

/// Builds a swipe on `axis` with `recipe` in memory and reads it back: the fields, the IOHID
/// event after the serialization round trip, and (when this macOS has it) Apple's own decode of
/// that event. Posts nothing.
fn self_test(recipe: DockRecipe, os_major: u64, axis: DockAxis) -> Result<(), String> {
    const TAG: i64 = crate::inject::INJECTED_TAG_PREFIX | 0x7E57;
    let samples = [
        DockSample { axis, phase: GesturePhase::Began, progress: PROGRESS_EPSILON, velocity_x: 0.0, velocity_y: 0.0, inverted: false },
        DockSample { axis, phase: GesturePhase::Changed, progress: -0.4, velocity_x: 0.0, velocity_y: 0.0, inverted: false },
        DockSample { axis, phase: GesturePhase::Ended, progress: 0.75, velocity_x: -2.5, velocity_y: 1.5, inverted: false },
    ];
    let check = |ok: bool, what: &str| if ok { Ok(()) } else { Err(what.to_string()) };
    for s in samples {
        let built = dock_swipe_events(recipe, os_major, &s, TAG).ok_or("could not build the event")?;
        let e = &built.dock;
        check(get_int(e, FIELD_EVENT_TYPE) == i64::from(EVENT_DOCK_CONTROL), "wrong event type")?;
        check(get_int(e, FIELD_SUBTYPE) == HID_DOCK_SWIPE, "wrong subtype")?;
        check(get_int(e, FIELD_SWIPE_MOTION) == i64::from(axis.motion()), "wrong motion")?;
        check(get_int(e, FIELD_PHASE) == i64::from(s.phase.bits()), "wrong phase")?;
        check(get_double(e, FIELD_SWIPE_PROGRESS) == f64::from(s.progress as f32), "wrong progress")?;
        check(get_int(e, CGEventField::EventSourceUserData.0) == TAG, "lost the tag")?;
        if recipe == DockRecipe::Payload {
            let payload = payload_of(e).ok_or("no IOHID event record after the round trip")?;
            let summary = parse_payload(&payload).ok_or("unreadable IOHID event record")?;
            summary.matches(&s).map_err(|why| format!("IOHID event record: {why}"))?;
            if let Some(decoder) = hid_decoder() {
                let decoded = decoder.decode(e).ok_or("Apple's decoder finds no IOHID event")?;
                decoded.matches(&s).map_err(|why| format!("Apple's decoder: {why}"))?;
            }
        }
    }
    Ok(())
}

// MARK: Dock swipe events

/// One Dock swipe sample as this Mac's Dock should get it: the values in its own direction
/// convention (the wire's times [`dock_direction`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DockSample {
    pub axis: DockAxis,
    pub phase: GesturePhase,
    /// Distance since the swipe began; ±1.0 is about one whole transition.
    pub progress: f64,
    /// Exit velocity, posted on ended or cancelled.
    pub velocity_x: f64,
    pub velocity_y: f64,
    /// Field 136, replayed as it came.
    pub inverted: bool,
}

/// The events that post one Dock swipe sample, in order, at the session tap.
pub struct DockEvents {
    pub dock: CFRetained<CGEvent>,
    /// A bare gesture event after the Dock event, for the recipes whose sources post one.
    pub companion: Option<CFRetained<CGEvent>>,
}

/// Builds (never posts) the events for one Dock swipe sample with `recipe` as macOS `os_major`
/// wants them, stamped with `user_data` (the tag with its relay depth). None for Discrete, or if
/// an event can't be made: then nothing of the sample may be posted.
///
/// Ported from the gesture poster experiment, whose events Apple's decoder reads back
/// correctly. Dock events come from no event source, as in every recipe known to work.
pub fn dock_swipe_events(recipe: DockRecipe, os_major: u64, s: &DockSample, user_data: i64) -> Option<DockEvents> {
    if recipe == DockRecipe::Discrete {
        return None;
    }
    let e = CGEvent::new(None)?;
    CGEvent::set_type(Some(&e), CGEventType(EVENT_DOCK_CONTROL));
    set_int(&e, FIELD_SUBTYPE, HID_DOCK_SWIPE);
    let phase = i64::from(s.phase.bits());
    set_int(&e, FIELD_PHASE, phase);
    set_int(&e, FIELD_SWIPE_MOTION, i64::from(s.axis.motion()));
    set_double(&e, FIELD_SWIPE_PROGRESS, s.progress);
    if s.phase.ends() {
        set_double(&e, FIELD_SWIPE_VELOCITY_X, s.velocity_x);
        set_double(&e, FIELD_SWIPE_VELOCITY_Y, s.velocity_y);
    }
    if s.inverted {
        set_int(&e, FIELD_SWIPE_INVERTED, 1);
    }
    match recipe {
        DockRecipe::Legacy => set_int(&e, FIELD_PHASE_MIRROR, phase),
        // What the macOS 27 Dock validates (Space Rabbit, measured on 27 seeds): the phase
        // mirror, the flavor, a timestamp and a non-zero position. Pinch gets the vertical
        // treatment, which is unverified.
        DockRecipe::Payload if os_major >= 27 => {
            set_int(&e, FIELD_PHASE_MIRROR, phase);
            set_double(&e, FIELD_FLAVOR, f64::from(FLAVOR_DOCK_PRIMARY));
            // SAFETY: no preconditions.
            set_double(&e, FIELD_GESTURE_TIMESTAMP, unsafe { mach_absolute_time() } as f64);
            if s.axis == DockAxis::Horizontal {
                set_double(&e, FIELD_SWIPE_POSITION_X, 0.1);
            } else {
                let up = s.progress >= 0.0;
                set_int(&e, FIELD_SWIPE_VALUE, if up { SWIPE_UP } else { SWIPE_DOWN });
                set_double(&e, FIELD_SWIPE_POSITION_Y, if up { 0.1 } else { -0.1 });
            }
        }
        DockRecipe::Payload | DockRecipe::Discrete => {}
    }
    let dock = if recipe == DockRecipe::Payload { with_payload(&e, &iohid_payload(&e))? } else { e };
    // After any rebuild: `CGEventCreateFromData` drops it.
    set_int(&dock, CGEventField::EventSourceUserData.0, user_data);
    let companion = if wants_companion(recipe, s.axis) { Some(companion_event(user_data)?) } else { None };
    Some(DockEvents { dock, companion })
}

/// The gesture poster's `--companion auto`: Mac Mouse Fix (before 27) and Space Rabbit's
/// horizontal paths post a bare gesture event after each Dock event; Space Rabbit's vertical
/// path, FasterSwiper and ISS never do.
fn wants_companion(recipe: DockRecipe, axis: DockAxis) -> bool {
    match recipe {
        DockRecipe::Legacy => true,
        DockRecipe::Payload => axis == DockAxis::Horizontal,
        DockRecipe::Discrete => false,
    }
}

fn companion_event(user_data: i64) -> Option<CFRetained<CGEvent>> {
    let e = CGEvent::new(None)?;
    CGEvent::set_type(Some(&e), CGEventType(EVENT_GESTURE));
    set_int(&e, CGEventField::EventSourceUserData.0, user_data);
    Some(e)
}

/// Float to signed 16.16 fixed point, saturating, keeping the sign of values too small for it
/// (a direction must not round to 0), as Space Rabbit, FasterSwiper and ISS do.
fn fixed_16_16(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    let fixed = (v * 65536.0).trunc().clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
    match fixed {
        0 if v > 0.0 => 1,
        0 if v < 0.0 => -1,
        _ => fixed,
    }
}

/// The IOHID event for the Dock swipe `e` describes, as a serialized queue element: a 28-byte
/// header (timestamp, sender, options, attribute length, event count), the DockSwipe event
/// (40 bytes) and, when there is a velocity or the swipe ended, a Velocity child (28 bytes).
/// Little endian, packed (FasterSwiper `gesture-serialization.cc`, Space Rabbit
/// `generateIOHIDPayload`). The timestamp must be real: the macOS 27 Dock checks it.
fn iohid_payload(e: &CGEvent) -> Vec<u8> {
    let phase = get_int(e, FIELD_PHASE) as u32 & 0xFF;
    let (vx, vy) = (get_double(e, FIELD_SWIPE_VELOCITY_X), get_double(e, FIELD_SWIPE_VELOCITY_Y));
    let with_velocity = vx != 0.0 || vy != 0.0 || phase == u32::from(GesturePhase::Ended.bits());
    let timestamp = match CGEvent::timestamp(Some(e)) {
        // SAFETY: no preconditions.
        0 => unsafe { mach_absolute_time() },
        t => t,
    };
    let mut p = Vec::with_capacity(96);
    p.extend(timestamp.to_le_bytes());
    p.extend(0u64.to_le_bytes());
    p.extend(0u32.to_le_bytes());
    p.extend(0u32.to_le_bytes());
    p.extend(if with_velocity { 2u32 } else { 1 }.to_le_bytes());
    // DockSwipe: size, type, options (the phase in the top byte), depth and padding, position
    // x/y/z, swipe mask, motion, flavor, progress.
    p.extend(40u32.to_le_bytes());
    p.extend((HID_DOCK_SWIPE as u32).to_le_bytes());
    p.extend((phase << 24).to_le_bytes());
    p.extend([0u8; 4]);
    p.extend(fixed_16_16(get_double(e, FIELD_SWIPE_POSITION_X)).to_le_bytes());
    p.extend(fixed_16_16(get_double(e, FIELD_SWIPE_POSITION_Y)).to_le_bytes());
    p.extend(0i32.to_le_bytes());
    p.extend((get_int(e, FIELD_SWIPE_VALUE) as u32).to_le_bytes());
    p.extend((get_int(e, FIELD_SWIPE_MOTION) as u16).to_le_bytes());
    p.extend(FLAVOR_DOCK_PRIMARY.to_le_bytes());
    p.extend(fixed_16_16(get_double(e, FIELD_SWIPE_PROGRESS)).to_le_bytes());
    if with_velocity {
        // Velocity: size, type, options, depth 1 and padding, x, y, z.
        p.extend(28u32.to_le_bytes());
        p.extend(HID_VELOCITY.to_le_bytes());
        p.extend(0u32.to_le_bytes());
        p.extend([1u8, 0, 0, 0]);
        p.extend(fixed_16_16(vx).to_le_bytes());
        p.extend(fixed_16_16(vy).to_le_bytes());
        p.extend(0i32.to_le_bytes());
    }
    p
}

/// What an IOHID DockSwipe event says (from the serialized record or Apple's decoder).
#[derive(Debug, Clone, Copy, PartialEq)]
struct HidSwipe {
    phase: u32,
    motion: u16,
    flavor: u16,
    progress: f64,
    /// Exit velocity, if the event has a Velocity child.
    velocity: Option<(f64, f64)>,
}

impl HidSwipe {
    /// Whether it carries `s` (up to 16.16 rounding).
    fn matches(&self, s: &DockSample) -> Result<(), String> {
        let close = |a: f64, b: f64| (a - b).abs() <= 2.0 / 65536.0;
        if self.phase != u32::from(s.phase.bits()) {
            return Err(format!("phase {} for {:?}", self.phase, s.phase));
        }
        if self.motion != u16::from(s.axis.motion()) || self.flavor != FLAVOR_DOCK_PRIMARY {
            return Err(format!("motion {} flavor {}", self.motion, self.flavor));
        }
        if !close(self.progress, s.progress) {
            return Err(format!("progress {} for {}", self.progress, s.progress));
        }
        if s.phase == GesturePhase::Ended && !self.velocity.is_some_and(|(x, y)| close(x, s.velocity_x) && close(y, s.velocity_y)) {
            return Err(format!("velocity {:?} for ({}, {})", self.velocity, s.velocity_x, s.velocity_y));
        }
        Ok(())
    }
}

/// Reads back a payload made by [`iohid_payload`].
fn parse_payload(p: &[u8]) -> Option<HidSwipe> {
    let u32_at = |o: usize| p.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let u16_at = |o: usize| p.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let fixed_at = |o: usize| u32_at(o).map(|v| f64::from(v as i32) / 65536.0);
    let count = u32_at(24)?;
    let swipe = 28 + usize::try_from(u32_at(20)?).ok()?;
    if u32_at(swipe)? != 40 || u32_at(swipe + 4)? != HID_DOCK_SWIPE as u32 {
        return None;
    }
    let velocity = match count {
        1 => None,
        2 if u32_at(swipe + 40)? == 28 && u32_at(swipe + 44)? == HID_VELOCITY => Some((fixed_at(swipe + 56)?, fixed_at(swipe + 60)?)),
        _ => return None,
    };
    Some(HidSwipe {
        phase: u32_at(swipe + 8)? >> 24,
        motion: u16_at(swipe + 32)?,
        flavor: u16_at(swipe + 34)?,
        progress: fixed_at(swipe + 36)?,
        velocity,
    })
}

/// One record of a serialized Quartz event (version 2): big-endian size and `tag << 14 | field`,
/// then the value.
struct Record<'a> {
    field: u16,
    tag: u16,
    size: u16,
    bytes: &'a [u8],
}

fn parse_records(data: &[u8]) -> Option<Vec<Record<'_>>> {
    if data.get(..4)? != [0, 0, 0, 2] {
        return None;
    }
    let mut out = Vec::new();
    let mut at = 4;
    while at < data.len() {
        let head = data.get(at..at + 4)?;
        let size = u16::from_be_bytes([head[0], head[1]]);
        let tag_field = u16::from_be_bytes([head[2], head[3]]);
        let (tag, field) = (tag_field >> 14, tag_field & 0x3FFF);
        let len = match (tag, size) {
            (0, 1) | (3, 2) => 8,
            (0, n) if n > 1 => usize::from(n),
            (1, 1) | (3, 1) => 4,
            _ => return None,
        };
        out.push(Record { field, tag, size, bytes: data.get(at + 4..at + 4 + len)? });
        at += 4 + len;
    }
    Some(out)
}

fn serialize(records: &[Record]) -> Vec<u8> {
    let mut data = vec![0, 0, 0, 2];
    for r in records {
        data.extend(r.size.to_be_bytes());
        data.extend(((r.tag << 14) | (r.field & 0x3FFF)).to_be_bytes());
        data.extend(r.bytes);
    }
    data
}

/// A copy of `e` carrying `payload` as its IOHID event record (replacing any), rebuilt with the
/// public `CGEventCreateData` / `CGEventCreateFromData`. The copy has lost field 42.
fn with_payload(e: &CGEvent, payload: &[u8]) -> Option<CFRetained<CGEvent>> {
    let data = CGEvent::new_data(None, Some(e))?.to_vec();
    let mut records = parse_records(&data)?;
    records.retain(|r| r.field != RECORD_IOHID_EVENT);
    // A size of 1 would mean an 8-byte value.
    let size = u16::try_from(payload.len()).ok().filter(|&n| n > 1)?;
    records.push(Record { field: RECORD_IOHID_EVENT, tag: 0, size, bytes: payload });
    CGEvent::from_data(None, Some(&CFData::from_bytes(&serialize(&records))))
}

/// The IOHID event record of `e`, if it has one.
fn payload_of(e: &CGEvent) -> Option<Vec<u8>> {
    let data = CGEvent::new_data(None, Some(e))?.to_vec();
    parse_records(&data)?.into_iter().find(|r| r.field == RECORD_IOHID_EVENT).map(|r| r.bytes.to_vec())
}

/// Apple's own reading of the IOHID event attached to a Quartz event: SkyLight's
/// `CGEventCopyIOHIDEvent` and IOKit's getters, resolved at run time. None where any is missing.
struct HidDecoder {
    copy: CopyHidEvent,
    event_type: HidEventGetter<u32>,
    phase: HidEventGetter<u32>,
    integer: HidFieldGetter<isize>,
    float: HidFieldGetter<f64>,
}

type CopyHidEvent = unsafe extern "C" fn(*const CGEvent) -> *mut c_void;
type HidEventGetter<T> = unsafe extern "C" fn(*mut c_void) -> T;
type HidFieldGetter<T> = unsafe extern "C" fn(*mut c_void, u32) -> T;

fn hid_decoder() -> Option<&'static HidDecoder> {
    static DECODER: OnceLock<Option<HidDecoder>> = OnceLock::new();
    DECODER
        .get_or_init(|| {
            let copy = symbol(c"CGEventCopyIOHIDEvent").or_else(|| symbol(c"SLEventCopyIOHIDEvent")).or_else(|| {
                // SAFETY: a valid path; the handle is never closed.
                let skylight = unsafe { libc::dlopen(c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight".as_ptr(), libc::RTLD_LAZY) };
                if skylight.is_null() {
                    return None;
                }
                // SAFETY: a live handle and a C string.
                NonNull::new(unsafe { libc::dlsym(skylight, c"CGEventCopyIOHIDEvent".as_ptr()) })
            })?;
            let (event_type, phase) = (symbol(c"IOHIDEventGetType")?, symbol(c"IOHIDEventGetPhase")?);
            let (integer, float) = (symbol(c"IOHIDEventGetIntegerValue")?, symbol(c"IOHIDEventGetFloatValue")?);
            // SAFETY: these are the functions' C signatures (IOKit's IOHIDEvent.h; the copy
            // returns a +1 IOHIDEventRef or NULL).
            unsafe {
                Some(HidDecoder {
                    copy: std::mem::transmute::<*mut c_void, CopyHidEvent>(copy.as_ptr()),
                    event_type: std::mem::transmute::<*mut c_void, HidEventGetter<u32>>(event_type.as_ptr()),
                    phase: std::mem::transmute::<*mut c_void, HidEventGetter<u32>>(phase.as_ptr()),
                    integer: std::mem::transmute::<*mut c_void, HidFieldGetter<isize>>(integer.as_ptr()),
                    float: std::mem::transmute::<*mut c_void, HidFieldGetter<f64>>(float.as_ptr()),
                })
            }
        })
        .as_ref()
}

impl HidDecoder {
    /// The DockSwipe attached to `e`; None if it has no IOHID event or another kind.
    fn decode(&self, e: &CGEvent) -> Option<HidSwipe> {
        // SAFETY: `e` is a valid event; the result is +1 or null.
        let raw = NonNull::new(unsafe { (self.copy)(e) })?;
        // SAFETY: an owned CF object; released when this goes out of scope.
        let _owned = unsafe { CFRetained::from_raw(raw.cast::<CFType>()) };
        let hid = raw.as_ptr();
        // SAFETY: `hid` is a live IOHIDEventRef. The phase is masked in case the getter returns
        // a narrower type than declared here.
        unsafe {
            if (self.event_type)(hid) != HID_DOCK_SWIPE as u32 {
                return None;
            }
            Some(HidSwipe {
                phase: (self.phase)(hid) & 0xFF,
                motion: (self.integer)(hid, HID_FIELD_DOCK_SWIPE_MOTION) as u16,
                flavor: (self.integer)(hid, HID_FIELD_DOCK_SWIPE_FLAVOR) as u16,
                progress: (self.float)(hid, HID_FIELD_DOCK_SWIPE_PROGRESS),
                // The getters search the children; 0 when there is no Velocity child.
                velocity: Some(((self.float)(hid, HID_FIELD_VELOCITY_X), (self.float)(hid, HID_FIELD_VELOCITY_Y))),
            })
        }
    }
}

fn symbol(name: &CStr) -> Option<NonNull<c_void>> {
    // SAFETY: `name` is a C string; RTLD_DEFAULT searches every loaded image.
    NonNull::new(unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) })
}

// MARK: App gestures

/// A gesture event for the view under `at` (type 29), from `source` so it carries the poster's
/// tag. The subtype goes first: what the other setters write depends on it.
fn app_gesture_event(source: Option<&CGEventSource>, subtype: i64, at: Point, flags: u64) -> Option<CFRetained<CGEvent>> {
    let e = CGEvent::new(source)?;
    CGEvent::set_type(Some(&e), CGEventType(EVENT_GESTURE));
    set_int(&e, FIELD_SUBTYPE, subtype);
    // AppKit sends a gesture to the view under the pointer where it began.
    CGEvent::set_location(Some(&e), CGPoint { x: at.x, y: at.y });
    CGEvent::set_flags(Some(&e), CGEventFlags(flags | FLAG_NON_COALESCED));
    Some(e)
}

/// Pinch: `delta` as `NSEvent.magnification`.
pub fn magnify_event(source: Option<&CGEventSource>, at: Point, phase: GesturePhase, delta: f64, flags: u64) -> Option<CFRetained<CGEvent>> {
    let e = app_gesture_event(source, HID_ZOOM, at, flags)?;
    set_int(&e, FIELD_PHASE, i64::from(phase.bits()));
    set_double(&e, FIELD_ZOOM_VALUE, delta);
    Some(e)
}

/// Rotation: `degrees` as `NSEvent.rotation` (counterclockwise positive).
pub fn rotate_event(source: Option<&CGEventSource>, at: Point, phase: GesturePhase, degrees: f64, flags: u64) -> Option<CFRetained<CGEvent>> {
    let e = app_gesture_event(source, HID_ROTATION, at, flags)?;
    set_int(&e, FIELD_PHASE, i64::from(phase.bits()));
    set_double(&e, FIELD_ROTATION_VALUE, degrees);
    Some(e)
}

/// Two-finger double tap. It has no phase.
pub fn smart_magnify_event(source: Option<&CGEventSource>, at: Point, flags: u64) -> Option<CFRetained<CGEvent>> {
    app_gesture_event(source, HID_SMART_ZOOM, at, flags)
}

/// A swipe between pages, as `NSEvent.deltaX/Y` (-1, 0 or 1 each): a Began event with the
/// direction, then an Ended one without (Hammerspoon). None if it has no direction.
pub fn navigation_swipe_events(source: Option<&CGEventSource>, at: Point, dx: i8, dy: i8, flags: u64) -> Option<[CFRetained<CGEvent>; 2]> {
    let mask = match dx.signum() {
        1 => SWIPE_LEFT,
        -1 => SWIPE_RIGHT,
        _ => 0,
    } | match dy.signum() {
        1 => SWIPE_UP,
        -1 => SWIPE_DOWN,
        _ => 0,
    };
    if mask == 0 {
        return None;
    }
    let began = app_gesture_event(source, HID_NAVIGATION_SWIPE, at, flags)?;
    set_int(&began, FIELD_PHASE, i64::from(GesturePhase::Began.bits()));
    set_int(&began, FIELD_SWIPE_VALUE, mask);
    let ended = app_gesture_event(source, HID_NAVIGATION_SWIPE, at, flags)?;
    set_int(&ended, FIELD_PHASE, i64::from(GesturePhase::Ended.bits()));
    Some([began, ended])
}

// MARK: System actions

const MISSION_CONTROL_APP: &str = "/System/Applications/Mission Control.app";
/// Launchpad's replacement from macOS 26 on (`Launchpad.app` is gone there).
const APPS_APP: &str = "/System/Applications/Apps.app";

/// Does `action` on this Mac as a whole, through the Dock's own notifications, or by opening the
/// app that does it. Doesn't block (an app opens on its own thread). Posts no events, so it
/// needs no tag: nothing loops through a viewer.
pub fn perform(action: SystemAction) {
    let done = match action {
        SystemAction::MissionControl => dock_notification("com.apple.expose.awake") || open_app(MISSION_CONTROL_APP),
        SystemAction::AppExpose => dock_notification("com.apple.expose.front.awake"),
        SystemAction::ShowDesktop => dock_notification("com.apple.showdesktop.awake"),
        // `com.apple.launchpad.toggle` does nothing on 26, which replaced Launchpad with Apps
        // (OpenLogi).
        SystemAction::Launchpad if os_major() >= 26 => open_app(APPS_APP),
        SystemAction::Launchpad => dock_notification("com.apple.launchpad.toggle"),
        // `InputState` makes these Dock swipes; they only get here when no swipe recipe works.
        SystemAction::PreviousSpace | SystemAction::NextSpace => false,
    };
    if !done {
        tracing::warn!(?action, "system action unavailable on this mac");
    }
}

/// Sends `CoreDockSendNotification(name, 0)` (HIServices; yabai's declaration). False if this
/// macOS doesn't have it. What it returns is not known to mean anything (yabai ignores it), so
/// it is only logged: treating it as a failure could do a toggling action twice.
fn dock_notification(name: &str) -> bool {
    type SendNotification = unsafe extern "C" fn(*const CFString, i32) -> i32;
    let Some(send) = symbol(c"CoreDockSendNotification") else { return false };
    // SAFETY: the function's signature.
    let send = unsafe { std::mem::transmute::<*mut c_void, SendNotification>(send.as_ptr()) };
    let cf_name = CFString::from_str(name);
    // SAFETY: `cf_name` is a valid CFString for the call.
    let result = unsafe { send(&*cf_name, 0) };
    if result != 0 {
        tracing::debug!(name, result, "dock notification returned an error code");
    }
    true
}

/// Opens the app at `path` without waiting for it.
fn open_app(path: &'static str) -> bool {
    if !std::path::Path::new(path).exists() {
        return false;
    }
    let opener = std::thread::Builder::new().name("lankvm-open-app".into());
    let opened = opener.spawn(move || match std::process::Command::new("/usr/bin/open").arg(path).status() {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(path, %status, "open failed"),
        Err(e) => tracing::warn!(path, "open failed: {e}"),
    });
    opened.is_ok()
}

// MARK: Fields

fn set_int(e: &CGEvent, field: u32, value: i64) {
    CGEvent::set_integer_value_field(Some(e), CGEventField(field), value);
}

fn set_double(e: &CGEvent, field: u32, value: f64) {
    CGEvent::set_double_value_field(Some(e), CGEventField(field), value);
}

fn get_int(e: &CGEvent, field: u32) -> i64 {
    CGEvent::integer_value_field(Some(e), CGEventField(field))
}

fn get_double(e: &CGEvent, field: u32) -> f64 {
    CGEvent::double_value_field(Some(e), CGEventField(field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventPhase, NSEventType};
    use objc2_core_graphics::CGEventSourceStateID;

    const TAG: i64 = crate::inject::INJECTED_TAG_PREFIX | 0x0012_3456;

    fn sample(axis: DockAxis, phase: GesturePhase, progress: f64) -> DockSample {
        DockSample { axis, phase, progress, velocity_x: 0.0, velocity_y: 0.0, inverted: false }
    }

    #[test]
    fn directions_follow_the_table() {
        for axis in AXES {
            for natural in [false, true] {
                assert_eq!(direction_for(axis, 26, natural), 1.0, "{axis:?} on 26");
                assert_eq!(direction_for(axis, 14, natural), 1.0, "{axis:?} on 14");
            }
        }
        assert_eq!(direction_for(DockAxis::Horizontal, 27, true), -1.0);
        assert_eq!(direction_for(DockAxis::Horizontal, 27, false), 1.0);
        assert_eq!(direction_for(DockAxis::Vertical, 27, true), 1.0, "pending calibration");
        assert_eq!(direction_for(DockAxis::Pinch, 28, true), 1.0);
        assert!([1.0, -1.0].contains(&dock_direction(DockAxis::Horizontal)));
    }

    #[test]
    fn direction_overrides_parse_strictly() {
        assert_eq!(parse_direction_override("h=-1,v=1,p=+1"), Some([Some(-1.0), Some(1.0), Some(1.0)]));
        assert_eq!(parse_direction_override(" v = -1 "), Some([None, Some(-1.0), None]));
        assert_eq!(parse_direction_override(""), Some([None; 3]));
        assert_eq!(parse_direction_override("h=2"), None);
        assert_eq!(parse_direction_override("x=1"), None);
        assert_eq!(parse_direction_override("h"), None);
    }

    #[test]
    fn recipes_follow_the_table_and_the_override() {
        use DockRecipe::*;
        assert_eq!(row(RECIPES, 15), [Legacy, Payload, Payload]);
        assert_eq!(row(RECIPES, 26), [Legacy, Payload, Payload]);
        assert_eq!(row(RECIPES, 27), [Payload; 3]);
        assert_eq!(row(RECIPES, 30), [Payload; 3]);
        assert_eq!(parse_recipe_override("auto"), Some(None));
        assert_eq!(parse_recipe_override("discrete"), Some(Some(Discrete)));
        assert_eq!(parse_recipe_override("fields"), None);
        assert_eq!(choose_recipes(26, Some(Discrete)), [Discrete; 3]);
        assert_eq!(DockModes::default().recipe(DockAxis::Horizontal), Legacy);
        assert_eq!(DockModes::default().direction(DockAxis::Pinch), 1.0);
    }

    /// The self-test is what keeps a broken recipe from being used, so it must pass where the
    /// recipes are known to build correctly: here, for both recipes, in both OS variants.
    #[test]
    fn every_recipe_passes_its_self_test_here() {
        for os in [26, 27] {
            for axis in AXES {
                for recipe in [DockRecipe::Legacy, DockRecipe::Payload] {
                    assert_eq!(self_test(recipe, os, axis), Ok(()), "{recipe:?} {axis:?} on {os}");
                }
            }
        }
        assert!(!dock_recipes().contains(&DockRecipe::Discrete), "{:?}", dock_recipes());
        assert!(hid_decoder().is_some(), "Apple's decoder is present on macOS 26");
    }

    #[test]
    fn fixed_point_keeps_tiny_values_and_saturates() {
        assert_eq!(fixed_16_16(1.0), 65536);
        assert_eq!(fixed_16_16(-0.75), -49152);
        assert_eq!(fixed_16_16(1e-9), 1);
        assert_eq!(fixed_16_16(-1e-9), -1);
        assert_eq!(fixed_16_16(0.0), 0);
        assert_eq!(fixed_16_16(f64::NAN), 0);
        assert_eq!(fixed_16_16(1e9), i32::MAX);
        assert_eq!(fixed_16_16(-1e9), i32::MIN);
    }

    #[test]
    fn serialized_events_parse_and_rebuild_unchanged() {
        let e = CGEvent::new(None).unwrap();
        CGEvent::set_type(Some(&e), CGEventType(EVENT_DOCK_CONTROL));
        set_int(&e, FIELD_SUBTYPE, HID_DOCK_SWIPE);
        set_double(&e, FIELD_SWIPE_PROGRESS, 0.3);
        let data = CGEvent::new_data(None, Some(&e)).unwrap().to_vec();
        let records = parse_records(&data).expect("version 2 serialization");
        assert!(records.iter().any(|r| r.field == FIELD_EVENT_TYPE as u16));
        assert_eq!(serialize(&records), data);
        assert!(parse_records(&[0, 0, 0, 1]).is_none(), "another version");
        assert!(parse_records(&data[..data.len() - 1]).is_none(), "truncated");
    }

    #[test]
    fn legacy_swipes_are_plain_fields() {
        let s = DockSample { axis: DockAxis::Horizontal, phase: GesturePhase::Ended, progress: -0.8, velocity_x: -3.0, velocity_y: 0.5, inverted: true };
        let built = dock_swipe_events(DockRecipe::Legacy, 26, &s, TAG).unwrap();
        let e = &built.dock;
        assert_eq!(CGEvent::r#type(Some(e)), CGEventType(EVENT_DOCK_CONTROL));
        assert_eq!(get_int(e, FIELD_EVENT_TYPE), 30);
        assert_eq!((get_int(e, FIELD_SUBTYPE), get_int(e, FIELD_SWIPE_MOTION)), (23, 1));
        assert_eq!((get_int(e, FIELD_PHASE), get_int(e, FIELD_PHASE_MIRROR)), (4, 4));
        assert_eq!(get_double(e, FIELD_SWIPE_PROGRESS), f64::from(-0.8f32));
        assert_eq!((get_double(e, FIELD_SWIPE_VELOCITY_X), get_double(e, FIELD_SWIPE_VELOCITY_Y)), (-3.0, 0.5));
        assert_eq!(get_int(e, FIELD_SWIPE_INVERTED), 1);
        assert_eq!(get_int(e, CGEventField::EventSourceUserData.0), TAG);
        assert!(payload_of(e).is_none());
        let companion = built.companion.expect("legacy swipes post a companion");
        assert_eq!(CGEvent::r#type(Some(&companion)), CGEventType(EVENT_GESTURE));
        assert_eq!(get_int(&companion, CGEventField::EventSourceUserData.0), TAG);
        // Velocities only on the end.
        let changed = dock_swipe_events(DockRecipe::Legacy, 26, &DockSample { phase: GesturePhase::Changed, ..s }, TAG).unwrap();
        assert_eq!(get_double(&changed.dock, FIELD_SWIPE_VELOCITY_X), 0.0);
        assert!(dock_swipe_events(DockRecipe::Discrete, 26, &s, TAG).is_none());
    }

    #[test]
    fn payload_swipes_carry_an_iohid_event_apple_can_read() {
        let began = sample(DockAxis::Vertical, GesturePhase::Began, PROGRESS_EPSILON);
        let built = dock_swipe_events(DockRecipe::Payload, 26, &began, TAG).unwrap();
        assert!(built.companion.is_none(), "vertical payload swipes have no companion");
        let payload = payload_of(&built.dock).unwrap();
        assert_eq!(payload.len(), 68);
        let summary = parse_payload(&payload).unwrap();
        assert_eq!(summary, HidSwipe { phase: 1, motion: 2, flavor: 3, progress: PROGRESS_EPSILON, velocity: None });
        assert_ne!(u64::from_le_bytes(payload[..8].try_into().unwrap()), 0, "a real timestamp");
        assert_eq!(get_int(&built.dock, CGEventField::EventSourceUserData.0), TAG, "stamped again after the rebuild");

        let ended = DockSample { axis: DockAxis::Pinch, phase: GesturePhase::Ended, progress: -1.25, velocity_x: 4.0, velocity_y: -2.0, inverted: false };
        let built = dock_swipe_events(DockRecipe::Payload, 26, &ended, TAG).unwrap();
        let payload = payload_of(&built.dock).unwrap();
        assert_eq!(payload.len(), 96);
        assert_eq!(parse_payload(&payload).unwrap().velocity, Some((4.0, -2.0)));
        let decoded = hid_decoder().unwrap().decode(&built.dock).expect("Apple's decoder reads it");
        assert_eq!(decoded.matches(&ended), Ok(()));
        assert_eq!(decoded.phase, 4);
        // A cancel without velocity has no Velocity child.
        let cancelled = sample(DockAxis::Vertical, GesturePhase::Cancelled, 0.3);
        let payload = payload_of(&dock_swipe_events(DockRecipe::Payload, 26, &cancelled, TAG).unwrap().dock).unwrap();
        assert_eq!(payload.len(), 68);
        // Horizontal payload swipes post a companion (Space Rabbit's horizontal path).
        let h = dock_swipe_events(DockRecipe::Payload, 26, &sample(DockAxis::Horizontal, GesturePhase::Changed, 0.5), TAG).unwrap();
        assert!(h.companion.is_some());
        // Legacy events carry no IOHID event.
        let legacy = dock_swipe_events(DockRecipe::Legacy, 26, &began, TAG).unwrap();
        assert_eq!(hid_decoder().unwrap().decode(&legacy.dock), None);
    }

    #[test]
    fn macos_27_swipes_get_the_extra_fields_without_losing_progress() {
        let up = sample(DockAxis::Vertical, GesturePhase::Changed, 0.6);
        let e = dock_swipe_events(DockRecipe::Payload, 27, &up, TAG).unwrap().dock;
        assert_eq!(get_int(&e, FIELD_PHASE_MIRROR), 2);
        assert_eq!(get_double(&e, FIELD_FLAVOR), 3.0);
        assert!(get_double(&e, FIELD_GESTURE_TIMESTAMP) > 0.0);
        assert_eq!((get_int(&e, FIELD_SWIPE_VALUE), get_double(&e, FIELD_SWIPE_POSITION_Y)), (SWIPE_UP, f64::from(0.1f32)));
        assert_eq!(get_double(&e, FIELD_SWIPE_PROGRESS), f64::from(0.6f32), "the extra fields don't alias the progress");
        assert_eq!(get_int(&e, FIELD_SWIPE_MOTION), 2);
        let payload = payload_of(&e).unwrap();
        let mask_at = 28 + 28;
        assert_eq!(u32::from_le_bytes(payload[mask_at..mask_at + 4].try_into().unwrap()), 1, "the swipe mask reaches the IOHID event");
        let down = dock_swipe_events(DockRecipe::Payload, 27, &sample(DockAxis::Vertical, GesturePhase::Changed, -0.6), TAG).unwrap();
        assert_eq!(get_int(&down.dock, FIELD_SWIPE_VALUE), SWIPE_DOWN);
        let right = dock_swipe_events(DockRecipe::Payload, 27, &sample(DockAxis::Horizontal, GesturePhase::Began, PROGRESS_EPSILON), TAG).unwrap();
        assert_eq!(get_double(&right.dock, FIELD_SWIPE_POSITION_X), f64::from(0.1f32));
        assert_eq!(get_int(&right.dock, FIELD_SWIPE_VALUE), 0);
        // Not on 26.
        let e = dock_swipe_events(DockRecipe::Payload, 26, &up, TAG).unwrap().dock;
        assert_eq!((get_int(&e, FIELD_PHASE_MIRROR), get_double(&e, FIELD_FLAVOR)), (0, 0.0));
    }

    fn ns(e: &CGEvent) -> objc2::rc::Retained<NSEvent> {
        NSEvent::eventWithCGEvent(e).expect("AppKit reads the event")
    }

    fn private_source() -> CFRetained<CGEventSource> {
        let source = CGEventSource::new(CGEventSourceStateID::Private).unwrap();
        CGEventSource::set_user_data(Some(&source), TAG);
        source
    }

    /// What AppKit makes of the app gestures: the events are only built and converted.
    #[test]
    fn app_gestures_become_the_matching_nsevents() {
        let source = private_source();
        let src = Some(&*source);
        let at = Point { x: 120.0, y: 80.0 };
        let phases = [
            (GesturePhase::Began, NSEventPhase::Began),
            (GesturePhase::Changed, NSEventPhase::Changed),
            (GesturePhase::Ended, NSEventPhase::Ended),
            (GesturePhase::Cancelled, NSEventPhase::Cancelled),
        ];
        for (phase, ns_phase) in phases {
            let e = magnify_event(src, at, phase, 0.05, crate::keys::SHIFT).unwrap();
            let m = ns(&e);
            assert_eq!((m.r#type(), m.phase()), (NSEventType::Magnify, ns_phase), "{phase:?}");
            assert!((m.magnification() - 0.05).abs() < 1e-6);
            assert!(m.modifierFlags().contains(NSEventModifierFlags::Shift));
            assert_eq!(get_int(&e, CGEventField::EventSourceUserData.0), TAG, "from the poster's source");
            let location = CGEvent::location(Some(&e));
            assert_eq!((location.x, location.y), (120.0, 80.0));

            let r = ns(&rotate_event(src, at, phase, -12.5, 0).unwrap());
            assert_eq!((r.r#type(), r.phase()), (NSEventType::Rotate, ns_phase));
            assert_eq!(r.rotation(), -12.5);
        }
        let tap = ns(&smart_magnify_event(src, at, 0).unwrap());
        assert_eq!(tap.r#type(), NSEventType::SmartMagnify);

        for (dx, dy, want) in [(1, 0, (1.0, 0.0)), (-1, 0, (-1.0, 0.0)), (0, 1, (0.0, 1.0)), (0, -1, (0.0, -1.0))] {
            let [began, ended] = navigation_swipe_events(src, at, dx, dy, 0).unwrap();
            let (b, e) = (ns(&began), ns(&ended));
            assert_eq!((b.r#type(), b.phase()), (NSEventType::Swipe, NSEventPhase::Began));
            assert_eq!((b.deltaX(), b.deltaY()), want, "dx {dx} dy {dy}");
            assert_eq!((e.r#type(), e.phase(), e.deltaX(), e.deltaY()), (NSEventType::Swipe, NSEventPhase::Ended, 0.0, 0.0));
        }
        assert!(navigation_swipe_events(src, at, 0, 0, 0).is_none());
    }

    #[test]
    fn the_dock_notification_function_resolves() {
        // Looked up only, never called.
        assert!(symbol(c"CoreDockSendNotification").is_some());
        assert!(std::path::Path::new(MISSION_CONTROL_APP).exists());
    }

    #[test]
    fn reads_the_natural_scrolling_setting() {
        // Either value is fine; the second read comes from the cache.
        assert_eq!(natural_scrolling(), natural_scrolling());
        assert!(os_major() >= 14, "{}", os_major());
    }
}
