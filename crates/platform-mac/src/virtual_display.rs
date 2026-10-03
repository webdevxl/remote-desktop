//! Virtual displays: screens that exist only in software. macOS treats one like any other display
//! (it gets windows, a menu bar and a Dock and can be made main or mirrored), so the host can make
//! one exactly the size of the viewer's screen and stream that instead of its own.
//!
//! This is CoreGraphics' `CGVirtualDisplay`, which is private API (BetterDisplay, DeskPad and
//! Chromium's tests use it). Its classes are looked up at run time, so a macOS without them fails
//! with an error instead of crashing. What it does, measured on macOS 26 (Apple silicon):
//! - With a private dispatch queue it works from any thread; the display is online about 100 ms
//!   after it is created, with its mode applied as soon as `applySettings:` returns.
//! - In a HiDPI mode, width and height are points and macOS draws twice as many pixels. That needs
//!   the descriptor's maximum pixel size to fit twice the mode, or macOS quietly drops to 1x.
//! - `applySettings:` again changes the mode in place; the display keeps its id and its windows.
//!   A new `CGVirtualDisplay` always gets a new id.
//! - Releasing the object removes the display within a few hundred milliseconds, and macOS puts
//!   main display and mirroring back by itself. So does LanKVM quitting or crashing. (In a process
//!   without an AppKit event loop, a display that took part in a display configuration stays until
//!   the process exits: tests must not arrange them.)

use std::ffi::CStr;
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CGSize};
use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration, CGConfigureDisplayMirrorOfDisplay,
    CGConfigureDisplayOrigin, CGConfigureDisplayWithDisplayMode, CGConfigureOption, CGDisplayBounds, CGDisplayConfigRef,
    CGDisplayCopyAllDisplayModes, CGDisplayCopyDisplayMode, CGDisplayIsActive, CGDisplayIsBuiltin, CGDisplayMirrorsDisplay,
    CGDisplayMode, CGDisplayVendorNumber, CGError, CGGetOnlineDisplayList, CGMainDisplayID, kCGDisplayShowDuplicateLowResolutionModes,
    kCGNullDirectDisplay,
};
use objc2_foundation::{NSArray, NSString};

/// Vendor number of LanKVM's virtual displays ("LK"), which tells them apart from real ones.
pub const VENDOR_ID: u32 = 0x4C4B;
const PRODUCT_ID: u32 = 0x0001;
/// Largest size, in pixels per side, a virtual display can have. Its descriptor allows this much
/// from the start, so any mode up to it applies in place. (HEVC encodes up to 16384 a side on Apple
/// silicon, but beyond about 8K it can't keep up with 30 fps.)
pub const MAX_PIXELS: u32 = 8192;
/// How long a new mode may take to show up as the display's current mode.
const MODE_TIMEOUT: Duration = Duration::from_secs(2);
/// A display just removed can linger in WindowServer for a moment, and making one with the same
/// serial number fails until it's gone: try this often, this far apart.
const CREATE_ATTEMPTS: u32 = 5;
const CREATE_RETRY: Duration = Duration::from_millis(400);

/// A display mode, in pixels: a HiDPI mode looks like half the size in each direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub hidpi: bool,
    pub refresh_hz: u32,
}

impl Mode {
    /// The size macOS lays the desktop out in (the mode is described to it in points).
    pub fn points(&self) -> (u32, u32) {
        if self.hidpi { (self.width / 2, self.height / 2) } else { (self.width, self.height) }
    }

    fn check(&self) -> Result<()> {
        if !(1..=MAX_PIXELS).contains(&self.width) || !(1..=MAX_PIXELS).contains(&self.height) {
            bail!("{}×{} is larger than {MAX_PIXELS}×{MAX_PIXELS}", self.width, self.height);
        }
        if self.hidpi && (self.width % 2 != 0 || self.height % 2 != 0) {
            bail!("a HiDPI mode needs an even size, not {}×{}", self.width, self.height);
        }
        Ok(())
    }

    /// A physical size giving the pixel density of Apple's own displays (about 218 ppi at 2x,
    /// 109 at 1x). macOS shows it in Displays settings; it doesn't change how things are drawn.
    fn size_mm(&self) -> CGSize {
        let ppi = if self.hidpi { 218.0 } else { 109.0 };
        CGSize { width: f64::from(self.width) / ppi * 25.4, height: f64::from(self.height) / ppi * 25.4 }
    }
}

