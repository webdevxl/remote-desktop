//! Benchmark of how the media engine shares work between encoder sessions: one encoder for the
//! whole picture against the picture split into 2 or 4 parts encoded at once, each with 1 or 2
//! frames in flight. Tells whether more sessions use more of the chip (an M3 Max has two video
//! encode engines) or just take turns.
//!
//!   cargo test --release -p platform-mac --test engines -- --ignored --nocapture
//!
//! `LANKVM_BENCH_SIZE=WIDTHxHEIGHT` picks the frame size (default 6144x2560).

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferIOSurfacePropertiesKey,
};
use platform_mac::encoder::{Encoder, EncoderConfig};
use protocol::Codec;

fn size() -> (usize, usize) {
    std::env::var("LANKVM_BENCH_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((6144, 2560))
}

/// Screen-like content (text-sized high-contrast blocks with some noise), different per seed.
fn frame(w: usize, h: usize, seed: u32) -> CFRetained<CVPixelBuffer> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let attrs: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[unsafe { kCVPixelBufferIOSurfacePropertiesKey }], &[empty.as_ref()]);
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(None, w, h, u32::from_be_bytes(*b"420f"), Some(attrs.as_opaque()), NonNull::from(&mut raw))
    };
    assert_eq!(status, 0);
    let pb = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };
    unsafe {
        CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
        for plane in 0..2 {
            let base = CVPixelBufferGetBaseAddressOfPlane(&pb, plane) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, plane);
            let rows = CVPixelBufferGetHeightOfPlane(&pb, plane);
            for y in 0..rows {
                let row = std::slice::from_raw_parts_mut(base.add(y * stride), stride);
                for (x, px) in row.iter_mut().enumerate() {
                    if plane == 1 {
                        *px = 128;
                        continue;
                    }
                    let mut r = (x as u32).wrapping_mul(0x9e37_79b9) ^ (y as u32).wrapping_mul(0x85eb_ca6b) ^ seed.wrapping_mul(0xc2b2_ae35);
                    r ^= r >> 15;
                    r = r.wrapping_mul(0x2c1b_3c6d);
                    r ^= r >> 12;
                    let cell = ((x / 6) as u32 + seed).wrapping_mul(2_654_435_761) ^ ((y / 12) as u32).wrapping_mul(40_503);
                    *px = if r & 63 == 0 { (r >> 8) as u8 } else if (cell >> 13) & 3 == 0 { 20 } else { 235 };
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
    }
    pb
}

/// The stream's bit budget, as the host sets it (60 fps worth).
fn budget(w: usize, h: usize) -> f64 {
    (w as f64 * h as f64 * 60.0 * 0.12).clamp(8e6, 150e6)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

fn p90(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v.get(v.len() * 9 / 10).copied().unwrap_or(0.0)
}

/// Encodes `w`×`h` frames split into `parts` horizontal bands, one encoder each, with up to
/// `depth` frames in flight per encoder, for `frames` frames. Prints the latency of a frame (all
/// its bands out) and the rate frames come out at.
fn run(w: usize, h: usize, parts: usize, depth: usize, frames: usize) {
    let band = (h / parts) & !1;
    let (tx, rx) = mpsc::channel::<(usize, u64, Instant)>();
    let encoders: Vec<Encoder> = (0..parts)
        .map(|p| {
            let tx = tx.clone();
            let height = if p + 1 == parts { h - band * (parts - 1) } else { band };
            let cfg = EncoderConfig {
                width: w as u32,
                height: height as u32,
                fps: 120,
                bitrate_bps: (budget(w, h) / parts as f64) as u32,
                codec: Codec::Hevc,
            };
            let enc = Encoder::new(&cfg, move |f| {
                let _ = tx.send((f.data.len(), f.tag, Instant::now()));
            })
            .expect("encoder");
            assert!(enc.hardware() && enc.codec() == Codec::Hevc, "no HEVC hardware encoder");
            enc
        })
        .collect();
    // Each band's own pixel buffers, so no session reads another's.
    let seeds = 4;
    let inputs: Vec<Vec<CFRetained<CVPixelBuffer>>> = (0..parts)
        .map(|p| {
            let height = if p + 1 == parts { h - band * (parts - 1) } else { band };
            (0..seeds).map(|s| frame(w, height, s as u32 * 7 + p as u32)).collect()
        })
        .collect();
    let warmup = 10;
    let mut submitted_at: Vec<Option<Instant>> = vec![None; frames];
    let mut remaining: Vec<usize> = vec![parts; frames];
    let mut done_at: Vec<Option<Instant>> = vec![None; frames];
    let mut bytes = 0usize;
    let mut next = 0usize;
    let mut finished = 0usize;
    let submit = |i: usize, submitted_at: &mut Vec<Option<Instant>>| {
        submitted_at[i] = Some(Instant::now());
        for (p, enc) in encoders.iter().enumerate() {
            enc.encode(&inputs[p][i % seeds], platform_mac::clock::now_us(), false, i as u64).expect("encode");
        }
    };
    while next < depth.min(frames) {
        submit(next, &mut submitted_at);
        next += 1;
    }
    while finished < frames {
        let (len, tag, at) = rx.recv_timeout(Duration::from_secs(5)).expect("encoded frame");
        let i = tag as usize;
        remaining[i] -= 1;
        if i >= warmup {
            bytes += len;
        }
        if remaining[i] == 0 {
            done_at[i] = Some(at);
            finished += 1;
            if next < frames {
                submit(next, &mut submitted_at);
                next += 1;
            }
        }
    }
    let lat: Vec<f64> = (warmup..frames).map(|i| (done_at[i].unwrap() - submitted_at[i].unwrap()).as_secs_f64() * 1000.0).collect();
    let span = (done_at[frames - 1].unwrap() - done_at[warmup].unwrap()).as_secs_f64();
    let fps = (frames - 1 - warmup) as f64 / span;
    println!(
        "{w}x{h} in {parts} band(s) of {w}x{band}, {depth} in flight: frame out after {:.1} ms (p90 {:.1})  {:.0} frames/s  {:.0} KB/frame",
        median(lat.clone()),
        p90(lat),
        fps,
        bytes as f64 / (frames - warmup) as f64 / 1024.0,
    );
}

/// One encoder of `ew`×`eh` fed `w`×`h` frames (VideoToolbox scales them), 1 in flight.
fn run_scaled(w: usize, h: usize, ew: usize, eh: usize, frames: usize) {
    let (tx, rx) = mpsc::channel::<(usize, Instant)>();
    let cfg = EncoderConfig { width: ew as u32, height: eh as u32, fps: 120, bitrate_bps: budget(ew, eh) as u32, codec: Codec::Hevc };
    let enc = Encoder::new(&cfg, move |f| {
        let _ = tx.send((f.data.len(), Instant::now()));
    })
    .expect("encoder");
    assert!(enc.hardware() && enc.codec() == Codec::Hevc, "no HEVC hardware encoder");
    let inputs: Vec<_> = (0..4).map(|s| frame(w, h, s)).collect();
    let (mut lat, mut bytes) = (Vec::new(), 0);
    let t_start = Instant::now();
    let mut t_warm = t_start;
    for i in 0..frames {
        let t0 = Instant::now();
        if i == 10 {
            t_warm = t0;
        }
        enc.encode(&inputs[i % 4], platform_mac::clock::now_us(), false, i as u64).expect("encode");
        let (len, at) = rx.recv_timeout(Duration::from_secs(5)).expect("encoded frame");
        if i >= 10 {
            lat.push((at - t0).as_secs_f64() * 1000.0);
            bytes += len;
        }
    }
    let fps = (frames - 10) as f64 / t_warm.elapsed().as_secs_f64();
    println!(
        "{w}x{h} scaled to {ew}x{eh} by the encoder, 1 in flight: frame out after {:.1} ms (p90 {:.1})  {:.0} frames/s  {:.0} KB/frame",
        median(lat.clone()),
        p90(lat),
        fps,
        bytes as f64 / (frames - 10) as f64 / 1024.0
    );
}

#[test]
#[ignore = "benchmark"]
fn scaled() {
    let (w, h) = size();
    for (num, den) in [(1, 1), (3, 4), (2, 3), (1, 2)] {
        let (ew, eh) = ((w * num / den) & !1, (h * num / den) & !1);
        run_scaled(w, h, ew, eh, 90);
    }
}

#[test]
#[ignore = "benchmark"]
fn engines() {
    let (w, h) = size();
    for (parts, depth) in [(1, 1), (1, 2), (1, 3), (2, 1), (2, 2), (4, 1), (4, 2), (8, 1), (1, 1)] {
        run(w, h, parts, depth, 90);
    }
}

/// An empty IOSurface-backed NV12 buffer.
fn empty(w: usize, h: usize) -> CFRetained<CVPixelBuffer> {
    let none: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let attrs: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[unsafe { kCVPixelBufferIOSurfacePropertiesKey }], &[none.as_ref()]);
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    let status = unsafe { CVPixelBufferCreate(None, w, h, u32::from_be_bytes(*b"420f"), Some(attrs.as_opaque()), NonNull::from(&mut raw)) };
    assert_eq!(status, 0);
    unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) }
}

