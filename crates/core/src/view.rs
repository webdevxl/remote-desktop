//! Draws a session's video into a `CAMetalLayer` owned by the UI, on a dedicated thread.
//!
//! Decoders publish tiles into a [`ViewSlot`]. Tiles of one captured frame (an update) are shown
//! together, so the picture is never half old, half new: the render thread wakes once every tile
//! of an update is in (or a deadline passes), copies them into its canvas and presents right
//! away. Nothing waits for the UI's main thread.

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{Message, msg_send, sel};
use objc2_core_foundation::CGSize;
use objc2_metal::{MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandQueue, MTLDrawable};
use objc2_quartz_core::{CALayer, CAMetalDrawable, CAMetalLayer};
use platform_mac::{CFRetained, CVPixelBuffer};
use protocol::{FULL_FRAME_TILE, MAX_TILES, TileRect};

use crate::client::Shared;
use crate::render::{self, Gpu, Part, TARGET_FORMAT, Texture, VideoRenderer};
use crate::stats::FrameTiming;

/// How long the tiles of an update wait for the rest of it (a lost tile, or one that is still
/// decoding) before they are shown anyway. The tiles of a full-screen update come out of the
/// host's encoders over ~20 ms; a lost one is rare (the client asks for it again after 60 ms).
const UPDATE_DEADLINE: Duration = Duration::from_millis(40);
/// Pending updates held at most; the oldest is shown when another one comes. Only reached if
/// updates keep coming incomplete faster than the deadline.
const MAX_PENDING_UPDATES: usize = 16;
/// How soon to try again after a draw found nothing to draw into (hidden window).
const REDRAW_RETRY: Duration = Duration::from_millis(50);
/// Index of the full-frame image in per-tile arrays.
const FULL: usize = FULL_FRAME_TILE as usize;
/// How far back (in updates) a place's content may be before it is treated as just "old": update
/// numbers are compared within half their range, and a place can stay unchanged for longer.
const OLD_UPDATE: u32 = 1 << 30;

/// A decoded tile on its way to the screen.
pub struct TileImage {
    pub pixel_buffer: CFRetained<CVPixelBuffer>,
    pub tile: TileRect,
    /// Size of the whole stream the tile is part of.
    pub stream: (u32, u32),
    /// [`protocol::VideoFrame::update`]: tiles of one captured frame share it.
    pub update: u32,
    /// [`protocol::VideoFrame::update_mask`]: the tiles that update has.
    pub update_mask: u64,
    /// [`protocol::VideoFrame::cover`]: for a whole-picture image, the tiles it paints (empty:
    /// all of it).
    #[allow(dead_code)] // Read once motion frames (scaled whole pictures) are drawn by their cover.
    pub cover: Vec<TileRect>,
    pub timing: FrameTiming,
}

// SAFETY: CVPixelBuffer is a thread-safe, reference-counted CoreFoundation object.
unsafe impl Send for TileImage {}

/// Wrapping-aware "a comes after b" (update numbers wrap).
fn is_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Where a tile goes, for [`Assembler`].
#[derive(Clone, Copy, Debug)]
struct TileKey {
    tile: TileRect,
    stream: (u32, u32),
    update: u32,
    update_mask: u64,
}

/// Tiles of one update that came in so far.
struct Group<T> {
    update: u32,
    /// The tiles the update has: the union of its tiles' `update_mask`s.
    expected: u64,
    /// Tiles in `tiles`, by bit ([`TileRect::bit`]).
    arrived: u64,
    /// Tiles handed to a decoder that haven't come out yet: an older update isn't passed over
    /// while they decode.
    decoding: u64,
    first: Instant,
    tiles: Vec<(TileRect, T)>,
}

impl<T> Group<T> {
    /// Whether it should be shown now: complete (but for `waiting` tiles, which won't come), or
    /// waited for long enough.
    fn due(&self, now: Instant, waiting: u64) -> bool {
        self.expected & !(self.arrived | waiting) == 0 || now >= self.first + UPDATE_DEADLINE
    }
}

/// What to show now.
struct Release<T> {
    stream: (u32, u32),
    /// Newest update shown complete (or as complete as it got), if any. `None` when this only
    /// fills in late tiles of updates already shown.
    update: Option<u32>,
    /// A full-frame image, copied first, and the tiles it must not overwrite because they show
    /// something newer.
    full: Option<(T, Vec<TileRect>)>,
    /// Tiles copied after it, at most one per tile index.
    tiles: Vec<T>,
}

/// Groups tiles into updates so each update is shown at once. No Metal here, so it is easy to
/// test.
///
/// An update is due once all its tiles are in, or [`UPDATE_DEADLINE`] after its first tile came;
/// it is then folded, with every older pending update, into `ready`, which keeps only the newest
/// image per tile index until the render thread takes it. So however long nobody draws, at most
/// one image per index plus the pending updates are held. A tile of an update older than one
/// already folded (it was late) goes to `ready` right away, unless something newer already covers
/// its place.
///
/// A full-frame image ([`FULL_FRAME_TILE`]) covers every tile: it marks every index as showing its
/// update, so older tiles never overwrite it, and is dropped if a newer full frame came first. If
/// a newer *tile* came first (decoders run in parallel, and a small tile decodes faster than the
/// whole picture), the full frame is still shown everywhere else.
struct Assembler<T> {
    stream: Option<(u32, u32)>,
    /// Newest update seen.
    newest: Option<u32>,
    /// Newest update folded into `ready`: tiles of updates up to it are late.
    folded: Option<u32>,
    /// Newest update folded since the last [`take_ready`](Self::take_ready), for its timing.
    ready_update: Option<u32>,
    /// Per tile index, the update its content comes from, in the canvas or in `ready` on its way.
    applied: [Option<u32>; MAX_TILES],
    /// Per tile index, the place its newest tile went (kept clear of a late full frame).
    rects: [Option<TileRect>; MAX_TILES],
    /// Pending updates, oldest first; all newer than `folded`.
    groups: Vec<Group<T>>,
    /// The newest image per tile index waiting to be shown, with its update. A full frame here
    /// is older than every tile here (it drops older ones).
    ready: [Option<(u32, T)>; MAX_TILES],
    /// Tiles whose frames the client drops until their keyframe comes: updates don't wait for
    /// them.
    waiting: u64,
}

