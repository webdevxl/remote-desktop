//! The tiler on synthetic NV12 frames: which tiles it reports, that their copies are byte-exact,
//! that row padding is ignored and bad input refused, and what the dirty-rectangle hint skips.
//! Needs no permissions.
//!
//!   cargo test --release -p platform-mac --test tiler -- --ignored --nocapture   # speed too

use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
};
use platform_mac::tiler::{TileCopy, Tiler};
use protocol::{TileRect, tile_layout};

/// A full-range NV12 frame, every byte from `value(plane, x_byte, y)`.
fn frame(w: usize, h: usize, value: impl Fn(usize, usize, usize) -> u8) -> CFRetained<CVPixelBuffer> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let attrs: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[unsafe { kCVPixelBufferIOSurfacePropertiesKey }], &[empty.as_ref()]);
    let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
    let status = unsafe {
        CVPixelBufferCreate(None, w, h, u32::from_be_bytes(*b"420f"), Some(attrs.as_opaque()), NonNull::from(&mut raw))
    };
    assert_eq!(status, 0);
    let pb = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };
    edit(&pb, |plane, x, y, px| *px = value(plane, x, y));
    pb
}

/// Runs `f` on every byte of both planes (within the plane's width; row padding is left alone).
fn edit(pb: &CVPixelBuffer, mut f: impl FnMut(usize, usize, usize, &mut u8)) {
    unsafe {
        assert_eq!(CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::empty()), 0);
        for plane in 0..2 {
            let base = CVPixelBufferGetBaseAddressOfPlane(pb, plane) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, plane);
            let bytes = CVPixelBufferGetWidthOfPlane(pb, plane) * (plane + 1);
            for y in 0..CVPixelBufferGetHeightOfPlane(pb, plane) {
                let row = std::slice::from_raw_parts_mut(base.add(y * stride), bytes);
                for (x, px) in row.iter_mut().enumerate() {
                    f(plane, x, y, px);
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::empty());
    }
}

/// Sets the row padding past each plane's width (if the buffer has any) to `value`.
fn fill_padding(pb: &CVPixelBuffer, value: u8) {
    unsafe {
        assert_eq!(CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::empty()), 0);
        for plane in 0..2 {
            let base = CVPixelBufferGetBaseAddressOfPlane(pb, plane) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, plane);
            let bytes = CVPixelBufferGetWidthOfPlane(pb, plane) * (plane + 1);
            for y in 0..CVPixelBufferGetHeightOfPlane(pb, plane) {
                std::slice::from_raw_parts_mut(base.add(y * stride + bytes), stride - bytes).fill(value);
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::empty());
    }
}

/// Both planes' bytes, rows without padding.
fn planes(pb: &CVPixelBuffer) -> [Vec<Vec<u8>>; 2] {
    let mut out = [Vec::new(), Vec::new()];
    unsafe {
        assert_eq!(CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly), 0);
        for (plane, rows) in out.iter_mut().enumerate() {
            let base = CVPixelBufferGetBaseAddressOfPlane(pb, plane) as *const u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, plane);
            let bytes = CVPixelBufferGetWidthOfPlane(pb, plane) * (plane + 1);
            for y in 0..CVPixelBufferGetHeightOfPlane(pb, plane) {
                rows.push(std::slice::from_raw_parts(base.add(y * stride), bytes).to_vec());
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
    }
    out
}

/// Distinct-ish bytes everywhere, so a misplaced copy can't match by accident.
fn pattern(plane: usize, x: usize, y: usize) -> u8 {
    (x.wrapping_mul(31) ^ y.wrapping_mul(17) ^ plane.wrapping_mul(101)).wrapping_add(x / 251 + y / 7) as u8
}