/// Scaling a frame with a VTPixelTransferSession, then encoding it at that size.
#[test]
#[ignore = "benchmark"]
fn transfer() {
    use objc2_video_toolbox::VTPixelTransferSession;
    let (w, h) = size();
    let inputs: Vec<_> = (0..4).map(|s| frame(w, h, s)).collect();
    let mut raw: *mut VTPixelTransferSession = std::ptr::null_mut();
    assert_eq!(unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut raw)) }, 0);
    let session = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };
    if std::env::var("LANKVM_BENCH_REALTIME").is_ok() {
        use objc2_core_foundation::CFBoolean;
        use objc2_video_toolbox::{VTSession, VTSessionSetProperty, kVTPixelTransferPropertyKey_DownsamplingMode, kVTPixelTransferPropertyKey_RealTime};
        let vt: &VTSession = unsafe { &*(&*session as *const VTPixelTransferSession as *const VTSession) };
        let status = unsafe { VTSessionSetProperty(vt, kVTPixelTransferPropertyKey_RealTime, Some(CFBoolean::new(true).as_ref())) };
        println!("RealTime: {status}");
        if let Ok(mode) = std::env::var("LANKVM_BENCH_DOWNSAMPLING") {
            let value = CFString::from_str(&mode);
            let status = unsafe { VTSessionSetProperty(vt, kVTPixelTransferPropertyKey_DownsamplingMode, Some(value.as_ref())) };
            println!("DownsamplingMode {mode}: {status}");
        }
    }
    for (num, den) in [(3, 4), (2, 3), (1, 2)] {
        let (ew, eh) = ((w * num / den) & !1, (h * num / den) & !1);
        let outs: Vec<_> = (0..4).map(|_| empty(ew, eh)).collect();
        let (tx, rx) = mpsc::channel::<Instant>();
        let cfg = EncoderConfig { width: ew as u32, height: eh as u32, fps: 120, bitrate_bps: budget(ew, eh) as u32, codec: Codec::Hevc };
        let enc = Encoder::new(&cfg, move |_| {
            let _ = tx.send(Instant::now());
        })
        .expect("encoder");
        let (mut scale_ms, mut total_ms) = (Vec::new(), Vec::new());
        for i in 0..70 {
            let t0 = Instant::now();
            assert_eq!(unsafe { session.transfer_image(&inputs[i % 4], &outs[i % 4]) }, 0);
            let scaled = t0.elapsed();
            enc.encode(&outs[i % 4], platform_mac::clock::now_us(), false, i as u64).expect("encode");
            let at = rx.recv_timeout(Duration::from_secs(5)).expect("encoded");
            if i >= 10 {
                scale_ms.push(scaled.as_secs_f64() * 1000.0);
                total_ms.push((at - t0).as_secs_f64() * 1000.0);
            }
        }
        println!("{w}x{h} -> {ew}x{eh}: pixel transfer {:.2} ms (p90 {:.2}), transfer + encode {:.1} ms (p90 {:.1})", median(scale_ms.clone()), p90(scale_ms), median(total_ms.clone()), p90(total_ms));
    }
    unsafe { session.invalidate() };
}

