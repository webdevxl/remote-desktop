//! Scales captured frames down on the CPU, for the host's motion stream: while big changes go on
//! frame after frame, a smaller picture keeps the hardware encoder within a 120 Hz frame (at
//! 6144×2560 a whole frame takes ~16 ms, a half-size one ~5 ms).
//!
//! An area filter (each output sample averages the input it covers), so text stays as legible as
//! the size allows, with NEON doing 16-32 samples at a time over a few threads: 6144×2560 → 2/3
//! takes ~0.4 ms on an M3 Max, → 1/2 ~0.25 ms. VideoToolbox's own scaling (a pixel transfer
//! session, or a session smaller than its input) took ~6 ms for the same frame.

use std::ptr::NonNull;

use anyhow::{Context, Result, ensure};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey,
};

use crate::util::{check, dict};

const PIXEL_FORMAT_NV12_FULL: u32 = u32::from_be_bytes(*b"420f");
/// Threads scaling at once. Past 4-8, memory bandwidth limits it.
const MAX_THREADS: usize = 8;

/// How much smaller the output is, each way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ratio {
    /// 2/3: every 3×3 input samples become 2×2.
    TwoThirds,
    /// 1/2: every 2×2 input samples become one.
    Half,
}

impl Ratio {
    /// Output size for a `width`×`height` input: whole filter groups only, even, so NV12's
    /// chroma lines up. Up to 2 input pixels at the right and bottom edges are left out.
    pub fn output_size(self, width: u32, height: u32) -> (u32, u32) {
        let scale = |v: u32| match self {
            Ratio::TwoThirds => v / 3 * 2,
            Ratio::Half => v / 2,
        };
        (scale(width) & !1, scale(height) & !1)
    }

    /// Fraction of each side kept.
    pub fn factor(self) -> f64 {
        match self {
            Ratio::TwoThirds => 2.0 / 3.0,
            Ratio::Half => 0.5,
        }
    }
}

/// Scales `width`×`height` full-range NV12 frames by a fixed [`Ratio`] into buffers of its own
/// pool (IOSurface-backed, so the encoder reads them as they are).
pub struct Scaler {
    width: u32,
    height: u32,
    ratio: Ratio,
    out: (u32, u32),
    pool: CFRetained<CVPixelBufferPool>,
    threads: usize,
}

// SAFETY: used from one thread at a time (the host's encode thread); CoreVideo pools may be used
// from any thread.
unsafe impl Send for Scaler {}

impl Scaler {
    pub fn new(width: u32, height: u32, ratio: Ratio) -> Result<Self> {
        let out = ratio.output_size(width, height);
        ensure!(out.0 >= 2 && out.1 >= 2, "{width}×{height} is too small to scale by {ratio:?}");
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let threads = cores.saturating_sub(2).clamp(1, MAX_THREADS);
        Ok(Self { width, height, ratio, out, pool: make_pool(out.0, out.1)?, threads })
    }

    /// The size of what [`Scaler::scale`] returns.
    pub fn output_size(&self) -> (u32, u32) {
        self.out
    }

    pub fn ratio(&self) -> Ratio {
        self.ratio
    }