impl<T> Default for Assembler<T> {
    fn default() -> Self {
        Self {
            stream: None,
            newest: None,
            folded: None,
            ready_update: None,
            applied: [None; MAX_TILES],
            rects: [None; MAX_TILES],
            groups: Vec::new(),
            ready: std::array::from_fn(|_| None),
            waiting: 0,
        }
    }
}

impl<T> Assembler<T> {
    /// Adds a tile. Returns whether the render thread should look now: something is ready, or
    /// the earliest deadline changed.
    /// Notes a tile's stream and update. False for a late tile of a stream before a switch.
    fn enter(&mut self, key: &TileKey) -> bool {
        if usize::from(key.tile.index) >= MAX_TILES {
            return false;
        }
        if self.stream != Some(key.stream) {
            // A late tile of the stream before the switch.
            if self.newest.is_some_and(|n| !is_newer(key.update, n)) {
                return false;
            }
            // A new stream size starts from a black canvas: nothing of the old one applies.
            self.stream = Some(key.stream);
            self.groups.clear();
            self.ready = std::array::from_fn(|_| None);
            self.ready_update = None;
            self.applied = [None; MAX_TILES];
            self.rects = [None; MAX_TILES];
        }
        if self.newest.is_none_or(|n| is_newer(key.update, n)) {
            self.newest = Some(key.update);
            // Keep what unchanged places show comparable with new updates, however long ago.
            for applied in self.applied.iter_mut().flatten() {
                if key.update.wrapping_sub(*applied) > OLD_UPDATE {
                    *applied = key.update.wrapping_sub(OLD_UPDATE);
                }
            }
        }
        true
    }

    /// The pending group of `key`'s update, made if need be (with `now` as its first sign), unless
    /// that update was shown already.
    fn group(&mut self, key: &TileKey, now: Instant) -> Option<&mut Group<T>> {
        if self.folded.is_some_and(|f| !is_newer(key.update, f)) {
            return None;
        }
        let at = self.groups.partition_point(|g| is_newer(key.update, g.update));
        if self.groups.get(at).is_none_or(|g| g.update != key.update) {
            self.groups.insert(at, Group { update: key.update, expected: 0, arrived: 0, decoding: 0, first: now, tiles: Vec::new() });
        }
        let group = &mut self.groups[at];
        group.expected |= key.update_mask;
        Some(group)
    }

    /// A tile went into its decoder: its update waits for it (until the deadline), and newer
    /// updates aren't shown before it. Returns whether the render thread should look (the
    /// earliest deadline changed).
    fn expect(&mut self, key: TileKey, now: Instant) -> bool {
        if !self.enter(&key) {
            return false;
        }
        let armed = self.groups.is_empty();
        match self.group(&key, now) {
            Some(group) => group.decoding |= key.tile.bit(),
            None => return false,
        }
        armed
    }

    /// A tile handed to a decoder won't come out (it failed).
    fn failed(&mut self, index: u8, update: u32, now: Instant) -> bool {
        let bit = 1u64 << (index as u32 % MAX_TILES as u32);
        if let Some(group) = self.groups.iter_mut().find(|g| g.update == update) {
            group.decoding &= !bit;
        }
        self.fold(now)
    }

    /// Adds a decoded tile. Returns whether the render thread should look now: something is
    /// ready, or the earliest deadline changed.
    fn push(&mut self, key: TileKey, item: T, now: Instant) -> bool {
        if !self.enter(&key) {
            return false;
        }
        let armed = self.groups.is_empty();
        let Some(group) = self.group(&key, now) else {
            // Its update was shown without it.
            return self.commit(key.tile, key.update, item);
        };
        let bit = key.tile.bit();
        group.decoding &= !bit;
        if group.arrived & bit != 0 {
            // Sent twice: keep the newer copy.
            group.tiles.retain(|(t, _)| t.index != key.tile.index);
        }
        group.arrived |= bit;
        group.tiles.push((key.tile, item));
        self.fold(now) || armed
    }

    /// Folds every due update, and all older ones, into `ready`. Returns whether any was.
    fn fold(&mut self, now: Instant) -> bool {
        // Everything up to the newest due update goes, except that an older update whose tiles
        // are still decoding holds the newer ones back (until its own deadline): shown first, the
        // newer one would paint over places the older then leaves showing older pictures still.
        let waiting = self.waiting;
        let mut due = 0;
        if let Some(newest) = self.groups.iter().rposition(|g| g.due(now, waiting)) {
            for (i, g) in self.groups.iter().enumerate().take(newest + 1) {
                if i < newest && !g.due(now, waiting) && g.decoding & !g.arrived & !waiting != 0 {
                    break;
                }
                due = i + 1;
            }
        }
        let cut = due.max(self.groups.len().saturating_sub(MAX_PENDING_UPDATES));
        if cut == 0 {
            return false;
        }
        let pending = self.groups.split_off(cut);
        for group in std::mem::replace(&mut self.groups, pending) {
            self.folded = Some(group.update);
            self.ready_update = Some(group.update);
            for (tile, item) in group.tiles {
                self.commit(tile, group.update, item);
            }
        }
        true
    }

    /// Puts an image into `ready` unless its place shows something at least as new. Returns
    /// whether it did.
    fn commit(&mut self, tile: TileRect, update: u32, item: T) -> bool {
        let index = usize::from(tile.index);
        if self.applied[index].is_some_and(|a| !is_newer(update, a)) {
            return false;
        }
        if index == FULL {
            // It covers every place that shows something older; their waiting images are stale.
            for i in 0..MAX_TILES {
                if self.applied[i].is_none_or(|a| is_newer(update, a)) {
                    self.applied[i] = Some(update);
                    self.ready[i] = None;
                }
            }
        } else {
            self.applied[index] = Some(update);
            self.rects[index] = Some(tile);
        }
        self.ready[index] = Some((update, item));
        true
    }

    /// Folds what is due. Returns whether anything waits to be shown.
    fn due(&mut self, now: Instant) -> bool {
        self.fold(now);
        self.ready.iter().any(Option::is_some)
    }