/// Area-filtered 3:2 downscale of one 8-bit plane with `bpp` interleaved channels (1 luma,
/// 2 chroma), rows split over threads. Output size is floor(2/3) of the input.
fn scale_3_2(src: *const u8, src_stride: usize, w: usize, h: usize, dst: *mut u8, dst_stride: usize, bpp: usize, threads: usize) {
    let (ow, oh) = (w / 3 * 2, h / 3 * 2);
    let groups = oh / 2;
    let per = groups.div_ceil(threads);
    let (src, dst) = (src as usize, dst as usize);
    std::thread::scope(|s| {
        for t in 0..threads {
            s.spawn(move || {
                let (src, dst) = (src as *const u8, dst as *mut u8);
                let mut tmp = vec![0u16; ow * bpp * 3];
                for g in (t * per)..((t + 1) * per).min(groups) {
                    // Horizontal 3 -> 2 for the three input rows.
                    for r in 0..3 {
                        let row = unsafe { std::slice::from_raw_parts(src.add((g * 3 + r) * src_stride), w * bpp) };
                        let out = &mut tmp[r * ow * bpp..(r + 1) * ow * bpp];
                        for k in 0..ow / 2 {
                            for c in 0..bpp {
                                let a = row[(3 * k) * bpp + c] as u16;
                                let b = row[(3 * k + 1) * bpp + c] as u16;
                                let d = row[(3 * k + 2) * bpp + c] as u16;
                                out[(2 * k) * bpp + c] = 2 * a + b;
                                out[(2 * k + 1) * bpp + c] = b + 2 * d;
                            }
                        }
                    }
                    // Vertical 3 -> 2.
                    let (r0, r1, r2) = (&tmp[0..ow * bpp], &tmp[ow * bpp..2 * ow * bpp], &tmp[2 * ow * bpp..3 * ow * bpp]);
                    let o0 = unsafe { std::slice::from_raw_parts_mut(dst.add((g * 2) * dst_stride), ow * bpp) };
                    for i in 0..ow * bpp {
                        o0[i] = ((2 * r0[i] + r1[i] + 4) / 9) as u8;
                    }
                    let o1 = unsafe { std::slice::from_raw_parts_mut(dst.add((g * 2 + 1) * dst_stride), ow * bpp) };
                    for i in 0..ow * bpp {
                        o1[i] = ((r1[i] + 2 * r2[i] + 4) / 9) as u8;
                    }
                }
            });
        }
    });
}

