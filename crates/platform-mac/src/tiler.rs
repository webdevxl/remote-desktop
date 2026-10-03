//! Splits captured frames into tiles and finds the tiles that changed, so the host encodes (and
//! sends) only those. Each tile is its own hardware encoder session, so the tiles of one frame
//! encode in parallel and go out one by one as they finish.
//!
//! On the CPU: a tile is compared with one `memcmp` per row against what it last returned,
//! stopping at its first difference, and a changed tile is copied with one `memcpy` per row, tiles
//! spread over a few threads. A GPU pass did the same in 4-9 ms a frame on a live host (p50/p90 at
//! 4112×2658): between frames the GPU clocks down, shares time with WindowServer and takes a
//! while to start. Memory clocks down too, so what costs is the bytes read: ScreenCaptureKit's
//! dirty rectangles say which tiles to read at all (see [`Tiler::changed_in`]).

use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, ensure};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey,
};
use protocol::{MAX_TILES, TileRect};

use crate::util::{check, dict};

const PIXEL_FORMAT_NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
/// Threads comparing and copying tiles at once. Memory bandwidth, not cores, limits this.
const MAX_THREADS: usize = 8;

/// A changed tile's pixels.
pub struct TileCopy {
    /// Index into the tiles passed to [`Tiler::new`].
    pub index: usize,
    /// A fresh NV12 (full range) IOSurface-backed buffer the size of the tile, carrying the
    /// frame's color attachments.
    pub pixel_buffer: CFRetained<CVPixelBuffer>,
}

// SAFETY: CVPixelBuffer is a thread-safe, reference-counted CoreFoundation object.
unsafe impl Send for TileCopy {}

struct Tile {
    rect: TileRect,
    /// Into `Tiler::pools`.
    pool: usize,
    /// What the tile last returned; compared against, and re-encoded for keyframes.
    last: Option<CFRetained<CVPixelBuffer>>,
    /// Counts as changed next time whatever its pixels (see [`Tiler::invalidate`]).
    dirty: bool,
}

pub struct Tiler {
    width: u32,
    height: u32,
    /// One per distinct tile size.
    pools: Vec<CFRetained<CVPixelBufferPool>>,
    tiles: Vec<Tile>,
    threads: usize,
}

// SAFETY: used from one thread at a time (the host's encode thread); CoreVideo buffers and pools
// may be used from any thread.
unsafe impl Send for Tiler {}

impl Tiler {
    /// `width`×`height` is the size of the frames to split; `tiles` cover it exactly (see
    /// [`protocol::tile_layout`]).
    pub fn new(width: u32, height: u32, tiles: &[TileRect]) -> Result<Self> {
        ensure!(!tiles.is_empty() && tiles.len() <= MAX_TILES, "{} tiles (1 to {MAX_TILES} allowed)", tiles.len());
        let mut sizes: Vec<(u32, u32)> = Vec::new();
        let mut pools = Vec::new();
        let mut list = Vec::with_capacity(tiles.len());
        for &rect in tiles {
            let inside = rect.width > 0
                && rect.height > 0
                && u64::from(rect.x) + u64::from(rect.width) <= u64::from(width)
                && u64::from(rect.y) + u64::from(rect.height) <= u64::from(height);
            // Chroma is subsampled 2×2: a tile must start on a whole chroma sample.
            ensure!(inside && rect.x % 2 == 0 && rect.y % 2 == 0, "tile {rect:?} doesn't fit a {width}×{height} frame, or starts on an odd pixel");
            let pool = match sizes.iter().position(|&s| s == (rect.width, rect.height)) {
                Some(pool) => pool,
                None => {
                    pools.push(make_pool(rect.width, rect.height)?);
                    sizes.push((rect.width, rect.height));
                    sizes.len() - 1
                }
            };
            list.push(Tile { rect, pool, last: None, dirty: true });
        }
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let threads = cores.saturating_sub(2).clamp(1, MAX_THREADS).min(list.len());
        Ok(Self { width, height, pools, tiles: list, threads })
    }

    /// Copies the tiles of `frame` whose pixels differ from what this tiler last returned for
    /// them (every tile on the first call, or after [`Tiler::invalidate`]) into fresh buffers,
    /// and remembers those as the tiles' contents.
    pub fn changed(&mut self, frame: &CVPixelBuffer) -> Result<Vec<TileCopy>> {
        self.changed_in(frame, u64::MAX)
    }