    /// When the earliest pending update is due even if incomplete. (Updates due already, held
    /// back by an older one still decoding, wait for that one's deadline, or for it to come out:
    /// that wakes the render thread anyway.)
    fn next_deadline(&self) -> Option<Instant> {
        let now = Instant::now();
        self.groups.iter().filter(|g| !g.due(now, self.waiting)).map(|g| g.first + UPDATE_DEADLINE).min()
    }

    /// The tiles that won't come until their keyframe does. Returns whether that makes an update
    /// due now.
    fn set_waiting(&mut self, waiting: u64, now: Instant) -> bool {
        self.waiting = waiting;
        self.fold(now)
    }

    /// Takes what should be shown now, if anything.
    fn take_ready(&mut self, now: Instant) -> Option<Release<T>> {
        self.fold(now);
        let stream = self.stream?;
        let update = self.ready_update.take();
        let full = self.ready[FULL].take().map(|(full_update, item)| {
            // Places showing something newer (from earlier, or from the tiles below) stay.
            let keep = (0..FULL)
                .filter(|&i| self.applied[i].is_some_and(|a| is_newer(a, full_update)))
                .filter_map(|i| self.rects[i])
                .collect();
            (item, keep)
        });
        let tiles: Vec<T> = self.ready[..FULL].iter_mut().filter_map(|r| r.take().map(|(_, item)| item)).collect();
        if full.is_none() && tiles.is_empty() {
            return None;
        }
        Some(Release { stream, update, full, tiles })
    }

    /// The canvas lost what the tiles in `mask` showed: images for them apply again, even older
    /// ones, until the resent tiles come.
    fn forget(&mut self, mask: u64) {
        let full = self.ready[FULL].as_ref().map(|(u, _)| *u);
        for i in 0..MAX_TILES {
            if mask & (1 << i) != 0 {
                // Still true for what waits in `ready`.
                self.applied[i] = self.ready[i].as_ref().map(|(u, _)| *u).or(full);
            }
        }
    }
}

#[derive(Default)]
struct SlotState {
    tiles: Assembler<TileImage>,
    size: Option<(u32, u32)>,
    /// The attached view whose render thread runs (0: none).
    active: u64,
}

/// Hand-off between the decoders and the render thread: holds tiles until their update is shown,
/// never a queue of frames.
#[derive(Default)]
pub struct ViewSlot {
    state: Mutex<SlotState>,
    wake: Condvar,
    /// The canvas, locked by the active view's render thread for each draw. It lives here, not
    /// with a view, so a new view shows the picture at once: only tiles that change are sent.
    /// Lock order: `renderer`, then `state`.
    renderer: Mutex<VideoRenderer>,
    /// The canvas lost tiles since the client last asked (see [`Self::take_canvas_lost`]).
    canvas_lost: AtomicBool,
}

impl ViewSlot {
    /// Hands a decoded tile to the render thread, which shows it together with the other tiles of
    /// its update.
    pub fn publish_tile(&self, image: TileImage) {
        let key = TileKey { tile: image.tile, stream: image.stream, update: image.update, update_mask: image.update_mask };
        let mut state = self.state.lock().unwrap();
        if state.tiles.push(key, image, Instant::now()) {
            drop(state);
            self.wake.notify_all();
        }
    }

    pub fn resize(&self, width: u32, height: u32) {
        self.state.lock().unwrap().size = Some((width.max(1), height.max(1)));
        self.wake.notify_all();
    }

    /// A tile went into its decoder (call before `decode`): its update waits for it, and newer
    /// updates aren't shown before it is.
    pub fn expect_tile(&self, tile: TileRect, stream: (u32, u32), update: u32, update_mask: u64) {
        let key = TileKey { tile, stream, update, update_mask };
        let mut state = self.state.lock().unwrap();
        if state.tiles.expect(key, Instant::now()) {
            drop(state);
            self.wake.notify_all();
        }
    }

    /// A tile handed to a decoder (see [`Self::expect_tile`]) won't come out.
    pub fn tile_failed(&self, index: u8, update: u32) {
        let mut state = self.state.lock().unwrap();
        if state.tiles.failed(index, update, Instant::now()) {
            drop(state);
            self.wake.notify_all();
        }
    }

    /// Tiles whose frames the client drops until their keyframe comes (bit `i` is tile `i`):
    /// updates are shown without them rather than waiting out the deadline.
    pub fn set_waiting(&self, tiles: u64) {
        let mut state = self.state.lock().unwrap();
        if state.tiles.waiting != tiles && state.tiles.set_waiting(tiles, Instant::now()) {
            drop(state);
            self.wake.notify_all();
        }
    }

    /// Whether the canvas lost part of the picture since the last call (a GPU error, an
    /// unreadable tile). Only changes are sent, so the client must then ask for every tile again.
    pub fn take_canvas_lost(&self) -> bool {
        self.canvas_lost.swap(false, Ordering::AcqRel)
    }

    /// The canvas lost what `mask`'s tiles showed (all of it if it has the full frame).
    fn lose_canvas(&self, mask: u64) {
        let mask = if mask & (1 << FULL) != 0 { u64::MAX } else { mask };
        self.state.lock().unwrap().tiles.forget(mask);
        self.canvas_lost.store(true, Ordering::Release);
    }
}

/// Keeps the layer alive while the render thread uses it.
struct LayerRef(Retained<CAMetalLayer>);
// SAFETY: CAMetalLayer may be used from a background thread for rendering.
unsafe impl Send for LayerRef {}

pub struct ViewHandle {
    slot: Arc<ViewSlot>,
    id: u64,
    thread: Option<JoinHandle<()>>,
}

