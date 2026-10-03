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
use platform_mac::decoder::{DecodedFrame, Decoder};
use platform_mac::encoder::{EncodedFrame, Encoder, EncoderConfig};
use protocol::{Codec, TileRect, tile_layout};

const W: usize = 1920;
const H: usize = 1080;
const WAIT: Duration = Duration::from_secs(2);

fn nv12_frame(width: usize, height: usize, shift: usize) -> CFRetained<CVPixelBuffer> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let attrs: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[unsafe { kCVPixelBufferIOSurfacePropertiesKey }], &[empty.as_ref()]);
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(None, width, height, u32::from_be_bytes(*b"420f"), Some(attrs.as_opaque()), NonNull::from(&mut raw))
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

fn encoder(width: u32, height: u32, bitrate_bps: u32, tx: mpsc::Sender<EncodedFrame>) -> Encoder {
    let cfg = EncoderConfig { width, height, fps: 60, bitrate_bps, codec: Codec::Hevc };
    Encoder::new(&cfg, move |f| {
        let _ = tx.send(f);
    })
    .expect("create encoder")
}

/// `frames` encoded frames of a moving pattern at `width`×`height` (the first one a keyframe).
fn encode_sequence(width: u32, height: u32, frames: u64) -> Vec<EncodedFrame> {
    let (tx, rx) = mpsc::channel();
    let encoder = encoder(width, height, 10_000_000, tx);
    (0..frames)
        .map(|i| {
            let pb = nv12_frame(width as usize, height as usize, i as usize * 8);
            encoder.encode(&pb, 1_000 + i * 16_667, false, i).expect("encode");
            rx.recv_timeout(WAIT).expect("encoded frame")
        })
        .collect()
}

fn decoder<T: Send + 'static>(first: &EncodedFrame, tx: mpsc::Sender<DecodedFrame<T>>) -> Decoder<T> {
    assert!(first.keyframe && !first.param_sets.is_empty());
    Decoder::new(first.codec, &first.param_sets, first.nal_length_size, move |d| {
        let _ = tx.send(d);
    })
    .expect("create decoder")
}

#[test]
fn hevc_encode_decode_round_trip() {
    let (enc_tx, enc_rx) = mpsc::channel::<EncodedFrame>();
    let encoder = encoder(W as u32, H as u32, 20_000_000, enc_tx);
    println!("encoder: {:?}, hardware: {}", encoder.codec(), encoder.hardware());

    let (dec_tx, dec_rx) = mpsc::channel();
    let mut decoder: Option<Decoder> = None;
    let mut encode_times = Vec::new();

    for i in 0..30u64 {
        let pb = nv12_frame(W, H, i as usize * 8);
        let started = Instant::now();
        encoder.encode(&pb, 1_000 + i * 16_667, i == 15, i).expect("encode");
        let frame = enc_rx.recv_timeout(WAIT).expect("encoded frame");
        encode_times.push(started.elapsed());

        assert_eq!(frame.keyframe, i == 0 || i == 15, "frame {i} keyframe flag");
        if frame.keyframe {
            decoder = Some(self::decoder(&frame, dec_tx.clone()));
        }
        decoder.as_ref().unwrap().decode(&frame.data, i).expect("decode");
        let decoded = dec_rx.recv_timeout(WAIT).expect("decoded frame");
        assert_eq!(decoded.tag, i);
        let image = decoded.image.expect("decoded image");
        assert_eq!(CVPixelBufferGetWidth(&image), W);
        assert_eq!(CVPixelBufferGetHeight(&image), H);
    }
    encode_times.sort();
    println!("encode latency 1080p: median {:?}, max {:?}", encode_times[encode_times.len() / 2], encode_times.last().unwrap());
}

/// One decoder per tile, all fed at once without waiting: every tile's frames come out, in
/// order, at the tile's size, with the tag they went in with.
#[test]
fn tile_decoders_run_at_once() {
    // Uneven tiles: 960×576 above 960×504.
    let tiles = tile_layout(W as u32, H as u32, Some((2, 2)));
    assert_eq!(tiles.len(), 4);
    let streams: Vec<Vec<EncodedFrame>> = tiles.iter().map(|t| encode_sequence(t.width, t.height, 8)).collect();

    let (tx, rx) = mpsc::channel::<DecodedFrame<(TileRect, u64)>>();
    let decoders: Vec<Decoder<(TileRect, u64)>> = streams.iter().map(|s| decoder(&s[0], tx.clone())).collect();
    for i in 0..8 {
        for ((tile, stream), dec) in tiles.iter().zip(&streams).zip(&decoders) {
            dec.decode(&stream[i].data, (*tile, i as u64)).expect("decode");
        }
    }
    let mut next = vec![0u64; tiles.len()];
    for _ in 0..8 * tiles.len() {
        let decoded = rx.recv_timeout(WAIT).expect("decoded tile");
        let (tile, i) = decoded.tag;
        assert_eq!(i, next[usize::from(tile.index)], "tile {} out of order", tile.index);
        next[usize::from(tile.index)] += 1;
        let image = decoded.image.unwrap_or_else(|status| panic!("tile {} frame {i}: OSStatus {status}", tile.index));
        assert_eq!((CVPixelBufferGetWidth(&image), CVPixelBufferGetHeight(&image)), (tile.width as usize, tile.height as usize));
    }
    assert!(next.iter().all(|&n| n == 8), "{next:?}");
    assert!(rx.try_recv().is_err(), "one result per frame");
}