    /// Like [`Tiler::changed`], but only looks at the tiles in `candidates` (bit `i` is tile
    /// `i`), e.g. those ScreenCaptureKit's dirty rectangles touch: the others count as unchanged
    /// without reading them. Tiles without content yet, or invalidated, are always looked at.
    pub fn changed_in(&mut self, frame: &CVPixelBuffer, candidates: u64) -> Result<Vec<TileCopy>> {
        let format = CVPixelBufferGetPixelFormatType(frame);
        ensure!(
            format == PIXEL_FORMAT_NV12_FULL && CVPixelBufferGetPlaneCount(frame) == 2,
            "expected a full-range NV12 frame, got pixel format {:?}",
            format.to_be_bytes().map(char::from)
        );
        let size = (CVPixelBufferGetWidthOfPlane(frame, 0), CVPixelBufferGetHeightOfPlane(frame, 0));
        ensure!(
            size == (self.width as usize, self.height as usize),
            "frame is {}×{}, tiler expects {}×{}",
            size.0,
            size.1,
            self.width,
            self.height
        );
        let source = Locked::read(frame)?;
        // The last contents, read-locked for the comparisons (their encoders may be reading them
        // too, which read locks allow).
        let mut lasts = Vec::with_capacity(self.tiles.len());
        let mut look = Vec::with_capacity(self.tiles.len());
        for (i, tile) in self.tiles.iter().enumerate() {
            let fresh = tile.last.is_none() || tile.dirty;
            lasts.push(match tile.last.as_ref().filter(|_| !fresh && candidates & (1u64 << i) != 0) {
                Some(last) => Some(Locked::read(last)?),
                None => None,
            });
            if fresh || candidates & (1u64 << i) != 0 {
                look.push(i);
            }
        }

        let job = Job { tiles: &self.tiles, look: &look, pools: &self.pools, lasts: &lasts, source: &source, next: AtomicUsize::new(0) };
        let threads = self.threads.min(look.len());
        let mut results = Vec::new();
        std::thread::scope(|scope| {
            let helpers: Vec<_> = (1..threads)
                .map(|_| {
                    scope.spawn(|| {
                        crate::system::set_thread_interactive();
                        job.run()
                    })
                })
                .collect();
            // This thread works too.
            results = job.run();
            for helper in helpers {
                results.extend(helper.join().expect("tiler thread panicked"));
            }
        });
        drop(lasts);
        drop(source);

        // All or nothing: a tile whose copy is remembered as its content but never returned would
        // never be sent, and never compare as changed again.
        let fresh = results.into_iter().collect::<Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(fresh.len());
        for (i, Fresh(pixel_buffer)) in fresh {
            // So the encoder tags the tile's color like the frame's (BT.709, full range).
            frame.propagate_attachments(&pixel_buffer);
            out.push(TileCopy { index: i, pixel_buffer: pixel_buffer.clone() });
            let tile = &mut self.tiles[i];
            tile.last = Some(pixel_buffer);
            tile.dirty = false;
        }
        out.sort_unstable_by_key(|c| c.index);
        Ok(out)
    }

    /// What tile `index` last returned, e.g. to re-encode it as a keyframe.
    pub fn last(&self, index: usize) -> Option<CFRetained<CVPixelBuffer>> {
        self.tiles.get(index)?.last.clone()
    }

    /// Makes tile `index` count as changed next time (its last copy never reached the viewer).
    pub fn invalidate(&mut self, index: usize) {
        if let Some(tile) = self.tiles.get_mut(index) {
            tile.dirty = true;
        }
    }
}

/// One `changed` call's work, shared by its threads: each takes the next tile until none is left.
struct Job<'a, 'b> {
    tiles: &'a [Tile],
    /// Indexes of the tiles to look at.
    look: &'a [usize],
    pools: &'a [CFRetained<CVPixelBufferPool>],
    lasts: &'a [Option<Locked<'b>>],
    source: &'a Locked<'b>,
    next: AtomicUsize,
}

// SAFETY: the threads only read the tiles, pools and locked buffers (CoreVideo pools hand out
// buffers to any thread), and each writes only the fresh buffers it makes itself.
unsafe impl Sync for Job<'_, '_> {}

/// A fresh tile copy, handed from the thread that made it.
struct Fresh(CFRetained<CVPixelBuffer>);

// SAFETY: CVPixelBuffer is a thread-safe, reference-counted CoreFoundation object.
unsafe impl Send for Fresh {}

impl Job<'_, '_> {
    /// The changed tiles this thread took, with their fresh copies.
    fn run(&self) -> Vec<Result<(usize, Fresh)>> {
        let mut out = Vec::new();
        loop {
            let Some(&i) = self.look.get(self.next.fetch_add(1, Ordering::Relaxed)) else { return out };
            let tile = &self.tiles[i];
            if self.lasts[i].as_ref().is_some_and(|last| same(self.source, last, tile.rect)) {
                continue;
            }
            out.push(copy_tile(self.source, &self.pools[tile.pool], tile.rect).map(|pb| (i, Fresh(pb))));
        }
    }
}