/// Asserts the copy holds exactly the frame's pixels under `rect`.
fn assert_copy(frame: &CVPixelBuffer, rect: TileRect, copy: &CVPixelBuffer) {
    let [fy, fuv] = planes(frame);
    let [ty, tuv] = planes(copy);
    let (x, y, w, h) = (rect.x as usize, rect.y as usize, rect.width as usize, rect.height as usize);
    assert_eq!((ty.len(), ty[0].len()), (h, w), "luma size of {rect:?}");
    for r in 0..h {
        assert!(ty[r] == fy[y + r][x..x + w], "luma row {r} of {rect:?}");
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    assert_eq!((tuv.len(), tuv[0].len()), (ch, cw * 2), "chroma size of {rect:?}");
    for r in 0..ch {
        assert!(tuv[r] == fuv[y / 2 + r][x..x + cw * 2], "chroma row {r} of {rect:?}");
    }
}

fn indices(copies: &[TileCopy]) -> Vec<usize> {
    copies.iter().map(|c| c.index).collect()
}

#[test]
fn reports_changed_tiles_with_exact_copies() {
    let (w, h) = (1280, 720);
    let tiles = tile_layout(w, h, Some((2, 3)));
    assert_eq!(tiles.len(), 6);
    let mut tiler = Tiler::new(w, h, &tiles).expect("tiler");
    let a = frame(w as usize, h as usize, pattern);

    // First call: every tile, byte for byte.
    let first = tiler.changed(&a).unwrap();
    assert_eq!(indices(&first), (0..6).collect::<Vec<_>>());
    for copy in &first {
        assert_copy(&a, tiles[copy.index], &copy.pixel_buffer);
    }
    drop(first);

    // The same pixels again, in the same buffer and in another one: nothing changed.
    assert!(tiler.changed(&a).unwrap().is_empty());
    let same = frame(w as usize, h as usize, pattern);
    assert!(tiler.changed(&same).unwrap().is_empty());

    // One luma pixel inside tile 4.
    let t4 = tiles[4];
    let (px, py) = (t4.x as usize + 77, t4.y as usize + 33);
    let b = frame(w as usize, h as usize, |p, x, y| pattern(p, x, y) ^ u8::from(p == 0 && x == px && y == py));
    let copies = tiler.changed(&b).unwrap();
    assert_eq!(indices(&copies), [4]);
    assert_copy(&b, t4, &copies[0].pixel_buffer);
    assert_eq!(planes(&tiler.last(4).unwrap())[0][33][77], pattern(0, px, py) ^ 1, "last has the new content");
    drop(copies);
    assert!(tiler.changed(&b).unwrap().is_empty(), "compared with the new content now");

    // Only one chroma byte (a V sample) of tile 1, at its last row and column.
    let t1 = tiles[1];
    let (cx, cy) = (t1.x as usize + t1.width as usize - 1, (t1.y + t1.height) as usize / 2 - 1);
    let c = frame(w as usize, h as usize, |p, x, y| {
        let base = pattern(p, x, y) ^ u8::from(p == 0 && x == px && y == py);
        base.wrapping_add(u8::from(p == 1 && x == cx && y == cy))
    });
    let copies = tiler.changed(&c).unwrap();
    assert_eq!(indices(&copies), [1]);
    assert_copy(&c, t1, &copies[0].pixel_buffer);

    // A copy that never reached the viewer: sent again though nothing changed.
    tiler.invalidate(2);
    tiler.invalidate(5);
    tiler.invalidate(99); // out of range: ignored
    let copies = tiler.changed(&c).unwrap();
    assert_eq!(indices(&copies), [2, 5]);
    assert_copy(&c, tiles[5], &copies[1].pixel_buffer);
    assert!(tiler.changed(&c).unwrap().is_empty());

    // `last` is what each tile returned most recently.
    for (i, rect) in tiles.iter().enumerate() {
        assert_copy(&c, *rect, &tiler.last(i).unwrap());
    }
    assert!(tiler.last(6).is_none());
}

#[test]
fn copies_hold_while_the_next_frames_are_tiled() {
    // A returned copy belongs to the encoder: later calls must never write into it.
    let (w, h) = (640, 384);
    let tiles = tile_layout(w, h, Some((2, 2)));
    let mut tiler = Tiler::new(w, h, &tiles).unwrap();
    let frames: Vec<_> = (0..4u8).map(|s| frame(w as usize, h as usize, move |p, x, y| pattern(p, x, y).wrapping_add(s))).collect();
    let mut held = Vec::new();
    for round in 0..3 {
        for f in &frames {
            let copies = tiler.changed(f).unwrap();
            assert_eq!(copies.len(), tiles.len(), "round {round}");
            held.push((f.clone(), copies));
        }
    }
    for (f, copies) in &held {
        for copy in copies {
            assert_copy(f, tiles[copy.index], &copy.pixel_buffer);
        }
    }
}

#[test]
fn uneven_layouts() {
    // Last row and column narrower than the rest, and odd frame sizes (chroma rounds up).
    for (w, h, grid) in [(1000, 650, (3, 3)), (999, 651, (2, 2)), (130, 66, (2, 1)), (2, 2, (1, 1))] {
        let tiles = tile_layout(w, h, Some(grid));
        let mut tiler = Tiler::new(w, h, &tiles).unwrap_or_else(|e| panic!("{w}×{h}: {e:#}"));
        let a = frame(w as usize, h as usize, pattern);
        let copies = tiler.changed(&a).unwrap();
        assert_eq!(copies.len(), tiles.len(), "{w}×{h}");
        for copy in &copies {
            assert_copy(&a, tiles[copy.index], &copy.pixel_buffer);
        }
        drop(copies);
        assert!(tiler.changed(&a).unwrap().is_empty(), "{w}×{h}");
        // Row padding isn't picture: it never counts as a change.
        let padded = frame(w as usize, h as usize, pattern);
        fill_padding(&a, 0x00);
        fill_padding(&padded, 0xff);
        assert!(tiler.changed(&padded).unwrap().is_empty(), "{w}×{h} padding");
        // The bottom-right pixel, in the smallest tile.
        let b = frame(w as usize, h as usize, |p, x, y| pattern(p, x, y) ^ u8::from(p == 0 && x == w as usize - 1 && y == h as usize - 1));
        let copies = tiler.changed(&b).unwrap();
        assert_eq!(indices(&copies), [tiles.len() - 1], "{w}×{h}");
        assert_copy(&b, tiles[tiles.len() - 1], &copies[0].pixel_buffer);
    }
}

#[test]
fn rejects_what_it_cant_tile() {
    let tiles = tile_layout(640, 480, None);
    let mut tiler = Tiler::new(640, 480, &tiles).unwrap();
    assert!(tiler.changed(&frame(320, 240, pattern)).is_err(), "frame of another size");
    assert!(tiler.changed(&frame(640, 480, pattern)).is_ok());

    assert!(Tiler::new(640, 480, &[]).is_err());
    let outside = TileRect { index: 0, x: 0, y: 0, width: 642, height: 480 };
    assert!(Tiler::new(640, 480, &[outside]).is_err());
    let odd = TileRect { index: 0, x: 1, y: 0, width: 64, height: 64 };
    assert!(Tiler::new(640, 480, &[odd]).is_err());
}

fn median_ms(mut times: Vec<f64>) -> f64 {
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

#[test]
#[ignore = "benchmark"]
fn tiler_speed() {
    // Like the host's encode thread.
    platform_mac::system::set_thread_interactive();
    let (w, h) = (6144u32, 2560u32);
    let tiles = tile_layout(w, h, None);
    let mut tiler = Tiler::new(w, h, &tiles).unwrap();
    let a = frame(w as usize, h as usize, pattern);
    let b = frame(w as usize, h as usize, |p, x, y| pattern(p, x, y) ^ 0x55);
    let t = tiles[5];
    let (px, py) = (t.x as usize + 100, t.y as usize + 100);
    let one = frame(w as usize, h as usize, |p, x, y| pattern(p, x, y) ^ u8::from(p == 0 && x == px && y == py));
    let mut run = |frames: [&CVPixelBuffer; 2], expect: usize, gap: Duration| {
        tiler.changed(frames[1]).unwrap();
        let mut times = Vec::new();
        for i in 0..200 {
            std::thread::sleep(gap);
            let t0 = Instant::now();
            let copies = tiler.changed(frames[i % 2]).unwrap();
            times.push(t0.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(copies.len(), expect);
        }
        median_ms(times)
    };
    // Back to back, and 8 ms apart like frames at 120 Hz (memory clocks down in between).
    for gap in [Duration::ZERO, Duration::from_millis(8)] {
        let none = run([&a, &a], 0, gap);
        let single = run([&one, &a], 1, gap);
        let all = run([&b, &a], tiles.len(), gap);
        println!(
            "tiler {w}x{h}, {} tiles, {gap:?} apart: changed() median  0 changed {none:.3} ms  1 changed {single:.3} ms  all changed {all:.3} ms",
            tiles.len()
        );
    }
    // With ScreenCaptureKit's dirty rectangles as the hint: only the touched tile is read.
    tiler.changed(&a).unwrap();
    let mut times = Vec::new();
    for i in 0..200 {
        std::thread::sleep(Duration::from_millis(8));
        let t0 = Instant::now();
        let copies = tiler.changed_in(if i % 2 == 0 { &one } else { &a }, 1 << 5).unwrap();
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(copies.len(), 1);
    }
    println!("tiler {w}x{h}, 8ms apart, hinted to the 1 changed tile: changed_in() median {:.3} ms", median_ms(times));
}

#[test]
fn hints_limit_what_is_read() {
    let (w, h) = (640u32, 480u32);
    let tiles = tile_layout(w, h, Some((2, 2)));
    let mut tiler = Tiler::new(w, h, &tiles).unwrap();
    let a = frame(w as usize, h as usize, pattern);
    // The first call looks at every tile, whatever the hint: none has content yet.
    assert_eq!(indices(&tiler.changed_in(&a, 0).unwrap()), [0, 1, 2, 3]);
    let b = frame(w as usize, h as usize, |p, x, y| pattern(p, x, y) ^ 1);
    // Every tile differs, but only the hinted ones are read and reported...
    assert_eq!(indices(&tiler.changed_in(&b, 0b0101).unwrap()), [0, 2]);
    // ...and the others still compare against their old content later.
    assert_eq!(indices(&tiler.changed(&b).unwrap()), [1, 3]);
    // An invalidated tile is looked at even when the hint leaves it out.
    tiler.invalidate(3);
    assert_eq!(indices(&tiler.changed_in(&b, 0).unwrap()), [3]);
    assert!(tiler.changed_in(&b, u64::MAX).unwrap().is_empty());
}
