//! Measures hardware HEVC encode latency at real display sizes, one frame at a time like the
//! host feeds it. Ignored by default because it takes a few seconds:
//!
//!   cargo test --release -p platform-mac --test encode_latency -- --ignored --nocapture
//!
//! Set `LANKVM_BENCH_SIZE=WIDTHxHEIGHT` to measure one size instead of the default set.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
};
use platform_mac::clock;
use platform_mac::encoder::{Encoder, EncoderConfig};
use protocol::Codec;

fn sizes() -> Vec<(usize, usize)> {
    let custom = std::env::var("LANKVM_BENCH_SIZE").ok().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    });
    match custom {
        Some(size) => vec![size],
        // 16" MacBook Pro at "More Space", at native, and two smaller scaled sizes.
        None => vec![(4112, 2658), (3456, 2234), (2560, 1654), (1920, 1240)],
    }
}

/// Screen-like content: text-sized high-contrast blocks with some noise, different per frame.
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
    let mut rng = seed.wrapping_mul(7919) | 1;
    unsafe {
        CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
        for plane in 0..2 {
            let base = CVPixelBufferGetBaseAddressOfPlane(&pb, plane) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, plane);
            let rows = CVPixelBufferGetHeightOfPlane(&pb, plane);
            for y in 0..rows {
                let row = std::slice::from_raw_parts_mut(base.add(y * stride), stride);
                for (x, px) in row.iter_mut().enumerate() {
                    *px = if plane == 0 {
                        rng ^= rng << 13;
                        rng ^= rng >> 17;
                        rng ^= rng << 5;
                        let cell = ((x / 6) as u32).wrapping_mul(2_654_435_761) ^ ((y / 12) as u32).wrapping_mul(40_503);
                        if rng & 63 == 0 { (rng >> 8) as u8 } else if (cell >> 13) & 3 == 0 { 20 } else { 235 }
                    } else {
                        128
                    };
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
    }
    pb
}

#[test]
#[ignore = "benchmark"]
fn encode_latency() {
    for (w, h) in sizes() {
        let frames: Vec<_> = (0..4).map(|i| frame(w, h, i)).collect();
        let bitrate = (w as f64 * h as f64 * 60.0 * 0.12).clamp(8e6, 150e6) as u32;
        let cfg = EncoderConfig { width: w as u32, height: h as u32, fps: 60, bitrate_bps: bitrate, codec: Codec::Hevc };
        let (tx, rx) = mpsc::channel();
        let encoder = Encoder::new(&cfg, move |f| {
            let _ = tx.send((clock::now_us() - f.encode_start_us, f.data.len()));
        })
        .expect("create encoder");

        let mut times = Vec::new();
        let mut bytes = 0;
        for (i, pb) in frames.iter().cycle().take(70).enumerate() {
            encoder.encode(pb, clock::now_us(), i == 0, 0).unwrap();
            let (us, len) = rx.recv_timeout(Duration::from_secs(2)).expect("encoded frame");
            // The first frames warm the encoder up.
            if i >= 10 {
                times.push(us as f64 / 1000.0);
                bytes += len;
            }
        }
        times.sort_by(f64::total_cmp);
        let median = times[times.len() / 2];
        println!(
            "{w}x{h} {:?} (hardware: {})  median {median:.1} ms  max {:.1} ms  → up to {:.0} fps  {} KB/frame",
            encoder.codec(),
            encoder.hardware(),
            times[times.len() - 1],
            1000.0 / median,
            bytes / times.len() / 1024,
        );
    }
}