/// The tiles that `[left, top, right, bottom)` rectangles touch (bit `i` is `tiles[i]`).
pub fn tiles_touched(tiles: &[TileRect], rects: &[[u32; 4]]) -> u64 {
    let mut mask = 0u64;
    for (i, t) in tiles.iter().enumerate().take(64) {
        let (right, bottom) = (t.x.saturating_add(t.width), t.y.saturating_add(t.height));
        if rects.iter().any(|&[l, top, r, b]| l < r && top < b && l < right && r > t.x && top < bottom && b > t.y) {
            mask |= 1u64 << i;
        }
    }
    mask
}

/// One plane of a locked buffer.
#[derive(Clone, Copy)]
struct Plane {
    base: *mut u8,
    stride: usize,
    rows: usize,
}

impl Plane {
    /// Row `y`, bytes `x..x + len`. The caller checks the range is inside the plane.
    ///
    /// # Safety
    /// The buffer must be locked, `y < rows` and `x + len <= stride`.
    unsafe fn row(&self, y: usize, x: usize, len: usize) -> &[u8] {
        debug_assert!(y < self.rows && x + len <= self.stride);
        unsafe { std::slice::from_raw_parts(self.base.add(y * self.stride + x), len) }
    }

    /// # Safety
    /// As [`Plane::row`], and the buffer must be locked for writing by this thread alone.
    #[allow(clippy::mut_from_ref)]
    unsafe fn row_mut(&self, y: usize, x: usize, len: usize) -> &mut [u8] {
        debug_assert!(y < self.rows && x + len <= self.stride);
        unsafe { std::slice::from_raw_parts_mut(self.base.add(y * self.stride + x), len) }
    }
}

/// A pixel buffer locked for CPU access, unlocked on drop.
struct Locked<'a> {
    buffer: &'a CVPixelBuffer,
    flags: CVPixelBufferLockFlags,
    planes: [Plane; 2],
}

// SAFETY: the planes point into memory that stays mapped while the buffer is locked (until drop),
// and threads only read it, or write disjoint buffers.
unsafe impl Send for Locked<'_> {}
unsafe impl Sync for Locked<'_> {}

impl<'a> Locked<'a> {
    fn read(buffer: &'a CVPixelBuffer) -> Result<Self> {
        Self::lock(buffer, CVPixelBufferLockFlags::ReadOnly)
    }

    fn write(buffer: &'a CVPixelBuffer) -> Result<Self> {
        Self::lock(buffer, CVPixelBufferLockFlags::empty())
    }

    fn lock(buffer: &'a CVPixelBuffer, flags: CVPixelBufferLockFlags) -> Result<Self> {
        ensure!(CVPixelBufferGetPlaneCount(buffer) == 2, "expected a two-plane NV12 buffer");
        // SAFETY: a valid pixel buffer; unlocked with the same flags on drop.
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        ensure!(status == 0, "lock pixel buffer (CVReturn {status})");
        // Unlocks on drop, also when a plane below has no memory.
        let mut locked = Self { buffer, flags, planes: [Plane { base: std::ptr::null_mut(), stride: 0, rows: 0 }; 2] };
        for (i, plane) in locked.planes.iter_mut().enumerate() {
            let base = CVPixelBufferGetBaseAddressOfPlane(buffer, i).cast::<u8>();
            ensure!(!base.is_null(), "pixel buffer plane {i} has no memory");
            *plane = Plane { base, stride: CVPixelBufferGetBytesPerRowOfPlane(buffer, i), rows: CVPixelBufferGetHeightOfPlane(buffer, i) };
        }
        Ok(locked)
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: locked in `lock` with these flags.
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, self.flags) };
    }
}

/// The rows of each plane a tile covers, and how many bytes of each row: (plane, first row, rows,
/// first byte, bytes). Chroma is one CbCr pair per 2×2 pixels.
fn spans(rect: TileRect) -> [(usize, usize, usize, usize, usize); 2] {
    let (x, y, w, h) = (rect.x as usize, rect.y as usize, rect.width as usize, rect.height as usize);
    [(0, y, h, x, w), (1, y / 2, h.div_ceil(2), x, w.div_ceil(2) * 2)]
}

/// Whether `rect` of `frame` holds exactly what `last` (a tile-sized buffer) does.
fn same(frame: &Locked<'_>, last: &Locked<'_>, rect: TileRect) -> bool {
    spans(rect).into_iter().all(|(plane, first, rows, x, bytes)| {
        let (src, old) = (frame.planes[plane], last.planes[plane]);
        if first + rows > src.rows || x + bytes > src.stride || rows > old.rows || bytes > old.stride {
            return false; // doesn't fit: count it as changed, the copy reports why
        }
        // SAFETY: ranges checked just above; both buffers are locked.
        (0..rows).all(|r| unsafe { src.row(first + r, x, bytes) == old.row(r, 0, bytes) })
    })
}

