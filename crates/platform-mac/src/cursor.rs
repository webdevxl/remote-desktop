//! The host's mouse cursor, so a controlling viewer can draw it locally (zero-latency pointer)
//! instead of waiting for it in the video.
//!
//! The shape comes from `NSCursor.currentSystemCursor`. It is deprecated ("will always be nil in a
//! future version of macOS") but still works on macOS 26; [`CursorMonitor`] reports
//! [`CursorUpdate::Unavailable`] when it stops working, so the host can put the cursor back into
//! the video. Reading it costs about a millisecond, so the monitor polls two cheap signals
//! instead and reads the shape only when they say something changed:
//! - `CGSCurrentCursorSeed` (private), which changes whenever any app sets a cursor;
//! - `CGCursorIsVisible` (deprecated), which goes false while an app hides the cursor (e.g. while
//!   typing). The seed does not change on hide.
//!
//! Both are looked up at run time, so a future macOS without them degrades instead of failing to
//! launch: without the seed, the shape is re-read on a timer; without visibility, the cursor counts
//! as always visible.

use std::collections::hash_map::DefaultHasher;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use objc2::AllocAnyThread;
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSCursor};
use objc2_foundation::NSDictionary;

/// A cursor image as PNG, with its size and hot spot in points (top-left origin).
#[derive(Debug, Clone, PartialEq)]
pub struct CursorImage {
    pub png: Vec<u8>,
    pub width: f64,
    pub height: f64,
    pub hot_x: f64,
    pub hot_y: f64,
}

/// Something the viewer should know about the cursor.
#[derive(Debug, Clone, PartialEq)]
pub enum CursorUpdate {
    /// A shape not sent before. `id` names it from now on.
    Shape { id: u32, image: Arc<CursorImage> },
    /// The cursor is visible with shape `id`.
    Show(u32),
    /// An app hid the cursor.
    Hide,
    /// This macOS no longer reports the cursor shape: draw it into the video instead.
    Unavailable,
}

/// How often the cheap change signals are read.
const POLL: Duration = Duration::from_millis(10);
/// Re-read the shape this often even if the seed didn't change (or doesn't exist).
const SAFETY_READ: Duration = Duration::from_millis(500);
/// Shapes remembered per monitor; animated cursors (the spinning wait cursor) cycle through many.
const MAX_SHAPES: usize = 64;
/// Consecutive failed shape reads before the cursor counts as unavailable. Reads happen on seed
/// changes and every [`SAFETY_READ`], so this spans at least a few hundred milliseconds.
const MISSES_UNAVAILABLE: u32 = 3;

type SeedFn = unsafe extern "C" fn() -> c_int;
type VisibleFn = unsafe extern "C" fn() -> c_int;

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// `RTLD_DEFAULT`: search every loaded image.
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

fn lookup(name: &CStr) -> *mut c_void {
    unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) }
}

/// Changes whenever an app sets a cursor (not on hide/unhide alone).
fn cursor_seed() -> Option<c_int> {
    let f = lookup(c"CGSCurrentCursorSeed");
    (!f.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, SeedFn>(f)() })
}

/// Whether the cursor is visible, or None if this macOS can't say.
pub fn cursor_visible() -> Option<bool> {
    let f = lookup(c"CGCursorIsVisible");
    (!f.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, VisibleFn>(f)() } != 0)
}

/// The cursor currently on screen (whichever app set it), at 2x resolution.
#[allow(deprecated)]
pub fn current_cursor() -> Option<CursorImage> {
    let cursor = NSCursor::currentSystemCursor()?;
    let image = cursor.image();
    let size = image.size();
    let hot = cursor.hotSpot();
    if size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    // Prefer the representation drawn for Retina (2x the point size); cursors also carry 1x, 5x
    // and 10x versions. Any representation can produce a CGImage at its own resolution.
    let reps = image.representations();
    let want = (size.width * 2.0).round() as isize;
    let rep = reps.iter().find(|r| r.pixelsWide() == want).or_else(|| reps.iter().max_by_key(|r| r.pixelsWide()))?;
    let cg = unsafe { rep.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }?;
    let bitmap = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg);
    let png = unsafe { bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
    Some(CursorImage { png: png.to_vec(), width: size.width, height: size.height, hot_x: hot.x, hot_y: hot.y })
}