/// 2:1 box downscale of one plane.
fn scale_2_1(src: *const u8, src_stride: usize, w: usize, h: usize, dst: *mut u8, dst_stride: usize, bpp: usize, threads: usize) {
    let (ow, oh) = (w / 2, h / 2);
    let per = oh.div_ceil(threads);
    let (src, dst) = (src as usize, dst as usize);
    std::thread::scope(|s| {
        for t in 0..threads {
            s.spawn(move || {
                let (src, dst) = (src as *const u8, dst as *mut u8);
                for y in (t * per)..((t + 1) * per).min(oh) {
                    let a = unsafe { std::slice::from_raw_parts(src.add(2 * y * src_stride), w * bpp) };
                    let b = unsafe { std::slice::from_raw_parts(src.add((2 * y + 1) * src_stride), w * bpp) };
                    let o = unsafe { std::slice::from_raw_parts_mut(dst.add(y * dst_stride), ow * bpp) };
                    for x in 0..ow {
                        for c in 0..bpp {
                            let i = 2 * x * bpp + c;
                            o[x * bpp + c] = ((a[i] as u16 + a[i + bpp] as u16 + b[i] as u16 + b[i + bpp] as u16 + 2) / 4) as u8;
                        }
                    }
                }
            });
        }
    });
}