/// A fresh buffer from `pool` holding `rect` of `frame`.
fn copy_tile(frame: &Locked<'_>, pool: &CVPixelBufferPool, rect: TileRect) -> Result<CFRetained<CVPixelBuffer>> {
    let pixel_buffer = new_buffer(pool)?;
    {
        let dst = Locked::write(&pixel_buffer)?;
        for (plane, first, rows, x, bytes) in spans(rect) {
            let (src, to) = (frame.planes[plane], dst.planes[plane]);
            ensure!(
                first + rows <= src.rows && x + bytes <= src.stride && rows <= to.rows && bytes <= to.stride,
                "tile {rect:?} plane {plane} doesn't fit the frame or its buffer"
            );
            for r in 0..rows {
                // SAFETY: ranges checked above; `dst` is a fresh buffer only this thread holds.
                unsafe { to.row_mut(r, 0, bytes).copy_from_slice(src.row(first + r, x, bytes)) };
            }
        }
    }
    Ok(pixel_buffer)
}

/// A pool of `width`×`height` full-range NV12 IOSurface buffers the encoder and Metal can read.
fn make_pool(width: u32, height: u32) -> Result<CFRetained<CVPixelBufferPool>> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let format = CFNumber::new_i64(i64::from(PIXEL_FORMAT_NV12_FULL));
    let (w, h) = (CFNumber::new_i64(i64::from(width)), CFNumber::new_i64(i64::from(height)));
    let yes = CFBoolean::new(true);
    // SAFETY: CoreVideo's key constants are valid for the life of the process.
    let attrs = unsafe {
        dict(&[
            (kCVPixelBufferPixelFormatTypeKey, format.as_ref()),
            (kCVPixelBufferWidthKey, w.as_ref()),
            (kCVPixelBufferHeightKey, h.as_ref()),
            (kCVPixelBufferIOSurfacePropertiesKey, empty.as_ref()),
            (kCVPixelBufferMetalCompatibilityKey, yes.as_ref()),
        ])
    };
    let mut raw: *mut CVPixelBufferPool = std::ptr::null_mut();
    // SAFETY: `raw` receives a +1 reference on success.
    check(unsafe { CVPixelBufferPool::create(None, None, Some(attrs.as_opaque()), NonNull::from(&mut raw)) }, "CVPixelBufferPoolCreate")?;
    // SAFETY: created above with a +1 reference.
    Ok(unsafe { CFRetained::from_raw(NonNull::new(raw).context("null pixel buffer pool")?) })
}

fn new_buffer(pool: &CVPixelBufferPool) -> Result<CFRetained<CVPixelBuffer>> {
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: `raw` receives a +1 reference on success.
    check(unsafe { CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut raw)) }, "CVPixelBufferPoolCreatePixelBuffer")?;
    // SAFETY: created above with a +1 reference.
    Ok(unsafe { CFRetained::from_raw(NonNull::new(raw).context("null pixel buffer")?) })
}

#[cfg(test)]
mod tests {
    use super::{spans, tiles_touched};
    use protocol::{TileRect, tile_layout};

    #[test]
    fn rectangles_touch_the_tiles_they_overlap() {
        let tiles = tile_layout(640, 640, Some((2, 2))); // 320×320 each
        assert_eq!(tiles_touched(&tiles, &[]), 0);
        assert_eq!(tiles_touched(&tiles, &[[0, 0, 640, 640]]), 0b1111);
        assert_eq!(tiles_touched(&tiles, &[[319, 0, 321, 1]]), 0b0011, "straddles the column edge");
        assert_eq!(tiles_touched(&tiles, &[[320, 320, 321, 321]]), 0b1000);
        assert_eq!(tiles_touched(&tiles, &[[0, 0, 320, 320]]), 0b0001, "edges are exclusive");
        assert_eq!(tiles_touched(&tiles, &[[5, 5, 5, 9]]), 0, "empty");
        assert_eq!(tiles_touched(&tiles, &[[0, 400, 10, 410], [600, 10, 610, 20]]), 0b0110);
    }

    #[test]
    fn chroma_spans_cover_odd_sizes() {
        let t = TileRect { index: 0, x: 64, y: 320, width: 999, height: 651 };
        assert_eq!(spans(t), [(0, 320, 651, 64, 999), (1, 160, 326, 64, 1000)]);
    }
}