    /// A fresh buffer holding `frame` scaled down, with its color attachments.
    pub fn scale(&self, frame: &CVPixelBuffer) -> Result<CFRetained<CVPixelBuffer>> {
        let format = CVPixelBufferGetPixelFormatType(frame);
        ensure!(
            format == PIXEL_FORMAT_NV12_FULL && CVPixelBufferGetPlaneCount(frame) == 2,
            "expected a full-range NV12 frame, got pixel format {:?}",
            format.to_be_bytes().map(char::from)
        );
        let size = (CVPixelBufferGetWidthOfPlane(frame, 0) as u32, CVPixelBufferGetHeightOfPlane(frame, 0) as u32);
        ensure!(size == (self.width, self.height), "frame is {}×{}, scaler expects {}×{}", size.0, size.1, self.width, self.height);
        let out = new_buffer(&self.pool)?;
        {
            let src = Locked::lock(frame, CVPixelBufferLockFlags::ReadOnly)?;
            let dst = Locked::lock(&out, CVPixelBufferLockFlags::empty())?;
            for plane in 0..2 {
                let (s, d) = (src.planes[plane], dst.planes[plane]);
                // Chroma: one CbCr pair per 2×2 pixels.
                let (bpp, w, h) = if plane == 0 { (1, self.width as usize, self.height as usize) } else { (2, self.width as usize / 2, self.height as usize / 2) };
                let (ow, oh) = if plane == 0 { (self.out.0 as usize, self.out.1 as usize) } else { (self.out.0 as usize / 2, self.out.1 as usize / 2) };
                ensure!(w * bpp <= s.stride && h <= s.rows && ow * bpp <= d.stride && oh <= d.rows, "plane {plane} doesn't fit its buffer");
                scale_plane(s, d, w, h, ow, oh, bpp, self.ratio, self.threads);
            }
        }
        frame.propagate_attachments(&out);
        Ok(out)
    }
}

/// One plane of a locked buffer.
#[derive(Clone, Copy)]
struct Plane {
    base: *mut u8,
    stride: usize,
    rows: usize,
}

// SAFETY: points into a locked buffer; threads read the source and write disjoint rows.
unsafe impl Send for Plane {}
unsafe impl Sync for Plane {}

struct Locked<'a> {
    buffer: &'a CVPixelBuffer,
    flags: CVPixelBufferLockFlags,
    planes: [Plane; 2],
}