#[test]
#[ignore = "benchmark"]
fn cpu_scale() {
    let (w, h) = size();
    let input = frame(w, h, 1);
    for (name, num, den) in [("3:2", 2, 3), ("2:1", 1, 2)] {
        let (ow, oh) = (w * num / den, h * num / den);
        let out = empty(ow & !1, oh & !1);
        for threads in [4, 8] {
            let mut times = Vec::new();
            for _ in 0..40 {
                let t0 = Instant::now();
                unsafe {
                    CVPixelBufferLockBaseAddress(&input, CVPixelBufferLockFlags::ReadOnly);
                    CVPixelBufferLockBaseAddress(&out, CVPixelBufferLockFlags::empty());
                    for plane in 0..2 {
                        let (bpp, pw, ph) = if plane == 0 { (1, w, h) } else { (2, w / 2, h / 2) };
                        let src = CVPixelBufferGetBaseAddressOfPlane(&input, plane) as *const u8;
                        let ss = CVPixelBufferGetBytesPerRowOfPlane(&input, plane);
                        let dst = CVPixelBufferGetBaseAddressOfPlane(&out, plane) as *mut u8;
                        let ds = CVPixelBufferGetBytesPerRowOfPlane(&out, plane);
                        if num == 2 {
                            scale_3_2(src, ss, pw, ph, dst, ds, bpp, threads);
                        } else {
                            scale_2_1(src, ss, pw, ph, dst, ds, bpp, threads);
                        }
                    }
                    CVPixelBufferUnlockBaseAddress(&out, CVPixelBufferLockFlags::empty());
                    CVPixelBufferUnlockBaseAddress(&input, CVPixelBufferLockFlags::ReadOnly);
                }
                times.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
            println!("{w}x{h} -> {ow}x{oh} ({name}) on the CPU, {threads} threads: {:.2} ms (p90 {:.2})", median(times.clone()), p90(times));
        }
    }
}

mod neon {
    use std::arch::aarch64::*;

    /// 3 -> 2 horizontally of `n_out` output samples (`n_out` even) with `bpp` bytes per sample:
    /// out[2k] = 2·a + b, out[2k+1] = b + 2·d for input samples a, b, d = 3k, 3k+1, 3k+2.
    pub unsafe fn h32(row: &[u8], out: &mut [u16], bpp: usize) {
        let n = out.len(); // output bytes-lanes (samples × bpp)
        let mut i = 0; // output lane
        let mut j = 0; // input byte
        unsafe {
            if bpp == 1 {
                while i + 32 <= n && j + 48 <= row.len() {
                    let v = vld3q_u8(row.as_ptr().add(j));
                    let (a, b, d) = (v.0, v.1, v.2);
                    let o0l = vaddw_u8(vshll_n_u8(vget_low_u8(a), 1), vget_low_u8(b));
                    let o0h = vaddw_u8(vshll_n_u8(vget_high_u8(a), 1), vget_high_u8(b));
                    let o1l = vaddw_u8(vshll_n_u8(vget_low_u8(d), 1), vget_low_u8(b));
                    let o1h = vaddw_u8(vshll_n_u8(vget_high_u8(d), 1), vget_high_u8(b));
                    vst2q_u16(out.as_mut_ptr().add(i), uint16x8x2_t(o0l, o1l));
                    vst2q_u16(out.as_mut_ptr().add(i + 16), uint16x8x2_t(o0h, o1h));
                    i += 32;
                    j += 48;
                }
            } else {
                while i + 32 <= n && j + 48 <= row.len() {
                    let v = vld3q_u16(row.as_ptr().add(j).cast());
                    let (a, b, d) = (vreinterpretq_u8_u16(v.0), vreinterpretq_u8_u16(v.1), vreinterpretq_u8_u16(v.2));
                    let o0l = vaddw_u8(vshll_n_u8(vget_low_u8(a), 1), vget_low_u8(b));
                    let o0h = vaddw_u8(vshll_n_u8(vget_high_u8(a), 1), vget_high_u8(b));
                    let o1l = vaddw_u8(vshll_n_u8(vget_low_u8(d), 1), vget_low_u8(b));
                    let o1h = vaddw_u8(vshll_n_u8(vget_high_u8(d), 1), vget_high_u8(b));
                    // Each CbCr pair is one u32 of the u16 lanes: interleave pairs.
                    vst2q_u32(out.as_mut_ptr().add(i).cast(), uint32x4x2_t(vreinterpretq_u32_u16(o0l), vreinterpretq_u32_u16(o1l)));
                    vst2q_u32(out.as_mut_ptr().add(i + 16).cast(), uint32x4x2_t(vreinterpretq_u32_u16(o0h), vreinterpretq_u32_u16(o1h)));
                    i += 32;
                    j += 48;
                }
            }
        }
        // Tail.
        while i < n {
            let s = i / bpp;
            let c = i % bpp;
            let k = s / 2;
            let a = row[(3 * k) * bpp + c] as u16;
            let b = row[(3 * k + 1) * bpp + c] as u16;
            let d = row[(3 * k + 2) * bpp + c] as u16;
            out[i] = if s % 2 == 0 { 2 * a + b } else { b + 2 * d };
            i += 1;
        }
    }

    /// (2·x + y + 4) / 9 per lane.
    pub unsafe fn v32(x: &[u16], y: &[u16], out: &mut [u8]) {
        let n = out.len();
        let mut i = 0;
        unsafe {
            let k = vdupq_n_u32(7282);
            let four = vdupq_n_u16(4);
            while i + 16 <= n {
                let s0 = vaddq_u16(vaddq_u16(vshlq_n_u16(vld1q_u16(x.as_ptr().add(i)), 1), vld1q_u16(y.as_ptr().add(i))), four);
                let s1 = vaddq_u16(vaddq_u16(vshlq_n_u16(vld1q_u16(x.as_ptr().add(i + 8)), 1), vld1q_u16(y.as_ptr().add(i + 8))), four);
                let d = |s: uint16x8_t| -> uint16x8_t {
                    let lo = vshrq_n_u32(vmulq_u32(vmovl_u16(vget_low_u16(s)), k), 16);
                    let hi = vshrq_n_u32(vmulq_u32(vmovl_u16(vget_high_u16(s)), k), 16);
                    vcombine_u16(vmovn_u32(lo), vmovn_u32(hi))
                };
                vst1q_u8(out.as_mut_ptr().add(i), vcombine_u8(vmovn_u16(d(s0)), vmovn_u16(d(s1))));
                i += 16;
            }
        }
        while i < n {
            out[i] = ((2 * x[i] + y[i] + 4) / 9) as u8;
            i += 1;
        }
    }

    /// 2:1 box of two rows, `bpp` bytes per sample.
    pub unsafe fn box21(a: &[u8], b: &[u8], out: &mut [u8], bpp: usize) {
        let n = out.len();
        let mut i = 0;
        unsafe {
            if bpp == 1 {
                while i + 16 <= n {
                    let s = vaddq_u16(vpaddlq_u8(vld1q_u8(a.as_ptr().add(2 * i))), vpaddlq_u8(vld1q_u8(b.as_ptr().add(2 * i))));
                    let t = vaddq_u16(vpaddlq_u8(vld1q_u8(a.as_ptr().add(2 * i + 16))), vpaddlq_u8(vld1q_u8(b.as_ptr().add(2 * i + 16))));
                    vst1q_u8(out.as_mut_ptr().add(i), vcombine_u8(vrshrn_n_u16(s, 2), vrshrn_n_u16(t, 2)));
                    i += 16;
                }
            } else {
                while i + 16 <= n {
                    // Pairs: even and odd CbCr samples.
                    let va = vld2q_u16(a.as_ptr().add(2 * i).cast());
                    let vb = vld2q_u16(b.as_ptr().add(2 * i).cast());
                    let (a0, a1) = (vreinterpretq_u8_u16(va.0), vreinterpretq_u8_u16(va.1));
                    let (b0, b1) = (vreinterpretq_u8_u16(vb.0), vreinterpretq_u8_u16(vb.1));
                    let lo = vaddq_u16(vaddl_u8(vget_low_u8(a0), vget_low_u8(a1)), vaddl_u8(vget_low_u8(b0), vget_low_u8(b1)));
                    let hi = vaddq_u16(vaddl_u8(vget_high_u8(a0), vget_high_u8(a1)), vaddl_u8(vget_high_u8(b0), vget_high_u8(b1)));
                    vst1q_u8(out.as_mut_ptr().add(i), vcombine_u8(vrshrn_n_u16(lo, 2), vrshrn_n_u16(hi, 2)));
                    i += 16;
                }
            }
        }
        while i < n {
            let (s, c) = (i / bpp, i % bpp);
            let p = 2 * s * bpp + c;
            out[i] = ((a[p] as u16 + a[p + bpp] as u16 + b[p] as u16 + b[p + bpp] as u16 + 2) / 4) as u8;
            i += 1;
        }
    }
}

fn neon_plane(src: *const u8, ss: usize, w: usize, h: usize, dst: *mut u8, ds: usize, bpp: usize, threads: usize, ratio32: bool) {
    let (src, dst) = (src as usize, dst as usize);
    std::thread::scope(|s| {
        for t in 0..threads {
            s.spawn(move || {
                let (src, dst) = (src as *const u8, dst as *mut u8);
                let row = |y: usize| unsafe { std::slice::from_raw_parts(src.add(y * ss), w * bpp) };
                if ratio32 {
                    let ow = w / 3 * 2;
                    let groups = h / 3;
                    let per = groups.div_ceil(threads);
                    let mut tmp = vec![0u16; ow * bpp * 3];
                    for g in (t * per)..((t + 1) * per).min(groups) {
                        for r in 0..3 {
                            unsafe { neon::h32(row(g * 3 + r), &mut tmp[r * ow * bpp..(r + 1) * ow * bpp], bpp) };
                        }
                        let (r0, rest) = tmp.split_at(ow * bpp);
                        let (r1, r2) = rest.split_at(ow * bpp);
                        let o0 = unsafe { std::slice::from_raw_parts_mut(dst.add(2 * g * ds), ow * bpp) };
                        unsafe { neon::v32(r0, r1, o0) };
                        let o1 = unsafe { std::slice::from_raw_parts_mut(dst.add((2 * g + 1) * ds), ow * bpp) };
                        unsafe { neon::v32(r2, r1, o1) };
                    }
                } else {
                    let (ow, oh) = (w / 2, h / 2);
                    let per = oh.div_ceil(threads);
                    for y in (t * per)..((t + 1) * per).min(oh) {
                        let o = unsafe { std::slice::from_raw_parts_mut(dst.add(y * ds), ow * bpp) };
                        unsafe { neon::box21(row(2 * y), row(2 * y + 1), o, bpp) };
                    }
                }
            });
        }
    });
}

#[test]
#[ignore = "benchmark"]
fn neon_scale() {
    let (w, h) = size();
    let input = frame(w, h, 1);
    for (name, ratio32) in [("3:2", true), ("2:1", false)] {
        let (ow, oh) = if ratio32 { (w / 3 * 2, h / 3 * 2) } else { (w / 2, h / 2) };
        let out = empty(ow & !1, oh & !1);
        let reference = empty(ow & !1, oh & !1);
        for threads in [1, 2, 4, 8] {
            let mut times = Vec::new();
            for _ in 0..40 {
                let t0 = Instant::now();
                unsafe {
                    CVPixelBufferLockBaseAddress(&input, CVPixelBufferLockFlags::ReadOnly);
                    CVPixelBufferLockBaseAddress(&out, CVPixelBufferLockFlags::empty());
                    for plane in 0..2 {
                        let (bpp, pw, ph) = if plane == 0 { (1, w, h) } else { (2, w / 2, h / 2) };
                        let src = CVPixelBufferGetBaseAddressOfPlane(&input, plane) as *const u8;
                        let ss = CVPixelBufferGetBytesPerRowOfPlane(&input, plane);
                        let dst = CVPixelBufferGetBaseAddressOfPlane(&out, plane) as *mut u8;
                        let ds = CVPixelBufferGetBytesPerRowOfPlane(&out, plane);
                        neon_plane(src, ss, pw, ph, dst, ds, bpp, threads, ratio32);
                    }
                    CVPixelBufferUnlockBaseAddress(&out, CVPixelBufferLockFlags::empty());
                    CVPixelBufferUnlockBaseAddress(&input, CVPixelBufferLockFlags::ReadOnly);
                }
                times.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
            println!("{w}x{h} -> {ow}x{oh} ({name}) NEON, {threads} threads: {:.2} ms (p90 {:.2})", median(times.clone()), p90(times));
        }
        // Check against the scalar version.
        unsafe {
            CVPixelBufferLockBaseAddress(&input, CVPixelBufferLockFlags::ReadOnly);
            CVPixelBufferLockBaseAddress(&reference, CVPixelBufferLockFlags::empty());
            CVPixelBufferLockBaseAddress(&out, CVPixelBufferLockFlags::ReadOnly);
            let mut bad = 0usize;
            for plane in 0..2 {
                let (bpp, pw, ph) = if plane == 0 { (1, w, h) } else { (2, w / 2, h / 2) };
                let src = CVPixelBufferGetBaseAddressOfPlane(&input, plane) as *const u8;
                let ss = CVPixelBufferGetBytesPerRowOfPlane(&input, plane);
                let dst = CVPixelBufferGetBaseAddressOfPlane(&reference, plane) as *mut u8;
                let ds = CVPixelBufferGetBytesPerRowOfPlane(&reference, plane);
                if ratio32 { scale_3_2(src, ss, pw, ph, dst, ds, bpp, 8) } else { scale_2_1(src, ss, pw, ph, dst, ds, bpp, 8) }
                let got = CVPixelBufferGetBaseAddressOfPlane(&out, plane) as *const u8;
                let rows = CVPixelBufferGetHeightOfPlane(&out, plane);
                let width = CVPixelBufferGetWidthOfPlane(&out, plane) * bpp;
                for y in 0..rows {
                    let a = std::slice::from_raw_parts(dst.add(y * ds), width);
                    let b = std::slice::from_raw_parts(got.add(y * ds), width);
                    bad += a.iter().zip(b).filter(|(x, y)| (**x as i32 - **y as i32).abs() > 0).count();
                }
            }
            println!("  differs from scalar in {bad} samples");
            CVPixelBufferUnlockBaseAddress(&out, CVPixelBufferLockFlags::ReadOnly);
            CVPixelBufferUnlockBaseAddress(&reference, CVPixelBufferLockFlags::empty());
            CVPixelBufferUnlockBaseAddress(&input, CVPixelBufferLockFlags::ReadOnly);
        }
    }
}