/// Every private method used, with its type encoding (offsets left out) as macOS 26 has it. A
/// macOS whose methods differ is treated as having no virtual displays: calling a method with the
/// wrong argument types would crash the host, or worse.
const API: &[(&CStr, &str, &str)] = &[
    (c"CGVirtualDisplayDescriptor", "setDispatchQueue:", "v@:@"),
    (c"CGVirtualDisplayDescriptor", "setName:", "v@:@"),
    (c"CGVirtualDisplayDescriptor", "setMaxPixelsWide:", "v@:I"),
    (c"CGVirtualDisplayDescriptor", "setMaxPixelsHigh:", "v@:I"),
    (c"CGVirtualDisplayDescriptor", "setSizeInMillimeters:", "v@:{CGSize=dd}"),
    (c"CGVirtualDisplayDescriptor", "setVendorID:", "v@:I"),
    (c"CGVirtualDisplayDescriptor", "setProductID:", "v@:I"),
    (c"CGVirtualDisplayDescriptor", "setSerialNum:", "v@:I"),
    (c"CGVirtualDisplay", "initWithDescriptor:", "@@:@"),
    (c"CGVirtualDisplay", "applySettings:", "B@:@"),
    (c"CGVirtualDisplay", "displayID", "I@:"),
    (c"CGVirtualDisplaySettings", "setHiDPI:", "v@:I"),
    (c"CGVirtualDisplaySettings", "setModes:", "v@:@"),
    (c"CGVirtualDisplayMode", "initWithWidth:height:refreshRate:", "@@:IId"),
];

/// Whether this macOS has virtual displays, with exactly the methods this code calls.
pub fn supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| match check_api() {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("virtual displays unavailable: {e}");
            false
        }
    })
}

fn check_api() -> Result<()> {
    for (class_name, selector, want) in API {
        let class = class(class_name)?;
        let sel = objc2::runtime::Sel::register(&std::ffi::CString::new(*selector).expect("no NUL"));
        let method = class.instance_method(sel).with_context(|| format!("{} has no {selector}", class_name.to_string_lossy()))?;
        let types = unsafe { CStr::from_ptr(objc2::ffi::method_getTypeEncoding(method)) }.to_string_lossy();
        let have = without_offsets(&types);
        if have != *want {
            bail!("{} {selector} takes {have}, not {want}", class_name.to_string_lossy());
        }
    }
    Ok(())
}

/// A method type encoding without the stack offsets ("v24@0:8I16" -> "v@:I").
fn without_offsets(types: &str) -> String {
    types.chars().filter(|c| !c.is_ascii_digit()).collect()
}

fn class(name: &CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name).with_context(|| format!("this version of macOS has no virtual displays ({} is missing)", name.to_string_lossy()))
}

/// A virtual display. It exists as long as this value does.
pub struct VirtualDisplay {
    /// The `CGVirtualDisplay`; `None` only while dropping.
    display: Option<Retained<AnyObject>>,
    /// Where CoreGraphics calls the display back; it must outlive the display.
    _queue: DispatchRetained<DispatchQueue>,
    id: u32,
    mode: Mode,
}

// SAFETY: the display object is only told to apply settings and released, both of which
// CoreGraphics allows from any thread; its callbacks run on its own queue.
unsafe impl Send for VirtualDisplay {}

impl VirtualDisplay {
    /// Creates a display named `name` (shown in Displays settings) in `mode`. `serial` should be the
    /// same each time for the same purpose: macOS remembers a display's arrangement by it.
    pub fn create(name: &str, serial: u32, mode: Mode) -> Result<Self> {
        mode.check()?;
        if !supported() {
            bail!("this version of macOS has no virtual displays LanKVM can use");
        }
        let mut attempt = 1;
        loop {
            match Self::create_once(name, serial, mode) {
                Err(e) if attempt < CREATE_ATTEMPTS && e.downcast_ref::<Refused>().is_some() => {
                    tracing::debug!(attempt, "virtual display: {e}; trying again");
                    attempt += 1;
                    std::thread::sleep(CREATE_RETRY);
                }
                result => return result,
            }
        }
    }