impl ViewHandle {
    /// # Safety
    /// `layer` must be a valid `CAMetalLayer`.
    pub unsafe fn attach(layer: *mut c_void, width: u32, height: u32, shared: Arc<Shared>) -> Result<Self> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let gpu = render::gpu()?;
        // SAFETY: the caller passes a valid CAMetalLayer; retaining keeps it alive.
        let layer = unsafe { Retained::retain(layer.cast::<CAMetalLayer>()) }.context("null layer")?;
        configure(&layer, gpu, width, height);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let slot = shared.slot.clone();
        {
            let mut state = slot.state.lock().unwrap();
            state.active = id;
            state.size = None;
        }
        // Another view's thread (if any) sees it's no longer the active one and stops.
        slot.wake.notify_all();
        let layer = LayerRef(layer);
        let thread = std::thread::Builder::new().name("lankvm-render".into()).spawn(move || {
            let layer = layer;
            render_loop(gpu, &layer.0, &shared, id);
        })?;
        Ok(Self { slot, id, thread: Some(thread) })
    }
}

impl Drop for ViewHandle {
    fn drop(&mut self) {
        {
            let mut state = self.slot.state.lock().unwrap();
            if state.active == self.id {
                state.active = 0;
            }
        }
        self.slot.wake.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Sets the layer up as wgpu did (`Bgra8Unorm`, sRGB color space, opaque, `Immediate`
/// presentation), with at most two drawables so a frame is never queued behind another.
fn configure(layer: &CAMetalLayer, gpu: &Gpu, width: u32, height: u32) {
    layer.setDevice(Some(&gpu.device));
    layer.setPixelFormat(TARGET_FORMAT);
    layer.setFramebufferOnly(true);
    layer.setOpaque(true);
    // No color space: contents are taken as sRGB (what wgpu's `Auto` resolved to).
    layer.setColorspace(None);
    layer.setMaximumDrawableCount(2);
    // Present as soon as drawn instead of at the next vsync.
    layer.setDisplaySyncEnabled(false);
    set_drawable_size(layer, (width, height));
}

fn set_drawable_size(layer: &CAMetalLayer, (width, height): (u32, u32)) {
    layer.setDrawableSize(CGSize::new(f64::from(width.max(1)), f64::from(height.max(1))));
}

fn render_loop(gpu: &Gpu, layer: &CAMetalLayer, shared: &Arc<Shared>, id: u64) {
    let slot = &shared.slot;
    // Draw once right away: a kept canvas shows the picture before anything changes.
    let mut redraw_at = Some(Instant::now());
    loop {
        {
            let mut state = slot.state.lock().unwrap();
            loop {
                if state.active != id {
                    return;
                }
                let now = Instant::now();
                if state.tiles.due(now) || state.size.is_some() || redraw_at.is_some_and(|at| at <= now) {
                    break;
                }
                state = match [state.tiles.next_deadline(), redraw_at].into_iter().flatten().min() {
                    Some(at) => slot.wake.wait_timeout(state, at.saturating_duration_since(now)).unwrap().0,
                    None => slot.wake.wait(state).unwrap(),
                };
            }
        }
        // What to show is taken with the canvas locked, so a view that is being replaced can't
        // copy an older release over a newer one. A render thread that panicked left the canvas
        // in an unknown state: start a new one, and get every tile again.
        let mut renderer = slot.renderer.lock().unwrap_or_else(|poisoned| {
            let mut renderer = poisoned.into_inner();
            *renderer = VideoRenderer::default();
            slot.renderer.clear_poison();
            slot.lose_canvas(u64::MAX);
            renderer
        });
        let (release, size) = {
            let mut state = slot.state.lock().unwrap();
            if state.active != id {
                return;
            }
            (state.tiles.take_ready(Instant::now()), state.size.take())
        };
        if let Some(size) = size {
            set_drawable_size(layer, size);
        }
        let shown = autoreleasepool(|_| render_once(gpu, layer, &mut renderer, release, shared));
        drop(renderer);
        redraw_at = (!shown).then(|| Instant::now() + REDRAW_RETRY);
    }
}

/// Everything a command buffer reads that must outlive it: the decoder's pixel buffers (else
/// its pool may reuse a surface the GPU still copies from) and their textures.
struct InFlight {
    _tiles: Vec<TileImage>,
    _textures: Vec<Texture>,
}

/// Copies the released tiles into the canvas and draws it. Returns false if there was nothing to
/// draw into (hidden window, no drawable), so the caller tries again later. Content that doesn't
/// reach the canvas is reported lost.
fn render_once(gpu: &Gpu, layer: &CAMetalLayer, renderer: &mut VideoRenderer, release: Option<Release<TileImage>>, shared: &Arc<Shared>) -> bool {
    let slot = &shared.slot;
    let (mut images, mut keep) = (Vec::new(), Vec::new());
    let (mut stream, mut timing) = ((0, 0), None);
    if let Some(release) = release {
        stream = release.stream;
        if let Some((full, full_keep)) = release.full {
            images.push(full);
            keep = full_keep;
        }
        images.extend(release.tiles);
        timing = release.update.and_then(|update| update_timing(&images, update));
    }
    let written = images.iter().fold(0, |mask, image| mask | image.tile.bit());
    let Some(cb) = gpu.queue.commandBuffer() else {
        tracing::warn!("no Metal command buffer");
        if written != 0 {
            slot.lose_canvas(written);
        }
        return false;
    };
    let mut textures = Vec::new();
    if !images.is_empty() {
        // Only a full frame (always first) has places to keep.
        let parts = images.iter().enumerate().map(|(i, image)| Part {
            pixel_buffer: &image.pixel_buffer,
            tile: image.tile,
            keep: if i == 0 { &keep } else { &[] },
        });
        match renderer.apply(gpu, &cb, stream, parts) {
            Ok(applied) => {
                textures = applied.textures;
                if applied.failed != 0 {
                    slot.lose_canvas(applied.failed);
                }
            }
            Err(e) => {
                tracing::warn!("show tiles: {e:#}");
                slot.lose_canvas(u64::MAX);
            }
        }
    }
    let drawable = if window_visible(layer) { layer.nextDrawable() } else { None };
    if let Some(drawable) = &drawable {
        renderer.draw(gpu, &cb, &drawable.texture());
        if let Some(timing) = timing {
            on_screen(drawable, timing, shared.clone());
        }
        cb.presentDrawable(ProtocolObject::from_ref(&**drawable));
    }
    let in_flight = Cell::new(Some(InFlight { _tiles: images, _textures: textures }));
    let lost_to = (written != 0).then(|| slot.clone());
    let done = RcBlock::new(move |cb: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
        // SAFETY: Metal passes the completed command buffer, valid during the call.
        let failed = unsafe { cb.as_ref() }.status() == MTLCommandBufferStatus::Error;
        drop(in_flight.take());
        if let Some(slot) = lost_to.as_ref().filter(|_| failed) {
            // Which copies got through is unknown.
            tracing::warn!("Metal command buffer failed: the picture will be sent again");
            slot.lose_canvas(u64::MAX);
        }
    });
    // SAFETY: the block is valid; Metal copies it and calls it once the GPU is done.
    unsafe { cb.addCompletedHandler(RcBlock::as_ptr(&done)) };
    cb.commit();
    drawable.is_some()
}

/// The update's timing: when its last tile was decoded, and when it was captured.
fn update_timing(tiles: &[TileImage], update: u32) -> Option<FrameTiming> {
    let mut parts = tiles.iter().filter(|t| t.update == update).map(|t| t.timing);
    let first = parts.next()?;
    Some(parts.fold(first, |mut acc, t| {
        acc.decoded_us = acc.decoded_us.max(t.decoded_us);
        acc.capture_local_us = acc.capture_local_us.or(t.capture_local_us);
        acc
    }))
}

/// Records the update's latency when the drawable is really on the screen.
fn on_screen(drawable: &ProtocolObject<dyn CAMetalDrawable>, timing: FrameTiming, shared: Arc<Shared>) {
    let presented = RcBlock::new(move |drawable: NonNull<ProtocolObject<dyn MTLDrawable>>| {
        // SAFETY: Metal passes the presented drawable, valid during the call.
        let seconds = unsafe { drawable.as_ref() }.presentedTime();
        if seconds <= 0.0 {
            return; // skipped, never on screen
        }
        // Host time in seconds, the same mach timebase as `clock::now_us`.
        let glass_us = (seconds * 1e6) as u64;
        let mut stats = shared.stats.lock().unwrap();
        stats.present.add(glass_us.saturating_sub(timing.decoded_us) as f64);
        stats.display_samples.add(glass_us.saturating_sub(timing.decoded_us) as f64);
        if let Some(captured) = timing.capture_local_us {
            stats.total.add(glass_us.saturating_sub(captured) as f64);
            stats.total_samples.add(glass_us.saturating_sub(captured) as f64);
        }
        stats.on_shown();
    });
    // SAFETY: the block is valid; Metal copies it and calls it once.
    unsafe { drawable.addPresentedHandler(RcBlock::as_ptr(&presented)) };
}

/// Whether the window showing `layer` is on screen. In a hidden window presented drawables wait
/// for a vsync that never comes, and `nextDrawable` would then block for a second (wgpu skipped
/// those frames the same way).
fn window_visible(layer: &CAMetalLayer) -> bool {
    const NS_WINDOW_OCCLUSION_STATE_VISIBLE: usize = 1 << 1;
    // The first layer up the tree with a delegate is the backing layer of the hosting NSView.
    let mut current: Retained<CALayer> = Retained::into_super(layer.retain());
    loop {
        if let Some(delegate) = current.delegate() {
            if !delegate.respondsToSelector(sel!(window)) {
                return true;
            }
            // SAFETY: an NSView's `window`, an NSWindow or nil.
            let window: Option<Retained<NSObject>> = unsafe { msg_send![&*delegate, window] };
            let Some(window) = window else { return true };
            // SAFETY: `-[NSWindow occlusionState]` returns an NSUInteger bit set.
            let state: usize = unsafe { msg_send![&*window, occlusionState] };
            return state & NS_WINDOW_OCCLUSION_STATE_VISIBLE != 0;
        }
        match current.superlayer() {
            Some(parent) => current = parent,
            None => return true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::tests::{Target, assert_near, bgra, expected, nv12};

    const STREAM: (u32, u32) = (6144, 2560);

    /// Tile `index` of a test layout: 64×64 squares in a row.
    fn rect(index: u8) -> TileRect {
        if usize::from(index) == FULL {
            return TileRect { index, x: 0, y: 0, width: STREAM.0, height: STREAM.1 };
        }
        TileRect { index, x: u32::from(index) * 64, y: 0, width: 64, height: 64 }
    }

    /// The mask of an update with these tiles.
    fn mask(tiles: &[u8]) -> u64 {
        tiles.iter().fold(0, |m, &i| m | 1 << i)
    }

    const FULL_TILE: u8 = FULL_FRAME_TILE;

    /// Pushes tile `index` of `update` (which has the tiles in `update_mask`), with (index,
    /// update) as the payload.
    fn push(a: &mut Assembler<(u8, u32)>, index: u8, update: u32, update_mask: u64, at: Instant) -> bool {
        a.push(TileKey { tile: rect(index), stream: STREAM, update, update_mask }, (index, update), at)
    }

    fn sorted(release: Release<(u8, u32)>) -> (Option<u32>, Vec<(u8, u32)>) {
        assert!(release.full.is_none(), "unexpected full frame");
        let mut tiles = release.tiles;
        tiles.sort();
        (release.update, tiles)
    }

    /// An update whose tiles are still in their decoders holds back a newer one that decoded
    /// first (a small tile decodes faster than a full frame), so the screen never mixes them.
    #[test]
    fn a_newer_update_waits_for_an_older_one_still_decoding() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        let full = TileRect { index: FULL as u8, x: 0, y: 0, width: STREAM.0, height: STREAM.1 };
        // Update 1 is a full frame, update 2 one tile; both went into their decoders.
        assert!(a.expect(TileKey { tile: full, stream: STREAM, update: 1, update_mask: 1 << FULL }, t0));
        a.expect(TileKey { tile: rect(0), stream: STREAM, update: 2, update_mask: mask(&[0]) }, t0);
        // The tile decodes first: complete, but not shown before the full frame.
        push(&mut a, 0, 2, mask(&[0]), t0);
        assert!(a.take_ready(t0).is_none());
        // The full frame comes out: both go, the tile over it.
        assert!(a.push(TileKey { tile: full, stream: STREAM, update: 1, update_mask: 1 << FULL }, (FULL as u8, 1), t0));
        let release = a.take_ready(t0).unwrap();
        assert_eq!((release.update, release.full.as_ref().map(|(f, _)| *f)), (Some(2), Some((FULL as u8, 1))));
        assert_eq!(release.tiles, vec![(0, 2)]);
        // A decode that fails lets the newer update go at once.
        a.expect(TileKey { tile: rect(1), stream: STREAM, update: 3, update_mask: mask(&[1]) }, t0);
        push(&mut a, 0, 4, mask(&[0]), t0);
        assert!(a.take_ready(t0).is_none());
        assert!(a.failed(1, 3, t0));
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(4), vec![(0, 4)]));
        // And one that never comes out is passed over at its deadline.
        a.expect(TileKey { tile: rect(1), stream: STREAM, update: 5, update_mask: mask(&[1]) }, t0);
        push(&mut a, 0, 6, mask(&[0]), t0);
        assert!(a.take_ready(t0 + UPDATE_DEADLINE - Duration::from_millis(1)).is_none());
        assert_eq!(sorted(a.take_ready(t0 + UPDATE_DEADLINE).unwrap()), (Some(6), vec![(0, 6)]));
    }

    /// A tile waiting for its keyframe won't come: its updates are shown without it, at once.
    #[test]
    fn updates_dont_wait_for_tiles_awaiting_a_keyframe() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        let m = mask(&[0, 1]);
        assert!(push(&mut a, 0, 1, m, t0), "first pending update arms the deadline");
        assert!(a.take_ready(t0).is_none(), "tile 1 is still expected");
        assert!(a.set_waiting(mask(&[1]), t0), "tile 1 won't come: the update is due");
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(1), vec![(0, 1)]));
        // Later updates with it don't wait either, and once its keyframe is back they do again.
        assert!(push(&mut a, 0, 2, m, t0));
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(2), vec![(0, 2)]));
        assert!(!a.set_waiting(0, t0));
        push(&mut a, 0, 3, m, t0);
        assert!(a.take_ready(t0).is_none());
        assert!(push(&mut a, 1, 3, m, t0));
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(3), vec![(0, 3), (1, 3)]));
    }

    /// Images held: the ready ones plus the pending updates' tiles.
    fn held<T>(a: &Assembler<T>) -> usize {
        a.ready.iter().flatten().count() + a.groups.iter().map(|g| g.tiles.len()).sum::<usize>()
    }

    #[test]
    fn complete_update_is_shown_together() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        let m = mask(&[0, 1, 2]);
        assert!(push(&mut a, 0, 1, m, t0), "first pending update arms the deadline");
        assert!(!push(&mut a, 2, 1, m, t0));
        assert!(a.take_ready(t0).is_none());
        assert_eq!(a.next_deadline(), Some(t0 + UPDATE_DEADLINE));
        assert!(push(&mut a, 1, 1, m, t0), "complete");
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(1), vec![(0, 1), (1, 1), (2, 1)]));
        assert!(a.take_ready(t0).is_none());
        assert_eq!(a.next_deadline(), None);
    }

    #[test]
    fn completion_follows_the_mask() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        // Tiles 3 and 40, not the first two indexes: a count would be fooled by duplicates or
        // by tiles of other updates.
        let m = mask(&[3, 40]);
        push(&mut a, 3, 1, m, t0);
        assert!(!push(&mut a, 3, 1, m, t0), "a duplicate doesn't complete it");
        assert!(a.take_ready(t0).is_none());
        assert!(push(&mut a, 40, 1, m, t0));
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(1), vec![(3, 1), (40, 1)]));

        // The tiles' masks are merged: one that knows of more tiles keeps the update waiting.
        push(&mut a, 5, 2, mask(&[5]), t0);
        assert!(a.take_ready(t0).is_some_and(|r| r.tiles == vec![(5, 2)]), "{{5}} is complete");
        push(&mut a, 7, 4, mask(&[7, 8]), t0);
        assert!(!push(&mut a, 9, 4, mask(&[9]), t0), "update 4 still waits for tile 8");
        assert!(a.take_ready(t0).is_none());
        assert!(push(&mut a, 8, 4, mask(&[7, 8]), t0));
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(4), vec![(7, 4), (8, 4), (9, 4)]));
    }

    #[test]
    fn incomplete_update_is_shown_after_the_deadline() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        let m = mask(&[3, 4]);
        push(&mut a, 3, 7, m, t0);
        // A full-screen update's tiles come over ~20 ms: still waiting.
        assert!(a.take_ready(t0 + Duration::from_millis(25)).is_none());
        assert!(a.take_ready(t0 + UPDATE_DEADLINE - Duration::from_micros(1)).is_none());
        assert_eq!(sorted(a.take_ready(t0 + UPDATE_DEADLINE).unwrap()), (Some(7), vec![(3, 7)]));
        // Its missing tile shows on its own when it comes, as nothing newer covered it.
        assert!(push(&mut a, 4, 7, m, t0 + UPDATE_DEADLINE * 2));
        assert_eq!(sorted(a.take_ready(t0 + UPDATE_DEADLINE * 2).unwrap()), (None, vec![(4, 7)]));
    }

    #[test]
    fn newer_complete_update_takes_older_pending_tiles_along() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        // Update 5 changed tiles 0 and 1, but only tile 1 came so far; update 6 changed tile 0.
        push(&mut a, 1, 5, mask(&[0, 1]), t0);
        assert!(push(&mut a, 0, 6, mask(&[0]), t0));
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(6), vec![(0, 6), (1, 5)]));
        // Update 5's tile 0 is late and covered by update 6: dropped.
        assert!(!push(&mut a, 0, 5, mask(&[0, 1]), t0));
        assert!(a.take_ready(t0).is_none());
    }

    #[test]
    fn newer_tile_of_the_same_place_wins() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        push(&mut a, 2, 10, mask(&[1, 2]), t0);
        push(&mut a, 2, 11, mask(&[2]), t0);
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(11), vec![(2, 11)]));
    }

    #[test]
    fn newer_incomplete_update_keeps_waiting() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        push(&mut a, 0, 21, mask(&[0, 1]), t0);
        push(&mut a, 0, 20, mask(&[0]), t0);
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(20), vec![(0, 20)]));
        let later = t0 + Duration::from_millis(1);
        assert!(push(&mut a, 1, 21, mask(&[0, 1]), later));
        assert_eq!(sorted(a.take_ready(later).unwrap()), (Some(21), vec![(0, 21), (1, 21)]));
    }

    #[test]
    fn update_numbers_wrap() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        push(&mut a, 0, u32::MAX, mask(&[0]), t0);
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(u32::MAX), vec![(0, u32::MAX)]));
        push(&mut a, 0, 0, mask(&[0]), t0);
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(0), vec![(0, 0)]));
        // u32::MAX - 1 is older than 0.
        assert!(!push(&mut a, 0, u32::MAX - 1, mask(&[0, 1]), t0));
        assert!(push(&mut a, 1, u32::MAX - 1, mask(&[0, 1]), t0), "late, but its place has nothing newer");
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (None, vec![(1, u32::MAX - 1)]));
        // Pending updates stay in order across the wrap too.
        push(&mut a, 0, 2, mask(&[0, 1]), t0);
        push(&mut a, 0, 1, mask(&[0]), t0);
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(1), vec![(0, 1)]));
    }

    #[test]
    fn places_unchanged_for_half_the_update_range_still_change() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        push(&mut a, 0, 0, mask(&[0]), t0);
        // Only tile 1 changes for 3 × 2^30 updates.
        for update in [1 << 30, 2 << 30, 3 << 30] {
            push(&mut a, 1, update, mask(&[1]), t0);
        }
        let _ = a.take_ready(t0);
        assert!(push(&mut a, 0, (3 << 30) + 1, mask(&[0]), t0));
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some((3 << 30) + 1), vec![(0, (3 << 30) + 1)]));
    }

    #[test]
    fn stream_size_change_discards_the_old_size() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        push(&mut a, 0, 1, mask(&[0, 1]), t0);
        push(&mut a, 0, 2, mask(&[0]), t0);
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(2), vec![(0, 2)]));
        push(&mut a, 1, 3, mask(&[0, 1]), t0);
        // The display changed: update 4 is the first of a smaller stream.
        let small = |index: u8| TileKey { tile: rect(index), stream: (1920, 1080), update: 4, update_mask: mask(&[0, 1]) };
        assert!(a.push(small(0), (0, 4), t0));
        // Late tiles of the old size never mix in.
        assert!(!push(&mut a, 1, 3, mask(&[0, 1]), t0));
        assert!(a.take_ready(t0).is_none(), "update 3 was dropped, update 4 is incomplete");
        let release = a.take_ready(t0 + UPDATE_DEADLINE).unwrap();
        assert_eq!(release.stream, (1920, 1080));
        assert_eq!(sorted(release), (Some(4), vec![(0, 4)]));
        // The rest of update 4 is late, but nothing newer covered its place.
        assert!(a.push(small(1), (1, 4), t0));
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (None, vec![(1, 4)]));
    }

    #[test]
    fn out_of_range_tiles_are_ignored() {
        let mut a = Assembler::default();
        let key = TileKey { tile: TileRect { index: MAX_TILES as u8, x: 0, y: 0, width: 1, height: 1 }, stream: STREAM, update: 1, update_mask: 0 };
        assert!(!a.push(key, (0, 1), Instant::now()));
        assert!(a.take_ready(Instant::now()).is_none());
    }

    #[test]
    fn memory_is_bounded_while_nothing_draws() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        let all = mask(&(0..16).collect::<Vec<_>>());
        for update in 1..=1000 {
            for index in 0..16 {
                push(&mut a, index, update, all, t0);
            }
        }
        assert!(held(&a) <= 16, "complete updates: {} images held", held(&a));
        // Updates that never complete, faster than the deadline (the clock stands still here).
        for update in 1001..=2000 {
            push(&mut a, (update % 16) as u8, update, all, t0);
        }
        assert!(a.groups.len() <= MAX_PENDING_UPDATES);
        assert!(held(&a) <= MAX_TILES + MAX_PENDING_UPDATES, "incomplete updates: {} images held", held(&a));
        let release = a.take_ready(t0).unwrap();
        assert_eq!(release.tiles.len(), 16, "the newest image of every tile");
        assert!(release.tiles.iter().all(|(_, update)| *update > 1900), "{:?}", release.tiles);
    }

    #[test]
    fn full_frame_covers_every_tile() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        // Update 1's tile 0 came, tile 1 is late.
        push(&mut a, 0, 1, mask(&[0, 1]), t0);
        assert_eq!(sorted(a.take_ready(t0 + UPDATE_DEADLINE).unwrap()), (Some(1), vec![(0, 1)]));
        // A full frame is complete on its own.
        assert!(push(&mut a, FULL_TILE, 2, 1 << FULL_TILE, t0));
        let release = a.take_ready(t0).unwrap();
        assert_eq!(release.update, Some(2));
        let (full, keep) = release.full.unwrap();
        assert_eq!((full, keep), ((FULL_TILE, 2), vec![]));
        assert!(release.tiles.is_empty());
        // Older tiles never overwrite it, even in places update 2's frame was the first to fill.
        assert!(!push(&mut a, 1, 1, mask(&[0, 1]), t0));
        assert!(!push(&mut a, 5, 1, mask(&[5]), t0));
        // A newer tile goes over it.
        assert!(push(&mut a, 1, 3, mask(&[1]), t0));
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(3), vec![(1, 3)]));
    }

    #[test]
    fn full_frame_drops_older_waiting_tiles_and_goes_first() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        // Nothing draws in between: tiles of 1, the full frame of 2, a tile of 3.
        push(&mut a, 0, 1, mask(&[0, 1]), t0);
        push(&mut a, 1, 1, mask(&[0, 1]), t0);
        push(&mut a, FULL_TILE, 2, 1 << FULL_TILE, t0);
        push(&mut a, 4, 3, mask(&[4]), t0);
        assert_eq!(held(&a), 2);
        let release = a.take_ready(t0).unwrap();
        assert_eq!(release.update, Some(3));
        let (full, keep) = release.full.unwrap();
        assert_eq!(full, (FULL_TILE, 2));
        // Tile 4 is drawn over it anyway; keeping its place just saves the copy.
        assert_eq!(keep, vec![rect(4)]);
        assert_eq!(release.tiles, vec![(4, 3)]);
    }

    #[test]
    fn late_full_frame_keeps_newer_tiles() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        push(&mut a, 2, 10, mask(&[2]), t0);
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(10), vec![(2, 10)]));
        // Update 12's tile decoded before update 11's (a full frame).
        push(&mut a, 5, 12, mask(&[5]), t0);
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(12), vec![(5, 12)]));
        assert!(push(&mut a, FULL_TILE, 11, 1 << FULL_TILE, t0), "late, but newer than most places");
        let release = a.take_ready(t0).unwrap();
        assert_eq!(release.update, None);
        assert_eq!(release.full.unwrap(), ((FULL_TILE, 11), vec![rect(5)]));
        // It shows in tile 2's place now, so update 10's late tiles stay out.
        assert!(!push(&mut a, 2, 10, mask(&[2]), t0));
        // An even older full frame is dropped.
        assert!(!push(&mut a, FULL_TILE, 9, 1 << FULL_TILE, t0));
        assert!(a.take_ready(t0).is_none());
    }

    #[test]
    fn forgotten_tiles_apply_again() {
        let t0 = Instant::now();
        let mut a = Assembler::default();
        push(&mut a, 0, 5, mask(&[0]), t0);
        push(&mut a, 1, 6, mask(&[1]), t0);
        let _ = a.take_ready(t0);
        // Tile 2's image of update 7 waits; then the canvas loses tiles 0 and 2.
        push(&mut a, 2, 7, mask(&[2]), t0);
        a.forget(mask(&[0, 2]));
        assert!(push(&mut a, 0, 4, mask(&[0]), t0), "an old tile 0 is better than nothing");
        assert!(!push(&mut a, 1, 4, mask(&[1]), t0), "tile 1 still shows update 6");
        assert!(!push(&mut a, 2, 6, mask(&[2]), t0), "update 7 still waits for tile 2");
        assert_eq!(sorted(a.take_ready(t0).unwrap()), (Some(7), vec![(0, 4), (2, 7)]));
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(2);
        while !done() {
            assert!(Instant::now() < until, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn attach(shared: &Arc<Shared>, layer: &CAMetalLayer) -> ViewHandle {
        let ptr = std::ptr::from_ref(layer).cast_mut().cast::<c_void>();
        unsafe { ViewHandle::attach(ptr, 64, 32, shared.clone()) }.expect("attach")
    }

    /// Tile `index` (32×32, left or right half) of a 64×32 stream.
    fn image(pb: &CFRetained<CVPixelBuffer>, index: u8, update: u32, update_mask: u64) -> TileImage {
        TileImage {
            pixel_buffer: pb.clone(),
            tile: TileRect { index, x: u32::from(index) * 32, y: 0, width: 32, height: 32 },
            stream: (64, 32),
            update,
            update_mask,
            cover: Vec::new(),
            timing: FrameTiming::default(),
        }
    }

    /// The render thread shows an update on a layer outside any window, lets go of the decoder's
    /// buffers once the GPU is done with them, and keeps the canvas for the next view.
    #[test]
    fn render_thread_shows_tiles_and_releases_buffers() {
        let shared = Arc::new(Shared::default());
        let gpu = render::gpu().expect("Metal");
        let (left, right) = (nv12(32, 32, |_, _| 200, (128, 128)), nv12(32, 32, |x, _| x as u8, (90, 160)));
        let released = |pb: &CFRetained<CVPixelBuffer>| pb.retain_count() == 1;

        let first_layer = CAMetalLayer::new();
        let first = attach(&shared, &first_layer);
        // The update's third tile never comes: shown after the deadline anyway.
        shared.slot.publish_tile(image(&left, 0, 1, 0b111));
        shared.slot.publish_tile(image(&right, 1, 1, 0b111));
        wait_for("tiles shown and released", || released(&left) && released(&right));

        // A second view replaces the first; dropping the first afterwards doesn't stop it.
        let second_layer = CAMetalLayer::new();
        let second = attach(&shared, &second_layer);
        drop(first);
        let blue = nv12(32, 32, |_, _| 60, (220, 110));
        shared.slot.publish_tile(image(&blue, 0, 2, 0b1));
        wait_for("second view shows tiles", || released(&blue));

        // The canvas survived the swap: the tile that didn't change is still there.
        let target = Target::new(gpu, 64, 32);
        target.show(gpu, &shared.slot.renderer.lock().unwrap());
        assert_near(target.pixel(5, 5), expected(60, 220, 110), "changed tile");
        for x in [34, 40, 63] {
            assert_near(target.pixel(x, 20), expected((x - 32) as u8, 90, 160), &format!("unchanged tile at {x}"));
        }
        assert!(!shared.slot.take_canvas_lost());

        drop(second);
        assert_eq!(shared.slot.state.lock().unwrap().active, 0);
        assert_eq!(shared.slot.renderer.lock().unwrap().stream_size(), Some((64, 32)), "canvas kept");
    }

    /// A tile that can't reach the canvas makes the client ask for the picture again, and its
    /// place takes any image again.
    #[test]
    fn lost_tiles_are_reported() {
        let shared = Arc::new(Shared::default());
        let layer = CAMetalLayer::new();
        let view = attach(&shared, &layer);
        let good = nv12(32, 32, |_, _| 100, (128, 128));
        shared.slot.publish_tile(image(&good, 0, 5, 0b11));
        shared.slot.publish_tile(image(&bgra(32, 32), 1, 5, 0b11));
        wait_for("canvas lost", || shared.slot.take_canvas_lost());
        assert!(!shared.slot.take_canvas_lost(), "reported once");
        let state = shared.slot.state.lock().unwrap();
        assert_eq!(state.tiles.applied[0], Some(5));
        assert_eq!(state.tiles.applied[1], None);
        drop(state);
        drop(view);
    }
}
