//! Encodes synthetic NV12 frames with the hardware encoder and decodes them back.
//! Needs no permissions, so it runs anywhere on Apple Silicon.

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
};
use platform_mac::decoder::Decoder;
use platform_mac::encoder::{EncodedFrame, Encoder, EncoderConfig};
use protocol::Codec;

const W: usize = 1920;
const H: usize = 1080;

fn nv12_frame(shift: usize) -> CFRetained<CVPixelBuffer> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let attrs: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[unsafe { kCVPixelBufferIOSurfacePropertiesKey }], &[empty.as_ref()]);
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(None, W, H, u32::from_be_bytes(*b"420f"), Some(attrs.as_opaque()), NonNull::from(&mut raw))
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
                    *px = if plane == 0 { ((x + y + shift) % 256) as u8 } else { 128 };
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
    }
    pb
}

#[test]
fn hevc_encode_decode_round_trip() {
    let (enc_tx, enc_rx) = mpsc::channel::<EncodedFrame>();
    let cfg = EncoderConfig { width: W as u32, height: H as u32, fps: 60, bitrate_bps: 20_000_000, codec: Codec::Hevc };
    let encoder = Encoder::new(&cfg, move |f| {
        let _ = enc_tx.send(f);
    })
    .expect("create encoder");
    println!("encoder: {:?}, low latency: {}", encoder.codec(), encoder.low_latency());

    let (dec_tx, dec_rx) = mpsc::channel();
    let mut decoder: Option<Decoder> = None;
    let mut encode_times = Vec::new();

    for i in 0..30u64 {
        let pb = nv12_frame(i as usize * 8);
        let started = Instant::now();
        encoder.encode(&pb, 1_000 + i * 16_667, i == 15).expect("encode");
        let frame = enc_rx.recv_timeout(Duration::from_secs(2)).expect("encoded frame");
        encode_times.push(started.elapsed());

        assert_eq!(frame.keyframe, i == 0 || i == 15, "frame {i} keyframe flag");
        if frame.keyframe {
            assert!(!frame.param_sets.is_empty());
            let tx = dec_tx.clone();
            decoder = Some(
                Decoder::new(frame.codec, &frame.param_sets, frame.nal_length_size, move |d| {
                    let _ = tx.send(d);
                })
                .expect("create decoder"),
            );
        }
        decoder.as_ref().unwrap().decode(&frame.data, i).expect("decode");
        let decoded = dec_rx.recv_timeout(Duration::from_secs(2)).expect("decoded frame");
        assert_eq!(decoded.tag, i);
        assert_eq!(CVPixelBufferGetWidth(&decoded.pixel_buffer), W);
        assert_eq!(CVPixelBufferGetHeight(&decoded.pixel_buffer), H);
    }
    encode_times.sort();
    println!("encode latency 1080p: median {:?}, max {:?}", encode_times[encode_times.len() / 2], encode_times.last().unwrap());
}
