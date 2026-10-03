//! Benchmark of the host's tiled pipeline as it runs: the GPU tiler finds the changed tiles of a
//! frame, each goes to its own hardware encoder, and the update is done when the last tile is out.
//! Compares tile grids (1x1 is one encoder for the whole picture) with every tile, one tile or no
//! tile changing per frame, and, for every tile changing, the host's full-frame path (the frame
//! itself through one encoder after the tiler). Also times making the encoders.
//!
//!   cargo test --release -p platform-mac --test stripe_encode -- --ignored --nocapture
//!
//! `LANKVM_BENCH_SIZE=WIDTHxHEIGHT` picks the frame size (default 6144x2560).

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferIOSurfacePropertiesKey,
};
use platform_mac::encoder::{Encoder, EncoderConfig};
use platform_mac::tiler::Tiler;
use protocol::{Codec, tile_layout};

fn size() -> (usize, usize) {
    std::env::var("LANKVM_BENCH_SIZE")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((6144, 2560))
}

/// Screen-like content: text-sized high-contrast blocks with some noise, different per seed.
/// Inside `patch` (x0, y0, x1, y1) the seed is `patch_seed` instead, like a few changed words.
fn frame(w: usize, h: usize, seed: u32, patch: (usize, usize, usize, usize), patch_seed: u32) -> CFRetained<CVPixelBuffer> {
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
                    let inside = x >= patch.0 && x < patch.2 && y >= patch.1 && y < patch.3;
                    let s = if inside { patch_seed } else { seed };
                    // Position-hashed noise, so the same seed gives the same pixels anywhere.
                    let mut r = (x as u32).wrapping_mul(0x9e37_79b9) ^ (y as u32).wrapping_mul(0x85eb_ca6b) ^ s.wrapping_mul(0xc2b2_ae35);
                    r ^= r >> 15;
                    r = r.wrapping_mul(0x2c1b_3c6d);
                    r ^= r >> 12;
                    let cell = ((x / 6) as u32 + s).wrapping_mul(2_654_435_761) ^ ((y / 12) as u32).wrapping_mul(40_503);
                    *px = if r & 63 == 0 { (r >> 8) as u8 } else if (cell >> 13) & 3 == 0 { 20 } else { 235 };
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
    }
    pb
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Change {
    All,
    One,
    None,
}

/// How the changed tiles go out: each through its own encoder, or (the host's choice when most
/// of the picture changed) the captured frame itself through one encoder for the whole picture.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Route {
    Tiles,
    FullFrame,
}