    fn create_once(name: &str, serial: u32, mode: Mode) -> Result<Self> {
        let descriptor_class = class(c"CGVirtualDisplayDescriptor")?;
        let display_class = class(c"CGVirtualDisplay")?;
        // Autoreleased references to the display would otherwise keep it alive (and on screen)
        // after it is dropped, for as long as the calling thread's pool lives.
        autoreleasepool(|_| {
            let queue = DispatchQueue::new("lankvm.virtual-display", None);
            let display = unsafe {
                let descriptor: Retained<AnyObject> = msg_send![descriptor_class, new];
                let _: () = msg_send![&*descriptor, setDispatchQueue: &*queue];
                let _: () = msg_send![&*descriptor, setName: &*NSString::from_str(name)];
                let _: () = msg_send![&*descriptor, setMaxPixelsWide: MAX_PIXELS];
                let _: () = msg_send![&*descriptor, setMaxPixelsHigh: MAX_PIXELS];
                let _: () = msg_send![&*descriptor, setSizeInMillimeters: mode.size_mm()];
                let _: () = msg_send![&*descriptor, setVendorID: VENDOR_ID];
                let _: () = msg_send![&*descriptor, setProductID: PRODUCT_ID];
                let _: () = msg_send![&*descriptor, setSerialNum: serial];
                let allocated: Allocated<AnyObject> = msg_send![display_class, alloc];
                let display: Option<Retained<AnyObject>> = msg_send![allocated, initWithDescriptor: &*descriptor];
                display.ok_or(Refused)?
            };
            let id: u32 = unsafe { msg_send![&*display, displayID] };
            if id == kCGNullDirectDisplay {
                return Err(Refused.into());
            }
            let mut this = Self { display: Some(display), _queue: queue, id, mode };
            this.apply(mode)?;
            tracing::info!(id, ?mode, "virtual display created");
            Ok(this)
        })
    }

    /// The display's id, as CoreGraphics and ScreenCaptureKit know it.
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Switches to another mode in place: same display, same id, its windows stay. If macOS doesn't
    /// take it, the display goes back to the mode it had.
    pub fn set_mode(&mut self, mode: Mode) -> Result<()> {
        mode.check()?;
        let before = self.mode;
        if let Err(e) = autoreleasepool(|_| self.apply(mode)) {
            if let Err(back) = autoreleasepool(|_| self.apply(before)) {
                tracing::warn!(id = self.id, "virtual display: back to the mode it had: {back:#}");
            }
            return Err(e);
        }
        tracing::info!(id = self.id, ?mode, "virtual display mode changed");
        Ok(())
    }