impl<'a> Locked<'a> {
    fn lock(buffer: &'a CVPixelBuffer, flags: CVPixelBufferLockFlags) -> Result<Self> {
        ensure!(CVPixelBufferGetPlaneCount(buffer) == 2, "expected a two-plane NV12 buffer");
        // SAFETY: a valid pixel buffer; unlocked with the same flags on drop.
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        ensure!(status == 0, "lock pixel buffer (CVReturn {status})");
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

/// Scales a `w`×`h` plane of `bpp`-byte samples into `ow`×`oh` (as [`Ratio::output_size`] gives,
/// halved for chroma), rows split over `threads`. Output rows past what whole input groups give
/// (chroma of an odd group count) repeat the last one.
#[allow(clippy::too_many_arguments)]
fn scale_plane(src: Plane, dst: Plane, w: usize, h: usize, ow: usize, oh: usize, bpp: usize, ratio: Ratio, threads: usize) {
    // Output rows made from whole input groups, and how many input/output rows a group has.
    let (group_in, group_out) = match ratio {
        Ratio::TwoThirds => (3, 2),
        Ratio::Half => (2, 1),
    };
    let groups = (h / group_in).min(oh.div_ceil(group_out));
    let per = groups.div_ceil(threads.max(1)).max(1);
    // Output samples made per row: what whole input groups give, at most `ow`.
    let row_out = match ratio {
        Ratio::TwoThirds => (w / 3 * 2).min(ow),
        Ratio::Half => (w / 2).min(ow),
    };
    std::thread::scope(|scope| {
        for t in 0..threads.max(1) {
            let first = t * per;
            if first >= groups {
                break;
            }
            let last = ((t + 1) * per).min(groups);
            let work = move || {
                // The whole planes (Send), not just their pointers.
                let (src, dst) = (src, dst);
                // SAFETY (all row accesses): rows are below `h` / `oh` and lengths within the
                // strides (checked by the caller); each thread writes its own output rows.
                let row = |y: usize| unsafe { std::slice::from_raw_parts(src.base.add(y * src.stride), w * bpp) };
                let out_row = |y: usize| unsafe { std::slice::from_raw_parts_mut(dst.base.add(y * dst.stride), ow * bpp) };
                match ratio {
                    Ratio::TwoThirds => {
                        let mut tmp = vec![0u16; row_out * bpp * 3];
                        for g in first..last {
                            for r in 0..3 {
                                h32(row(g * 3 + r), &mut tmp[r * row_out * bpp..(r + 1) * row_out * bpp], bpp);
                            }
                            let (r0, rest) = tmp.split_at(row_out * bpp);
                            let (r1, r2) = rest.split_at(row_out * bpp);
                            for (k, (a, b)) in [(r0, r1), (r2, r1)].into_iter().enumerate() {
                                let y = g * 2 + k;
                                if y < oh {
                                    let o = out_row(y);
                                    v32(a, b, &mut o[..row_out * bpp]);
                                    pad_row(o, row_out, bpp);
                                }
                            }
                        }
                    }
                    Ratio::Half => {
                        for y in first..last {
                            let o = out_row(y);
                            box21(row(2 * y), row(2 * y + 1), &mut o[..row_out * bpp], bpp);
                            pad_row(o, row_out, bpp);
                        }
                    }
                }
            };
            // This thread takes the last share itself.
            if last == groups {
                work();
            } else {
                scope.spawn(move || {
                    // On the input-to-photon path, like the encode thread: kept on a performance core.
                    crate::system::set_thread_interactive();
                    work()
                });
            }
        }
    });
    // Rows no whole group reached repeat the last one made.
    let made = (groups * group_out).min(oh);
    if made > 0 {
        for y in made..oh {
            // SAFETY: both rows are inside the locked output plane and distinct.
            unsafe { std::ptr::copy_nonoverlapping(dst.base.add((made - 1) * dst.stride), dst.base.add(y * dst.stride), ow * bpp) };
        }
    }
}

/// Fills an output row past `made` samples with its last one.
fn pad_row(row: &mut [u8], made: usize, bpp: usize) {
    if made == 0 {
        return;
    }
    let total = row.len() / bpp;
    for s in made..total {
        row.copy_within((made - 1) * bpp..made * bpp, s * bpp);
    }
}

/// 3 → 2 horizontally: for input samples a, b, c (3k, 3k+1, 3k+2) of a row, out[2k] = 2a + b and
/// out[2k+1] = b + 2c (×3 of the average), per byte of a `bpp`-byte sample.
fn h32(row: &[u8], out: &mut [u16], bpp: usize) {
    let n = out.len();
    let mut i = 0;
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        let mut j = 0;
        // SAFETY: each iteration reads 48 input bytes (j + 48 <= row.len()) and writes 32 output
        // lanes (i + 32 <= n).
        unsafe {
            while i + 32 <= n && j + 48 <= row.len() {
                let (a, b, c) = if bpp == 1 {
                    let v = vld3q_u8(row.as_ptr().add(j));
                    (v.0, v.1, v.2)
                } else {
                    // A CbCr pair is one u16: deinterleave pairs, then work per byte.
                    let v = vld3q_u16(row.as_ptr().add(j).cast());
                    (vreinterpretq_u8_u16(v.0), vreinterpretq_u8_u16(v.1), vreinterpretq_u8_u16(v.2))
                };
                let o0l = vaddw_u8(vshll_n_u8(vget_low_u8(a), 1), vget_low_u8(b));
                let o0h = vaddw_u8(vshll_n_u8(vget_high_u8(a), 1), vget_high_u8(b));
                let o1l = vaddw_u8(vshll_n_u8(vget_low_u8(c), 1), vget_low_u8(b));
                let o1h = vaddw_u8(vshll_n_u8(vget_high_u8(c), 1), vget_high_u8(b));
                if bpp == 1 {
                    vst2q_u16(out.as_mut_ptr().add(i), uint16x8x2_t(o0l, o1l));
                    vst2q_u16(out.as_mut_ptr().add(i + 16), uint16x8x2_t(o0h, o1h));
                } else {
                    // Interleave whole pairs (two u16 lanes, one u32).
                    vst2q_u32(out.as_mut_ptr().add(i).cast(), uint32x4x2_t(vreinterpretq_u32_u16(o0l), vreinterpretq_u32_u16(o1l)));
                    vst2q_u32(out.as_mut_ptr().add(i + 16).cast(), uint32x4x2_t(vreinterpretq_u32_u16(o0h), vreinterpretq_u32_u16(o1h)));
                }
                i += 32;
                j += 48;
            }
        }
    }
    while i < n {
        let (s, c) = (i / bpp, i % bpp);
        let k = s / 2;
        let px = |m: usize| u16::from(row[(3 * k + m) * bpp + c]);
        out[i] = if s % 2 == 0 { 2 * px(0) + px(1) } else { px(1) + 2 * px(2) };
        i += 1;
    }
}

/// Vertical 3 → 2 of rows already scaled horizontally: (2x + y) / 9, rounded.
fn v32(x: &[u16], y: &[u16], out: &mut [u8]) {
    let n = out.len();
    let mut i = 0;
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        // SAFETY: each iteration reads and writes 16 lanes, i + 16 <= n <= x.len(), y.len().
        unsafe {
            // (s + 4) * 7282 >> 16 == (s + 4) / 9 for every s up to 9 * 255.
            let k = vdupq_n_u32(7282);
            let four = vdupq_n_u16(4);
            let div9 = |s: uint16x8_t| -> uint16x8_t {
                let lo = vshrq_n_u32(vmulq_u32(vmovl_u16(vget_low_u16(s)), k), 16);
                let hi = vshrq_n_u32(vmulq_u32(vmovl_u16(vget_high_u16(s)), k), 16);
                vcombine_u16(vmovn_u32(lo), vmovn_u32(hi))
            };
            while i + 16 <= n {
                let s0 = vaddq_u16(vaddq_u16(vshlq_n_u16(vld1q_u16(x.as_ptr().add(i)), 1), vld1q_u16(y.as_ptr().add(i))), four);
                let s1 = vaddq_u16(vaddq_u16(vshlq_n_u16(vld1q_u16(x.as_ptr().add(i + 8)), 1), vld1q_u16(y.as_ptr().add(i + 8))), four);
                vst1q_u8(out.as_mut_ptr().add(i), vcombine_u8(vmovn_u16(div9(s0)), vmovn_u16(div9(s1))));
                i += 16;
            }
        }
    }
    while i < n {
        out[i] = ((2 * x[i] + y[i] + 4) / 9) as u8;
        i += 1;
    }
}