fn shape_key(image: &CursorImage) -> u64 {
    let mut h = DefaultHasher::new();
    image.png.hash(&mut h);
    image.width.to_bits().hash(&mut h);
    image.height.to_bits().hash(&mut h);
    image.hot_x.to_bits().hash(&mut h);
    image.hot_y.to_bits().hash(&mut h);
    h.finish()
}

/// Turns raw readings into [`CursorUpdate`]s: assigns ids, sends each shape once, and reports
/// show/hide transitions. Separate from the polling thread so it can be tested.
pub struct CursorTracker {
    /// (key, id) of shapes already sent, most recently used last.
    known: Vec<(u64, u32)>,
    next_id: u32,
    current: Option<u32>,
    visible: bool,
    /// What the viewer was last told: `Some(Some(id))` shown, `Some(None)` hidden.
    told: Option<Option<u32>>,
    unavailable: bool,
    misses: u32,
}

impl Default for CursorTracker {
    fn default() -> Self {
        Self { known: Vec::new(), next_id: 0, current: None, visible: true, told: None, unavailable: false, misses: 0 }
    }
}

impl CursorTracker {
    /// A fresh shape reading. None means the system didn't return one.
    pub fn shape(&mut self, image: Option<CursorImage>, out: &mut Vec<CursorUpdate>) {
        let Some(image) = image else {
            // One failed read is not a missing API; only give up after several in a row.
            self.misses += 1;
            if self.misses >= MISSES_UNAVAILABLE && !self.unavailable {
                self.unavailable = true;
                self.told = None;
                out.push(CursorUpdate::Unavailable);
            }
            return;
        };
        self.misses = 0;
        self.unavailable = false;
        let key = shape_key(&image);
        let id = match self.known.iter().position(|(k, _)| *k == key) {
            Some(i) => {
                let entry = self.known.remove(i);
                self.known.push(entry);
                entry.1
            }
            None => {
                self.next_id = self.next_id.wrapping_add(1);
                let id = self.next_id;
                if self.known.len() == MAX_SHAPES {
                    self.known.remove(0);
                }
                self.known.push((key, id));
                out.push(CursorUpdate::Shape { id, image: Arc::new(image) });
                id
            }
        };
        self.current = Some(id);
        self.tell(out);
    }

    pub fn visibility(&mut self, visible: bool, out: &mut Vec<CursorUpdate>) {
        self.visible = visible;
        self.tell(out);
    }

    fn tell(&mut self, out: &mut Vec<CursorUpdate>) {
        let now = if self.visible { self.current } else { None };
        // While shapes can't be read, the last one known is stale: say nothing until one is.
        if self.unavailable || self.current.is_none() || self.told == Some(now) {
            return;
        }
        self.told = Some(now);
        out.push(match now {
            Some(id) => CursorUpdate::Show(id),
            None => CursorUpdate::Hide,
        });
    }
}