    fn apply(&mut self, mode: Mode) -> Result<()> {
        let settings_class = class(c"CGVirtualDisplaySettings")?;
        let mode_class = class(c"CGVirtualDisplayMode")?;
        let display = self.display.as_deref().expect("present until drop");
        let (width, height) = mode.points();
        let applied: bool = unsafe {
            let allocated: Allocated<AnyObject> = msg_send![mode_class, alloc];
            let cg_mode: Option<Retained<AnyObject>> =
                msg_send![allocated, initWithWidth: width, height: height, refreshRate: f64::from(mode.refresh_hz)];
            let cg_mode = cg_mode.context("macOS refused the display mode")?;
            let settings: Retained<AnyObject> = msg_send![settings_class, new];
            let _: () = msg_send![&*settings, setHiDPI: u32::from(mode.hidpi)];
            let _: () = msg_send![&*settings, setModes: &*NSArray::from_retained_slice(&[cg_mode])];
            msg_send![display, applySettings: &*settings]
        };
        if !applied {
            bail!("macOS refused a {}×{} display mode", mode.width, mode.height);
        }
        // The mode is normally current by the time `applySettings:` returns. If macOS picked
        // another one (the 1x twin of a Retina mode, or one it remembered), choose ours. It counts
        // as the display's mode only once it shows.
        let started = Instant::now();
        let mut chose = false;
        loop {
            if shows(self.id, mode) {
                self.mode = mode;
                return Ok(());
            }
            let waited = started.elapsed();
            if !chose && waited >= Duration::from_millis(300) {
                chose = true;
                if let Err(e) = choose_mode(self.id, mode) {
                    tracing::warn!(id = self.id, "virtual display mode: {e:#}");
                }
            }
            if waited >= MODE_TIMEOUT {
                let now = pixel_size(self.id).map_or("none".to_string(), |(w, h)| format!("{w}×{h}"));
                bail!("the virtual display didn't switch to {}×{} (it shows {now})", mode.width, mode.height);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Puts the display back in its mode if macOS changed it (it can bring back a mode it
    /// remembered for this display seconds after it appeared). Returns whether it had to.
    pub fn ensure_mode(&self) -> Result<bool> {
        if shows(self.id, self.mode) {
            return Ok(false);
        }
        tracing::info!(id = self.id, mode = ?self.mode, "virtual display changed mode; changing it back");
        choose_mode(self.id, self.mode)?;
        Ok(true)
    }
}

/// `initWithDescriptor:` said no; it can work a moment later (see [`CREATE_ATTEMPTS`]).
#[derive(Debug)]
struct Refused;

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("macOS refused to create a virtual display")
    }
}

impl std::error::Error for Refused {}

/// Whether the display's current mode is `mode`, at its scale.
fn shows(display_id: u32, mode: Mode) -> bool {
    CGDisplayCopyDisplayMode(display_id).is_some_and(|m| is_mode(&m, mode))
}

fn is_mode(m: &CGDisplayMode, mode: Mode) -> bool {
    let (w, h) = mode.points();
    let m = Some(m);
    (CGDisplayMode::width(m), CGDisplayMode::height(m), CGDisplayMode::pixel_width(m), CGDisplayMode::pixel_height(m))
        == (w as usize, h as usize, mode.width as usize, mode.height as usize)
}

/// Switches the display to `mode` by picking it from the modes macOS offers for it (each declared
/// mode is offered at 2x and at 1x).
fn choose_mode(display_id: u32, mode: Mode) -> Result<()> {
    let key: &CFString = unsafe { kCGDisplayShowDuplicateLowResolutionModes };
    let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[&CFBoolean::new(true)]);
    let modes = unsafe { CGDisplayCopyAllDisplayModes(display_id, Some(options.as_opaque())) }.context("the display has no modes")?;
    // SAFETY: the array holds display modes.
    let modes = unsafe { modes.cast_unchecked::<CGDisplayMode>() };
    let chosen = modes.iter().find(|m| is_mode(m, mode)).with_context(|| format!("macOS doesn't offer {}×{}", mode.width, mode.height))?;
    configure("display mode", |cfg| {
        let err = unsafe { CGConfigureDisplayWithDisplayMode(cfg, display_id, Some(&chosen), None) };
        (err != CGError::Success).then_some(err)
    })
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        let id = self.id;
        autoreleasepool(|_| drop(self.display.take()));
        tracing::info!(id, "virtual display removed");
    }
}

/// A display's current mode size in pixels.
pub fn pixel_size(display_id: u32) -> Option<(u32, u32)> {
    let mode = CGDisplayCopyDisplayMode(display_id)?;
    let (w, h) = (CGDisplayMode::pixel_width(Some(&mode)), CGDisplayMode::pixel_height(Some(&mode)));
    (w > 0 && h > 0).then_some((w as u32, h as u32))
}

/// Every display connected now, real or virtual (mirrored ones too).
pub fn online_displays() -> Vec<u32> {
    let mut ids = [0u32; 32];
    let mut count = 0u32;
    let err = unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        tracing::warn!(?err, "list displays");
        return Vec::new();
    }
    ids[..count as usize].to_vec()
}

/// The main display's id (the one with the menu bar, at the origin of global coordinates).
pub fn main_display() -> u32 {
    CGMainDisplayID()
}

/// Whether `display_id` is one of LanKVM's virtual displays (this copy's or another's).
pub fn is_lankvm(display_id: u32) -> bool {
    CGDisplayVendorNumber(display_id) == VENDOR_ID
}

/// Whether the display shows its own picture: online, and not a mirror of another display.
pub fn is_active(display_id: u32) -> bool {
    CGDisplayIsActive(display_id)
}