struct Stats {
    tiler_ms: f64,
    first_ms: f64,
    all_ms: f64,
    tiles: f64,
    kb: f64,
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// The stream's bit budget, as the host sets it (60 fps worth).
fn budget(w: u32, h: u32) -> f64 {
    (w as f64 * h as f64 * 60.0 * 0.12).clamp(8e6, 150e6)
}

/// A hardware HEVC encoder of `w`×`h` sending its frames' sizes to `tx`.
fn encoder(w: u32, h: u32, bitrate_bps: u32, tx: &mpsc::Sender<usize>) -> Option<Encoder> {
    let cfg = EncoderConfig { width: w, height: h, fps: 120, bitrate_bps, codec: Codec::Hevc };
    let tx = tx.clone();
    let enc = Encoder::new(&cfg, move |f| {
        let _ = tx.send(f.data.len());
    })
    .ok()?;
    if enc.codec() != Codec::Hevc || !enc.hardware() {
        println!("  {w}x{h}: no HEVC hardware encoder (got {:?})", enc.codec());
        return None;
    }
    Some(enc)
}

/// Encoders for `grid`, as the host makes them (all at once, on threads of their own): each tile
/// its area's share of the bitrate. Also the time it took.
fn encoders(w: u32, h: u32, grid: (u32, u32), tx: &mpsc::Sender<usize>, parallel: bool) -> Option<(Vec<protocol::TileRect>, Vec<Encoder>, f64)> {
    let tiles = tile_layout(w, h, Some(grid));
    let total = budget(w, h);
    let share = |t: &protocol::TileRect| (total * (t.width * t.height) as f64 / (w * h) as f64).max(1e6) as u32;
    let t0 = Instant::now();
    let list: Vec<Option<Encoder>> = if parallel {
        std::thread::scope(|s| {
            let threads: Vec<_> = tiles.iter().map(|t| s.spawn(move || encoder(t.width, t.height, share(t), tx))).collect();
            threads.into_iter().map(|t| t.join().unwrap()).collect()
        })
    } else {
        tiles.iter().map(|t| encoder(t.width, t.height, share(t), tx)).collect()
    };
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    Some((tiles, list.into_iter().collect::<Option<Vec<_>>>()?, ms))
}

fn run(w: usize, h: usize, grid: (u32, u32), change: Change, route: Route, iters: usize) -> Option<(usize, f64, Stats)> {
    let (tx, rx) = mpsc::channel();
    let (tiles, encoders, setup_ms) = encoders(w as u32, h as u32, grid, &tx, true)?;
    let full = match route {
        Route::Tiles => None,
        Route::FullFrame => Some(encoder(w as u32, h as u32, budget(w as u32, h as u32) as u32, &tx)?),
    };
    let mut tiler = Tiler::new(w as u32, h as u32, &tiles).unwrap();
    // A few words change in the middle of one tile, which (moving alone) gets the whole budget.
    let t = tiles[tiles.len() / 2];
    if change == Change::One {
        encoders[tiles.len() / 2].set_bitrate(budget(w as u32, h as u32) as u32).unwrap();
    }
    let patch = (t.x as usize + 40, t.y as usize + 40, t.x as usize + 400, t.y as usize + 64);
    let frames: Vec<_> = match change {
        Change::All => (0..4).map(|s| frame(w, h, s, (0, 0, 0, 0), 0)).collect(),
        Change::One => (0..2).map(|s| frame(w, h, 0, patch, s + 1)).collect(),
        Change::None => vec![frame(w, h, 0, (0, 0, 0, 0), 0)],
    };
    let (mut tiler_ms, mut firsts, mut alls, mut counts, mut bytes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), 0);
    for it in 0..iters {
        let t0 = Instant::now();
        let f = &frames[it % frames.len()];
        let copies = tiler.changed(f).unwrap();
        let tiled = t0.elapsed();
        let outputs = match &full {
            Some(full) if !copies.is_empty() => {
                full.encode(f, platform_mac::clock::now_us(), false, it as u64).unwrap();
                1
            }
            _ => {
                for c in &copies {
                    encoders[c.index].encode(&c.pixel_buffer, platform_mac::clock::now_us(), false, it as u64).unwrap();
                }
                copies.len()
            }
        };
        let (mut first, mut last, mut size) = (None, tiled, 0);
        for _ in 0..outputs {
            size += rx.recv_timeout(Duration::from_secs(2)).expect("encoded tile");
            last = t0.elapsed();
            first.get_or_insert(last);
        }
        // The first frames warm the encoders up.
        if it >= 10 {
            tiler_ms.push(tiled.as_secs_f64() * 1000.0);
            firsts.push(first.unwrap_or(tiled).as_secs_f64() * 1000.0);
            alls.push(last.as_secs_f64() * 1000.0);
            counts.push(copies.len() as f64);
            bytes += size;
        }
    }
    let n = alls.len() as f64;
    let stats = Stats {
        tiler_ms: median(tiler_ms),
        first_ms: median(firsts),
        all_ms: median(alls),
        tiles: counts.iter().sum::<f64>() / n,
        kb: bytes as f64 / n / 1024.0,
    };
    Some((tiles.len(), setup_ms, stats))
}

#[test]
#[ignore = "benchmark"]
fn tiled_encode() {
    let (w, h) = size();
    let default = tile_layout(w as u32, h as u32, None);
    let cols = default.iter().filter(|t| t.y == 0).count() as u32;
    let default_grid = (cols, default.len() as u32 / cols);

    // Making the encoders, one after another and all at once (a display switch waits for it).
    let (tx, _rx) = mpsc::channel();
    for parallel in [false, true, false, true] {
        if let Some((tiles, _, ms)) = encoders(w as u32, h as u32, default_grid, &tx, parallel) {
            println!("{w}x{h}: {} encoders made {} in {ms:.0} ms", tiles.len(), if parallel { "at once" } else { "one by one" });
        }
    }

    let mut grids = vec![(1, 1), (1, 4), (default_grid.0, default_grid.1.div_ceil(2)), default_grid, (cols * 2, default_grid.1)];
    grids.dedup();
    let mut cases: Vec<_> = grids.iter().flat_map(|&g| [Change::All, Change::One, Change::None].map(|c| (g, c, Route::Tiles))).collect();
    // What the host does when most of the picture changes.
    cases.insert(cases.iter().position(|&(g, c, _)| g == default_grid && c == Change::All).unwrap() + 1, (default_grid, Change::All, Route::FullFrame));
    for (grid, change, route) in cases {
        let Some((n, setup_ms, s)) = run(w, h, grid, change, route, 70) else { continue };
        let how = match route {
            Route::Tiles => "",
            Route::FullFrame => " as one full frame",
        };
        println!(
            "{w}x{h} grid {}x{} ({n} tiles, encoders made in {setup_ms:.0} ms)  {change:?} changed{how}: {:.1} tiles/update  \
             tiler {:.2} ms  first out {:.2} ms  all out {:.2} ms  ({:.0} updates/s)  {:.0} KB/update",
            grid.0,
            grid.1,
            s.tiles,
            s.tiler_ms,
            s.first_ms,
            s.all_ms,
            1000.0 / s.all_ms,
            s.kb,
        );
    }
}