/// 2 × 2 → 1 average of rows `a` and `b`, rounded, per byte of a `bpp`-byte sample.
fn box21(a: &[u8], b: &[u8], out: &mut [u8], bpp: usize) {
    let n = out.len();
    let mut i = 0;
    #[cfg(target_arch = "aarch64")]
    {
        use std::arch::aarch64::*;
        // SAFETY: each iteration reads 32 bytes of each row at 2i (2i + 32 <= 2n <= len) and
        // writes 16 output bytes.
        unsafe {
            while i + 16 <= n && 2 * i + 32 <= a.len().min(b.len()) {
                let (lo, hi) = if bpp == 1 {
                    let s = vaddq_u16(vpaddlq_u8(vld1q_u8(a.as_ptr().add(2 * i))), vpaddlq_u8(vld1q_u8(b.as_ptr().add(2 * i))));
                    let t = vaddq_u16(vpaddlq_u8(vld1q_u8(a.as_ptr().add(2 * i + 16))), vpaddlq_u8(vld1q_u8(b.as_ptr().add(2 * i + 16))));
                    (s, t)
                } else {
                    // Even and odd CbCr pairs, added per byte.
                    let va = vld2q_u16(a.as_ptr().add(2 * i).cast());
                    let vb = vld2q_u16(b.as_ptr().add(2 * i).cast());
                    let (a0, a1) = (vreinterpretq_u8_u16(va.0), vreinterpretq_u8_u16(va.1));
                    let (b0, b1) = (vreinterpretq_u8_u16(vb.0), vreinterpretq_u8_u16(vb.1));
                    let lo = vaddq_u16(vaddl_u8(vget_low_u8(a0), vget_low_u8(a1)), vaddl_u8(vget_low_u8(b0), vget_low_u8(b1)));
                    let hi = vaddq_u16(vaddl_u8(vget_high_u8(a0), vget_high_u8(a1)), vaddl_u8(vget_high_u8(b0), vget_high_u8(b1)));
                    (lo, hi)
                };
                vst1q_u8(out.as_mut_ptr().add(i), vcombine_u8(vrshrn_n_u16(lo, 2), vrshrn_n_u16(hi, 2)));
                i += 16;
            }
        }
    }
    while i < n {
        let (s, c) = (i / bpp, i % bpp);
        let p = 2 * s * bpp + c;
        out[i] = ((u16::from(a[p]) + u16::from(a[p + bpp]) + u16::from(b[p]) + u16::from(b[p + bpp]) + 2) / 4) as u8;
        i += 1;
    }
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
    use super::*;

    /// A plane of `w`×`h` samples of `bpp` bytes, rows padded to `stride`.
    fn plane(w: usize, h: usize, bpp: usize, stride: usize, seed: u32) -> Vec<u8> {
        let mut v = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w * bpp {
                let mut r = (x as u32).wrapping_mul(0x9e37_79b9) ^ (y as u32).wrapping_mul(0x85eb_ca6b) ^ seed;
                r ^= r >> 15;
                r = r.wrapping_mul(0x2c1b_3c6d);
                v[y * stride + x] = (r >> 7) as u8;
            }
        }
        v
    }

    /// The area filter written out plainly.
    fn reference(src: &[u8], stride: usize, w: usize, h: usize, bpp: usize, ratio: Ratio, ow: usize, oh: usize) -> Vec<Vec<u8>> {
        let mut out = vec![vec![0u8; ow * bpp]; oh];
        let px = |x: usize, y: usize, c: usize| u32::from(src[y * stride + x * bpp + c]);
        let made_rows = match ratio {
            Ratio::TwoThirds => (h / 3 * 2).min(oh),
            Ratio::Half => (h / 2).min(oh),
        };
        let made_cols = match ratio {
            Ratio::TwoThirds => (w / 3 * 2).min(ow),
            Ratio::Half => (w / 2).min(ow),
        };
        for y in 0..made_rows {
            for x in 0..made_cols {
                for c in 0..bpp {
                    out[y][x * bpp + c] = match ratio {
                        Ratio::Half => ((px(2 * x, 2 * y, c) + px(2 * x + 1, 2 * y, c) + px(2 * x, 2 * y + 1, c) + px(2 * x + 1, 2 * y + 1, c) + 2) / 4) as u8,
                        Ratio::TwoThirds => {
                            // Weights 2,1 / 1,2 each way.
                            let wx = |dx: usize| -> [u32; 3] { if dx == 0 { [2, 1, 0] } else { [0, 1, 2] } };
                            let (gx, gy) = (x / 2 * 3, y / 2 * 3);
                            let (fx, fy) = (wx(x % 2), wx(y % 2));
                            let mut sum = 0;
                            for j in 0..3 {
                                for i in 0..3 {
                                    sum += fx[i] * fy[j] * px(gx + i, gy + j, c);
                                }
                            }
                            ((sum + 4) / 9) as u8
                        }
                    };
                }
            }
            for x in made_cols..ow {
                for c in 0..bpp {
                    out[y][x * bpp + c] = out[y][(made_cols - 1) * bpp + c];
                }
            }
        }
        for y in made_rows..oh {
            out[y] = out[made_rows - 1].clone();
        }
        out
    }

    #[test]
    fn simd_matches_the_plain_filter() {
        for ratio in [Ratio::TwoThirds, Ratio::Half] {
            // Sizes around the SIMD widths, odd group counts and padding; chroma (bpp 2) too.
            for (w, h, bpp) in [(6144 / 2, 41, 2), (6144, 40, 1), (4112, 31, 1), (2056, 17, 2), (99, 9, 1), (51, 7, 2), (6, 6, 1), (3, 3, 2)] {
                let stride = w * bpp + 64;
                let src = plane(w, h, bpp, stride, w as u32 ^ (h as u32) << 8);
                let (mut ow, mut oh) = match ratio {
                    Ratio::TwoThirds => (w / 3 * 2, h / 3 * 2),
                    Ratio::Half => (w / 2, h / 2),
                };
                // Chroma planes may want a row/column more than whole groups give.
                if bpp == 2 {
                    ow += 1;
                    oh += 1;
                }
                if ow == 0 || oh == 0 {
                    continue;
                }
                let dstride = ow * bpp + 32;
                let mut dst = vec![0u8; dstride * oh];
                for threads in [1, 3, 8] {
                    dst.fill(0);
                    let s = Plane { base: src.as_ptr().cast_mut(), stride, rows: h };
                    let d = Plane { base: dst.as_mut_ptr(), stride: dstride, rows: oh };
                    scale_plane(s, d, w, h, ow, oh, bpp, ratio, threads);
                    let want = reference(&src, stride, w, h, bpp, ratio, ow, oh);
                    for (y, row) in want.iter().enumerate() {
                        assert_eq!(&dst[y * dstride..y * dstride + ow * bpp], &row[..], "{ratio:?} {w}x{h} bpp {bpp} threads {threads} row {y}");
                    }
                }
            }
        }
    }

    #[test]
    fn output_sizes_are_even_and_whole_groups() {
        assert_eq!(Ratio::TwoThirds.output_size(6144, 2560), (4096, 1706));
        assert_eq!(Ratio::Half.output_size(6144, 2560), (3072, 1280));
        assert_eq!(Ratio::TwoThirds.output_size(4112, 2658), (2740, 1772));
        assert_eq!(Ratio::Half.output_size(3456, 2234), (1728, 1116));
        assert_eq!(Ratio::Half.output_size(1366, 770), (682, 384));
    }

    #[test]
    fn scales_a_frame() {
        let (w, h) = (640u32, 360u32);
        let pool = make_pool(w, h).unwrap();
        let frame = new_buffer(&pool).unwrap();
        {
            let locked = Locked::lock(&frame, CVPixelBufferLockFlags::empty()).unwrap();
            for (i, p) in locked.planes.iter().enumerate() {
                for y in 0..p.rows {
                    // SAFETY: inside the locked plane.
                    let row = unsafe { std::slice::from_raw_parts_mut(p.base.add(y * p.stride), p.stride) };
                    row.fill(if i == 0 { 200 } else { 100 });
                }
            }
        }
        for ratio in [Ratio::TwoThirds, Ratio::Half] {
            let scaler = Scaler::new(w, h, ratio).unwrap();
            let out = scaler.scale(&frame).unwrap();
            let size = (CVPixelBufferGetWidthOfPlane(&out, 0) as u32, CVPixelBufferGetHeightOfPlane(&out, 0) as u32);
            assert_eq!(size, scaler.output_size());
            let locked = Locked::lock(&out, CVPixelBufferLockFlags::ReadOnly).unwrap();
            for (i, p) in locked.planes.iter().enumerate() {
                let width = CVPixelBufferGetWidthOfPlane(&out, i) * if i == 0 { 1 } else { 2 };
                for y in 0..p.rows {
                    // SAFETY: inside the locked plane.
                    let row = unsafe { std::slice::from_raw_parts(p.base.add(y * p.stride), width) };
                    assert!(row.iter().all(|&v| v == if i == 0 { 200 } else { 100 }), "{ratio:?} plane {i} row {y}");
                }
            }
        }
        assert!(Scaler::new(w, h, Ratio::Half).unwrap().scale(&new_buffer(&make_pool(64, 64).unwrap()).unwrap()).is_err(), "wrong size");
    }
}