/// How a virtual display sits among the Mac's own displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrangement {
    /// An extra display next to the others.
    Extend,
    /// Next to the others, and the main display: menu bar, Dock and new windows go to it.
    Main,
    /// The Mac's own displays mirror it, so every window is on it.
    Only,
}

/// Arranges the virtual display `id`. `previous_main` is the display to make main again when it
/// stops being main (if it's still there; otherwise the built-in or first real display).
///
/// Each step is a display configuration of its own, because the next one depends on where macOS
/// put things; then it checks that macOS did what was asked, and tries once more if not. They all
/// last only while LanKVM runs. Blocks for up to a second or so per step.
pub fn arrange(id: u32, arrangement: Arrangement, previous_main: Option<u32>) -> Result<()> {
    for attempt in 1..=2 {
        arrange_once(id, arrangement, previous_main)?;
        if arranged(id, arrangement) {
            return Ok(());
        }
        tracing::warn!(id, ?arrangement, attempt, "macOS didn't arrange the virtual display as asked");
    }
    bail!("macOS didn't arrange the display as asked")
}

/// Whether the virtual display `id` is arranged so: it shows its own picture, and has the main
/// role only for Main and Only.
pub fn arranged(id: u32, arrangement: Arrangement) -> bool {
    let main = CGMainDisplayID() == id;
    is_active(id) && CGDisplayMirrorsDisplay(id) == kCGNullDirectDisplay && main == (arrangement != Arrangement::Extend)
}

fn arrange_once(id: u32, arrangement: Arrangement, previous_main: Option<u32>) -> Result<()> {
    // macOS may have made the display a mirror itself (a setting it remembered for its serial
    // number): it must show its own picture.
    if CGDisplayMirrorsDisplay(id) != kCGNullDirectDisplay {
        configure("stop mirroring", |cfg| check(unsafe { CGConfigureDisplayMirrorOfDisplay(cfg, id, kCGNullDirectDisplay) }))?;
    }
    let real: Vec<u32> = online_displays().into_iter().filter(|&d| !is_lankvm(d)).collect();
    let mirroring: Vec<u32> = real.iter().copied().filter(|&d| CGDisplayMirrorsDisplay(d) == id).collect();
    if arrangement != Arrangement::Only && !mirroring.is_empty() {
        configure("stop mirroring", |cfg| {
            mirroring.iter().map(|&d| unsafe { CGConfigureDisplayMirrorOfDisplay(cfg, d, kCGNullDirectDisplay) }).find(|e| *e != CGError::Success)
        })?;
    }
    match arrangement {
        Arrangement::Only => {
            // One display at a time: one that can't mirror (an AirPlay display, another app's
            // virtual one) shouldn't keep the others from it.
            for d in real.into_iter().filter(|&d| CGDisplayMirrorsDisplay(d) != id) {
                if let Err(e) = configure("mirror", |cfg| check(unsafe { CGConfigureDisplayMirrorOfDisplay(cfg, d, id) })) {
                    tracing::warn!(display = d, "can't mirror the virtual display: {e:#}");
                }
            }
            if CGMainDisplayID() != id {
                make_main(id)?;
            }
        }
        Arrangement::Main => {
            if CGMainDisplayID() != id {
                make_main(id)?;
            }
        }
        Arrangement::Extend => {
            if CGMainDisplayID() == id {
                let real: Vec<u32> = online_displays().into_iter().filter(|&d| !is_lankvm(d) && is_active(d)).collect();
                let back = previous_main
                    .filter(|d| real.contains(d))
                    .or_else(|| real.iter().copied().max_by_key(|&d| CGDisplayIsBuiltin(d)))
                    .context("no other display to make main")?;
                make_main(back)?;
            }
        }
    }
    Ok(())
}

fn check(err: CGError) -> Option<CGError> {
    (err != CGError::Success).then_some(err)
}

/// Moves every display so that `id` sits at the origin, which makes it the main display, and the
/// others keep their places around it.
fn make_main(id: u32) -> Result<()> {
    let origin = CGDisplayBounds(id).origin;
    let displays: Vec<u32> = online_displays().into_iter().filter(|&d| is_active(d)).collect();
    configure("main display", |cfg| {
        displays
            .iter()
            .map(|&d| {
                let b = CGDisplayBounds(d).origin;
                unsafe { CGConfigureDisplayOrigin(cfg, d, (b.x - origin.x).round() as i32, (b.y - origin.y).round() as i32) }
            })
            .find(|e| *e != CGError::Success)
    })
}