/// Watches the cursor on its own thread and reports changes until dropped.
pub struct CursorMonitor {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CursorMonitor {
    pub fn start(on_update: impl Fn(CursorUpdate) + Send + 'static) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new().name("lankvm-cursor".into()).spawn({
            let stop = stop.clone();
            move || {
                crate::system::set_thread_interactive();
                let mut tracker = CursorTracker::default();
                let mut out = Vec::new();
                let mut seed = None;
                let mut last_read: Option<Instant> = None;
                while !stop.load(Ordering::Relaxed) {
                    let new_seed = cursor_seed();
                    let due = last_read.is_none_or(|t| t.elapsed() >= SAFETY_READ);
                    if new_seed != seed || due {
                        seed = new_seed;
                        last_read = Some(Instant::now());
                        tracker.shape(current_cursor(), &mut out);
                    }
                    tracker.visibility(cursor_visible().unwrap_or(true), &mut out);
                    for update in out.drain(..) {
                        on_update(update);
                    }
                    std::thread::sleep(POLL);
                }
            }
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for CursorMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(seed: u8, hot: f64) -> CursorImage {
        CursorImage { png: vec![seed; 8], width: 28.0, height: 40.0, hot_x: hot, hot_y: 4.0 }
    }

    #[test]
    fn each_shape_is_sent_once_and_reused_by_id() {
        let mut t = CursorTracker::default();
        let mut out = Vec::new();
        t.visibility(true, &mut out);
        t.shape(Some(image(1, 4.5)), &mut out);
        t.shape(Some(image(1, 4.5)), &mut out);
        t.shape(Some(image(2, 4.5)), &mut out);
        t.shape(Some(image(1, 4.5)), &mut out);
        let summary: Vec<String> = out
            .iter()
            .map(|u| match u {
                CursorUpdate::Shape { id, .. } => format!("shape{id}"),
                CursorUpdate::Show(id) => format!("show{id}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(summary, ["shape1", "show1", "shape2", "show2", "show1"]);
    }

    #[test]
    fn hot_spot_alone_makes_a_different_shape() {
        // AppKit's arrow and the system arrow have identical pixels but different hot spots.
        let mut t = CursorTracker::default();
        let mut out = Vec::new();
        t.shape(Some(image(1, 4.5)), &mut out);
        t.shape(Some(image(1, 4.0)), &mut out);
        assert_eq!(out.iter().filter(|u| matches!(u, CursorUpdate::Shape { .. })).count(), 2);
    }

    #[test]
    fn hide_and_show() {
        let mut t = CursorTracker::default();
        let mut out = Vec::new();
        t.shape(Some(image(1, 4.5)), &mut out);
        t.visibility(true, &mut out);
        t.visibility(false, &mut out);
        t.visibility(false, &mut out);
        // A shape change while hidden is remembered but not shown.
        t.shape(Some(image(2, 4.5)), &mut out);
        t.visibility(true, &mut out);
        let all: Vec<_> = out.iter().map(|u| format!("{u:?}").split_whitespace().next().unwrap().to_string()).collect();
        assert_eq!(all, ["Shape", "Show(1)", "Hide", "Shape", "Show(2)"]);
    }

    #[test]
    fn missing_shape_is_reported_once_after_several_misses() {
        let mut t = CursorTracker::default();
        let mut out = Vec::new();
        t.shape(None, &mut out);
        t.shape(None, &mut out);
        assert!(out.is_empty(), "a transient miss doesn't flip the cursor into the video");
        t.shape(None, &mut out);
        t.shape(None, &mut out);
        assert_eq!(out, [CursorUpdate::Unavailable]);
        t.shape(Some(image(1, 4.5)), &mut out);
        assert!(matches!(out[1], CursorUpdate::Shape { .. }));
        assert!(matches!(out[2], CursorUpdate::Show(1)), "{out:?}");
    }

    #[test]
    fn a_stale_shape_isnt_shown_while_shapes_are_unavailable() {
        let mut t = CursorTracker::default();
        let mut out = Vec::new();
        t.shape(Some(image(1, 4.5)), &mut out);
        t.visibility(true, &mut out);
        out.clear();
        for _ in 0..MISSES_UNAVAILABLE {
            t.shape(None, &mut out);
        }
        t.visibility(true, &mut out);
        t.visibility(false, &mut out);
        t.visibility(true, &mut out);
        assert_eq!(out, [CursorUpdate::Unavailable]);
        t.shape(Some(image(1, 4.5)), &mut out);
        assert_eq!(out[1..], [CursorUpdate::Show(1)], "a known shape is announced again");
    }

    #[test]
    fn reads_the_real_cursor() {
        // Reading needs no permission. On a future macOS without the API this is None, which the
        // monitor reports as Unavailable.
        if let Some(c) = current_cursor() {
            assert!(c.png.starts_with(b"\x89PNG"));
            assert!(c.width > 0.0 && c.height > 0.0);
            assert!(c.hot_x >= 0.0 && c.hot_x <= c.width && c.hot_y >= 0.0 && c.hot_y <= c.height);
        }
        assert!(cursor_seed().is_some(), "CGSCurrentCursorSeed is exported on macOS 26");
        assert!(cursor_visible().is_some(), "CGCursorIsVisible is exported on macOS 26");
    }
}