/// Dropping a decoder with frames still inside delivers them first, so their tags and pictures
/// are never lost or used after the decoder is gone.
#[test]
fn drop_delivers_frames_in_flight() {
    let frames = encode_sequence(1280, 720, 6);
    let (tx, rx) = mpsc::channel();
    let dec = decoder(&frames[0], tx);
    for (i, f) in frames.iter().enumerate() {
        dec.decode(&f.data, i as u64).expect("decode");
    }
    drop(dec);
    let tags: Vec<u64> = rx.try_iter().map(|d| d.tag).collect();
    assert_eq!(tags, [0, 1, 2, 3, 4, 5]);
}

/// A frame the decoder can't take is reported at most once: to the callback, by `decode`, or both
/// (VideoToolbox may hand the failure to the callback before `decode` returns it). A keyframe
/// afterwards decodes again.
#[test]
fn bad_frames_are_reported() {
    let frames = encode_sequence(640, 480, 2);
    let (tx, rx) = mpsc::channel();
    let dec = decoder(&frames[0], tx);
    // One NAL unit of garbage, with a valid length prefix.
    let mut garbage = vec![0, 0, 0, 200];
    garbage.extend((0..200u32).map(|i| (i * 37 % 251) as u8));
    let accepted = dec.decode(&garbage, 7);
    dec.decode(&frames[0].data, 8).expect("decode keyframe");
    // Results come in decode order: the bad frame's, if any, before the keyframe's.
    let mut bad = Vec::new();
    let keyframe = loop {
        let decoded = rx.recv_timeout(WAIT).expect("decoded keyframe");
        match decoded.tag {
            7 => bad.push(decoded.image.err()),
            8 => break decoded,
            other => panic!("unexpected tag {other}"),
        }
    };
    println!("bad frame: decode {:?}, callback {bad:?}", accepted.as_ref().err());
    assert!(bad.len() <= 1, "the bad frame reached the callback {} times", bad.len());
    if accepted.is_ok() {
        assert_eq!(bad.len(), 1, "a frame decode took gets a result");
    }
    assert!(keyframe.image.is_ok());
    drop(dec);
    assert!(rx.try_recv().is_err(), "nothing after the keyframe");
}

/// Pictures come out exactly the size they went in, also when it isn't a multiple of a coding
/// block (the client drops a tile whose picture doesn't match its rectangle).
#[test]
fn odd_sizes_decode_to_their_size() {
    for (width, height) in [(1470, 956), (1470, 316), (1366, 768), (3072, 320), (2944, 64), (1366, 1024)] {
        let frames = encode_sequence(width, height, 2);
        let (tx, rx) = mpsc::channel();
        let dec = decoder(&frames[0], tx);
        for (i, f) in frames.iter().enumerate() {
            dec.decode(&f.data, i as u64).expect("decode");
            let image = rx.recv_timeout(WAIT).expect("decoded").image.expect("image");
            assert_eq!((CVPixelBufferGetWidth(&image), CVPixelBufferGetHeight(&image)), (width as usize, height as usize));
        }
    }
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// Time from receiving an update to having it decoded, at 6144×2560: one full-frame decoder
/// against 16 tile decoders (2 × 8 tiles of 3072×320) fed at once.
///
///   cargo test --release -p platform-mac --test codec_roundtrip -- --ignored --nocapture
#[test]
#[ignore = "benchmark"]
fn tile_decode_latency() {
    const FRAMES: u64 = 40;
    let (width, height) = (6144u32, 2560u32);
    let tiles = tile_layout(width, height, None);
    println!("{} tiles of {}×{}", tiles.len(), tiles[0].width, tiles[0].height);

    let whole = encode_sequence(width, height, FRAMES);
    let (tx, rx) = mpsc::channel();
    let dec = decoder(&whole[0], tx);
    let mut full = Vec::new();
    for (i, f) in whole.iter().enumerate() {
        let started = Instant::now();
        dec.decode(&f.data, i as u64).expect("decode");
        rx.recv_timeout(WAIT).expect("decoded").image.expect("image");
        if i > 0 {
            full.push(started.elapsed());
        }
    }
    drop(dec);

    let streams: Vec<Vec<EncodedFrame>> = tiles.iter().map(|t| encode_sequence(t.width, t.height, FRAMES)).collect();
    let (tx, rx) = mpsc::channel::<(Instant, Instant)>();
    // Each tile's tag is when it went in; the callback notes when it came out.
    let decoders: Vec<Decoder<Instant>> = streams
        .iter()
        .map(|s| {
            let tx = tx.clone();
            Decoder::new(s[0].codec, &s[0].param_sets, s[0].nal_length_size, move |d: DecodedFrame<Instant>| {
                assert!(d.image.is_ok());
                let _ = tx.send((d.tag, Instant::now()));
            })
            .expect("create decoder")
        })
        .collect();
    let (mut submit, mut tile, mut all) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..FRAMES as usize {
        let started = Instant::now();
        for (stream, dec) in streams.iter().zip(&decoders) {
            dec.decode(&stream[i].data, Instant::now()).expect("decode");
        }
        let submitted = started.elapsed();
        let mut last = started;
        for _ in 0..tiles.len() {
            let (went_in, came_out) = rx.recv_timeout(WAIT).expect("decoded");
            last = last.max(came_out);
            if i > 0 {
                tile.push(came_out - went_in);
            }
        }
        if i > 0 {
            submit.push(submitted);
            all.push(last - started);
        }
    }
    let bytes = |frames: &[EncodedFrame]| frames[1..].iter().map(|f| f.data.len()).sum::<usize>() / (frames.len() - 1);
    println!("full frame: decode median {:?} ({} bytes a frame)", median(full), bytes(&whole));
    println!(
        "16 tiles:   queueing all {:?}, each tile in → out {:?}, first in → last out {:?} ({} bytes an update), medians",
        median(submit),
        median(tile),
        median(all),
        streams.iter().map(|s| bytes(s)).sum::<usize>()
    );
}