/// Runs one display configuration: `steps` returns the first error, if any. It applies only while
/// this process runs, so a crash can't leave the Mac arranged around a display that's gone.
fn configure(what: &str, steps: impl FnOnce(CGDisplayConfigRef) -> Option<CGError>) -> Result<()> {
    autoreleasepool(|_| unsafe {
        let mut cfg: CGDisplayConfigRef = ptr::null_mut();
        let err = CGBeginDisplayConfiguration(&mut cfg);
        if err != CGError::Success {
            bail!("{what}: can't change the display configuration ({err:?})");
        }
        if let Some(err) = steps(cfg) {
            CGCancelDisplayConfiguration(cfg);
            bail!("{what}: {err:?}");
        }
        let started = Instant::now();
        let err = CGCompleteDisplayConfiguration(cfg, CGConfigureOption::ForAppOnly);
        tracing::debug!(what, ms = started.elapsed().as_secs_f64() * 1000.0, "display configuration");
        if err != CGError::Success {
            return Err(anyhow!("{what}: macOS refused the display configuration ({err:?})"));
        }
        Ok(())
    })
}

/// Calls `changed` (on the main thread) whenever the Mac's displays change: added, removed,
/// rearranged, mirrored or set to another mode, by LanKVM or anyone else (the lid, a cable, Displays
/// settings). Needs the main thread's run loop, which an app has. For the life of the process.
pub fn on_reconfiguration(changed: impl Fn() + Send + Sync + 'static) {
    use objc2_core_graphics::{CGDisplayChangeSummaryFlags, CGDisplayRegisterReconfigurationCallback};
    unsafe extern "C-unwind" fn callback(_display: u32, flags: CGDisplayChangeSummaryFlags, info: *mut std::ffi::c_void) {
        // Each change is announced twice: before (with this flag) and after.
        if flags.contains(CGDisplayChangeSummaryFlags::BeginConfigurationFlag) {
            return;
        }
        let changed = unsafe { &*(info as *const Box<dyn Fn() + Send + Sync>) };
        changed();
    }
    let changed: Box<Box<dyn Fn() + Send + Sync>> = Box::new(Box::new(changed));
    let err = unsafe { CGDisplayRegisterReconfigurationCallback(Some(callback), Box::into_raw(changed).cast()) };
    if err != CGError::Success {
        tracing::warn!(?err, "can't watch display changes");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_private_api_is_checked_without_offsets() {
        assert_eq!(without_offsets("@32@0:8I16I20d24"), "@@:IId");
        assert_eq!(without_offsets("v32@0:8{CGSize=dd}16"), "v@:{CGSize=dd}");
        // On this Mac every method is there with the expected types.
        check_api().unwrap();
    }

    #[test]
    fn modes_are_described_in_points() {
        let retina = Mode { width: 6144, height: 2560, hidpi: true, refresh_hz: 60 };
        assert_eq!(retina.points(), (3072, 1280));
        assert_eq!(Mode { hidpi: false, ..retina }.points(), (6144, 2560));
        // About 28 inches wide at Retina density.
        assert!((retina.size_mm().width - 715.8).abs() < 1.0);
    }

    #[test]
    fn modes_are_checked() {
        let ok = Mode { width: 6144, height: 2560, hidpi: true, refresh_hz: 60 };
        assert!(ok.check().is_ok());
        assert!(Mode { width: 8194, ..ok }.check().is_err());
        assert!(Mode { height: 0, ..ok }.check().is_err());
        assert!(Mode { width: 6143, ..ok }.check().is_err(), "half of an odd width isn't a point size");
        assert!(Mode { width: 6143, hidpi: false, ..ok }.check().is_ok());
    }

    #[test]
    fn this_macos_has_virtual_displays() {
        // Only looks the classes up; nothing is created.
        assert!(supported());
    }

    #[test]
    fn lists_the_main_display() {
        let main = CGMainDisplayID();
        assert!(online_displays().contains(&main));
        assert!(is_active(main));
        assert!(pixel_size(main).is_some());
    }
}
