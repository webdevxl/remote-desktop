//! Headless viewer for testing a real connection. It connects to a LanKVM host, pairs if
//! needed, receives and hardware-decodes the host's screen for a while, and prints the same
//! stats as the viewer overlay. It runs the app's core, minus drawing into a window.
//!
//!   cargo run --release -p lankvm-core --example probe -- 127.0.0.1:47800 [--seconds 10] [--max 1920x1200]
//!
//! On first contact the host shows a PIN: type it here (one line on stdin). The probe keeps its
//! own identity in `~/Library/Application Support/lankvm-probe` (or `$LANKVM_DATA_DIR`), so later
//! runs connect without pairing. Exits 0 if frames were decoded, 1 otherwise.
//!
//! ScreenCaptureKit only sends frames when the host's screen changes, so fps follows what's on
//! screen; keep something animating there to measure throughput.
//!
//! Remote control (the host must allow it):
//!
//!   probe HOST --control --script steps.jsonl      # play input, one JSON object per line
//!   probe HOST --control --input-latency 20 --rect X,Y,W,H
//!   probe HOST --control --lab input-lab.jsonl --script steps.jsonl --input-latency 20
//!
//! Positions are in the host display's points (top-left origin), converted with `--display WxH`
//! (default: this Mac's main display, right for a host on the same Mac), or named after a part of
//! LanKVM Input Lab given `--lab <its log>`: "text", "patch", "scroll", or "text+10,20" for a
//! point that far from that part's top-left corner. Script steps:
//!   {"move":[x,y]}  {"click":[x,y],"button":0,"clicks":1}  {"double":[x,y]}  {"triple":[x,y]}
//!   {"down":[x,y],"button":0}  {"up":[x,y],"button":0}  {"drag":[[x0,y0],[x1,y1]],"steps":12}
//!   {"scroll":[x,y],"dy":-120,"dx":0,"steps":6}   trackpad-style, with began/changed/ended phases
//!   {"wheel":[x,y],"lines":-3}  {"mods":["lcmd","lshift"]}  {"key":"a"} or {"key":36}
//!   {"keydown":"a"}  {"keyup":"a"}
//!   {"text":"Hello, world"}  {"hold":"a","ms":1500,"heartbeat":true}  {"sleep":100}
//!   {"release":true}  {"control":false}  {"disconnect":true}  {"relayed":1} (input as if relayed)
//!   {"assert_idle":true}   fails unless this Mac holds no modifier or mouse button now
//! Trackpad gestures (updates 16 ms apart, like a trackpad's):
//!   {"pinch":[x,y],"amount":0.5,"steps":8}    magnify began, changed ×steps (to 1.5×), ended
//!   {"rotate":[x,y],"degrees":45,"steps":8}   counterclockwise positive
//!   {"smart_magnify":[x,y]}  {"swipe":[x,y],"dx":-1,"dy":0} (swipe between pages)
//!   {"dock":"vertical","to":1.0,"steps":10,"velocity":2.0,"end":"ended"}   a swipe the Dock acts
//!       on: horizontal, vertical or pinch; progress in the wire's convention (+ is right, Mission
//!       Control); "end" is ended, cancelled or none (leave it hanging, e.g. to test silence)
//!   {"system":"mission_control"}   also app_expose, show_desktop, launchpad, previous_space,
//!       next_space
//!
//! A virtual display (the host makes a display this size and streams it instead of its own):
//!
//!   probe HOST --virtual 6144x2560@2x [--virtual 3840x2160 ...] [--then-main] [--arrange extend|main|only] [--refresh 60]
//!
//! WxH is in pixels; @2x makes it Retina (it looks like half that). Each size is asked for in turn
//! (the host resizes its display in place) and watched for --seconds; --then-main goes back to the
//! host's own screen at the end. The arrangement defaults to extend, which leaves the host's own
//! screen alone. Passes if the decoded frames have the size of each step.
//!
//! `--input-latency N` clicks the middle of `--rect` (for example LanKVM Input Lab's patch,
//! which flips between black and white on every click) N times and times each click until the
//! decoded video shows the change: input → host → app redraw → capture → encode → network →
//! decode. Display (≈1 frame) is not included.
//!
//! Frame numbers (LanKVM Frame Source, scripts/frame-source.swift, draws its frame number in a
//! strip of black and white squares on the host's screen):
//!
//!   probe HOST --virtual 2560x1600@2x --refresh 120 --barcode-log fs.jsonl [--trace updates.jsonl]
//!   probe HOST --barcode X,Y,W,H [--barcode-scale 2] [--trace updates.jsonl]
//!
//! reads the number off every update decoded here, and reports per second and at the end how many
//! source frames arrived (and how many never did), capture → decoded per update, and, joined with
//! the source's log, source presented → decoded per frame (committed → decoded on a screen that
//! reports no presentation times, as a virtual display doesn't). `--barcode-log` takes the strip's
//! place from the source's log (its last "window" line, waited for); `--barcode` gives it in the
//! captured display's points, converted with `--display WxH` or the display's pixels over
//! `--barcode-scale` (default: 2 for a Retina virtual display, 1 for another, this Mac's main
//! display's own scale). `--trace` writes a JSON line per finished update: {update, n,
//! capture_local_us, decoded_us, tiles, bytes, new, full, motion} (n null where unreadable; new:
//! the first update showing n; bytes null until the frame probe sees encoded sizes).

use std::collections::HashMap;
use std::io::{BufRead, Read, Seek, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use lankvm_core::{Core, Event, ProbeFrame};
use objc2_core_foundation::Type;
use platform_mac::{CFRetained, CVPixelBuffer, clock};
use protocol::{
    Arrangement, DisplayChoice, DockAxis, FULL_FRAME_TILE, GestureInput, GesturePhase, InputMsg, MOTION_FRAME_TILE, POS_MAX, ScrollInput,
    SystemAction, TileRect, VirtualDisplaySpec,
};
use serde_json::Value;

/// Long enough for someone to read the PIN off the host (which allows 120 s).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(150);

struct Args {
    target: String,
    seconds: u64,
    max: (u32, u32),
    control: bool,
    script: Option<String>,
    latency: Option<usize>,
    rect: Option<(f64, f64, f64, f64)>,
    display: Option<(f64, f64)>,
    /// Frame rate to ask for (the viewer's refresh rate).
    fps: u32,
    /// LanKVM Input Lab's log: script positions may name its parts ("text", "patch", "scroll").
    lab: Option<String>,
    /// Virtual displays to ask the host for, one after the other.
    virtual_displays: Vec<VirtualDisplaySpec>,
    /// Then go back to the host's own screen.
    then_main: bool,
    /// Where LanKVM Frame Source's frame number strip is, to read the frame each update shows.
    barcode: Option<Barcode>,
    /// The captured display's pixels per point, for `--barcode X,Y,W,H`.
    barcode_scale: Option<f64>,
    /// One JSON line per finished update goes here.
    trace: Option<String>,
}

enum Barcode {
    /// The source's log: its last "window" line says where the strip is, its "frame" lines when
    /// each frame was presented.
    Log(String),
    /// The strip's rectangle in the captured display's points (16 squares).
    Rect((f64, f64, f64, f64)),
}

/// "X,Y,W,H".
fn rect_of(s: &str) -> Option<(f64, f64, f64, f64)> {
    let v: Vec<f64> = s.split(',').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    (v.len() == 4).then(|| (v[0], v[1], v[2], v[3]))
}

/// "6144x2560" or "6144x2560@2x".
fn virtual_size(s: &str) -> Option<(u32, u32, bool)> {
    let (size, hidpi) = match s.strip_suffix("@2x") {
        Some(size) => (size, true),
        None => (s, false),
    };
    let (w, h) = pair_of(size, 'x')?;
    Some((w, h, hidpi))
}

fn pair_of<T: std::str::FromStr>(s: &str, sep: char) -> Option<(T, T)> {
    let (a, b) = s.split_once(sep)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut a = Args {
        target: String::new(),
        seconds: 10,
        max: (16384, 16384),
        control: false,
        script: None,
        latency: None,
        rect: None,
        display: None,
        lab: None,
        fps: 120,
        virtual_displays: Vec::new(),
        then_main: false,
        barcode: None,
        barcode_scale: None,
        trace: None,
    };
    let mut target = None;
    let mut arrangement = Arrangement::EXTEND;
    let mut refresh_hz = 60;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => a.seconds = args.next().and_then(|s| s.parse().ok()).ok_or("--seconds needs a number")?,
            "--max" => a.max = args.next().and_then(|s| pair_of(&s, 'x')).ok_or("--max needs WIDTHxHEIGHT")?,
            "--control" => a.control = true,
            "--script" => a.script = Some(args.next().ok_or("--script needs a file")?),
            "--input-latency" => a.latency = Some(args.next().and_then(|s| s.parse().ok()).ok_or("--input-latency needs a count")?),
            "--rect" => a.rect = Some(args.next().as_deref().and_then(rect_of).ok_or("--rect needs X,Y,W,H")?),
            "--display" => a.display = Some(args.next().and_then(|s| pair_of(&s, 'x')).ok_or("--display needs WIDTHxHEIGHT")?),
            "--lab" => a.lab = Some(args.next().ok_or("--lab needs Input Lab's log file")?),
            "--fps" => a.fps = args.next().and_then(|s| s.parse().ok()).ok_or("--fps needs a number")?,
            "--virtual" => {
                let (width, height, hidpi) = args.next().as_deref().and_then(virtual_size).ok_or("--virtual needs WIDTHxHEIGHT or WIDTHxHEIGHT@2x")?;
                a.virtual_displays.push(VirtualDisplaySpec { width, height, hidpi, refresh_hz: 0, arrangement: Arrangement::EXTEND });
            }
            "--arrange" => {
                arrangement = match args.next().as_deref() {
                    Some("extend") => Arrangement::EXTEND,
                    Some("main") => Arrangement::MAIN,
                    Some("only") => Arrangement::ONLY,
                    _ => return Err("--arrange needs extend, main or only".into()),
                }
            }
            "--then-main" => a.then_main = true,
            "--refresh" => refresh_hz = args.next().and_then(|s| s.parse().ok()).ok_or("--refresh needs a number")?,
            "--barcode-log" => a.barcode = Some(Barcode::Log(args.next().ok_or("--barcode-log needs Frame Source's log file")?)),
            "--barcode" => a.barcode = Some(Barcode::Rect(args.next().as_deref().and_then(rect_of).ok_or("--barcode needs X,Y,W,H")?)),
            "--barcode-scale" => {
                a.barcode_scale = Some(args.next().and_then(|s| s.parse().ok()).filter(|s: &f64| *s > 0.0).ok_or("--barcode-scale needs a number")?)
            }
            "--trace" => a.trace = Some(args.next().ok_or("--trace needs a file")?),
            _ if target.is_none() && !arg.starts_with('-') => target = Some(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    a.target = target.ok_or(
        "usage: probe <host[:port]> [--seconds N] [--max WxH] [--fps N] [--virtual WxH[@2x] [--arrange extend|main|only] [--refresh HZ]] [--control [--script FILE] [--input-latency N --rect X,Y,W,H] [--display WxH]] [--barcode-log FILE | --barcode X,Y,W,H [--barcode-scale S]] [--trace FILE]",
    )?;
    if (a.script.is_some() || a.latency.is_some()) && !a.control {
        return Err("--script and --input-latency need --control".into());
    }
    if a.latency.is_some() && a.rect.is_none() && a.lab.is_none() {
        return Err("--input-latency needs --rect (or --lab, to use its patch)".into());
    }
    for spec in &mut a.virtual_displays {
        spec.arrangement = arrangement;
        spec.refresh_hz = refresh_hz;
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    // A separate device from the app on this Mac: own identity, any free port. Set before the
    // core starts any threads.
    if std::env::var_os("LANKVM_DATA_DIR").is_none() {
        let home = std::env::var("HOME").unwrap_or_default();
        unsafe { std::env::set_var("LANKVM_DATA_DIR", format!("{home}/Library/Application Support/lankvm-probe")) };
    }
    if std::env::var_os("LANKVM_PORT").is_none() {
        unsafe { std::env::set_var("LANKVM_PORT", "0") };
    }

    // Tiles decoded before the strip's place is known are kept to read it from.
    let mut watch = Watch { latest: args.barcode.is_some().then(HashMap::new), ..Default::default() };
    if let Some(path) = &args.trace {
        match std::fs::File::create(path) {
            Ok(file) => watch.trace = Some(std::io::BufWriter::new(file)),
            Err(e) => {
                eprintln!("--trace {path}: {e}");
                return ExitCode::from(2);
            }
        }
    }

    let (tx, events) = mpsc::channel();
    let core = match Core::start(Arc::new(move |e| drop(tx.send(e)))) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("start: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let id = core.connect(&args.target, args.max, args.fps);
    let source = match &args.barcode {
        Some(Barcode::Log(path)) => Some(SourceLog::new(path)),
        _ => None,
    };
    let mut probe = Probe {
        core: core.clone(),
        id,
        events,
        pending: Vec::new(),
        lab: Default::default(),
        watch: Arc::new(Mutex::new(watch)),
        source,
        shown: None,
    };
    let code = probe.run(&args);
    core.disconnect(id);
    // Skip tearing down the runtime and capture threads; the OS cleans up.
    std::process::exit(code)
}

/// What the host shows the probe.
struct Shown {
    kind: &'static str,
    stream: (u32, u32, u32),
    /// The display's own size in pixels (the stream's for the host's own screen).
    pixels: (u32, u32),
    hidpi: bool,
    arrangement: &'static str,
    reason: u16,
    message: String,
    unavailable: String,
}

impl std::fmt::Display for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (w, h, fps) = self.stream;
        write!(f, "display: {} ({w}x{h} @ {fps} fps", self.kind)?;
        if self.kind == "virtual" {
            write!(f, ", {}{}", self.arrangement, if self.hidpi { ", Retina" } else { "" })?;
        }
        write!(f, ")")?;
        if self.reason != 0 || !self.message.is_empty() {
            write!(f, " — reason {}: {}", self.reason, self.message)?;
        }
        if !self.unavailable.is_empty() {
            write!(f, " [virtual displays: {}]", self.unavailable)?;
        }
        Ok(())
    }
}

struct Probe {
    core: Arc<Core>,
    id: u64,
    events: mpsc::Receiver<Event>,
    /// Events that arrived while waiting for another kind.
    pending: Vec<Event>,
    /// Named rectangles (points, top-left origin) from Input Lab.
    lab: HashMap<String, (f64, f64, f64, f64)>,
    /// What every frame probe this installs (but the input-latency one) learns.
    watch: Arc<Mutex<Watch>>,
    /// Frame Source's log, given `--barcode-log`.
    source: Option<SourceLog>,
    /// The display the host shows now.
    shown: Option<Shown>,
}

/// Times whole updates: from the host showing a frame to the last of its tiles decoded here (the
/// viewer shows an update once all its tiles are in, so per-tile averages flatter big changes).
#[derive(Default)]
struct UpdateTimes {
    /// By update: the tiles it sent, those decoded, its capture time (our clock), and the newest
    /// decode time.
    pending: HashMap<u32, (u64, u64, Option<u64>, u64)>,
    /// Finished updates' latency, ms, since the last `take`.
    samples: Vec<f64>,
    /// And since the last `take_all`.
    all: Vec<f64>,
    /// Newest update seen.
    newest: Option<u32>,
}

/// An update all of whose tiles were decoded.
struct Finished {
    update: u32,
    /// Its tiles ([`protocol::VideoFrame::update_mask`]).
    mask: u64,
    /// Captured on the host (our clock, µs), and its last tile decoded.
    captured: Option<u64>,
    decoded: u64,
}

/// Wrapping-aware "update a comes after b".
fn is_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

impl UpdateTimes {
    /// Takes in a decoded tile; returns its update if that was its last tile.
    fn tile(&mut self, f: &ProbeFrame<'_>) -> Option<Finished> {
        let entry = self.pending.entry(f.update).or_insert((0, 0, f.timing.capture_local_us, 0));
        entry.0 |= f.update_mask;
        entry.1 |= f.tile.bit();
        entry.3 = entry.3.max(f.timing.decoded_us);
        let mut finished = None;
        if entry.1 & entry.0 == entry.0 {
            let (mask, _, captured, decoded) = self.pending.remove(&f.update).expect("just seen");
            if let Some(captured) = captured {
                let ms = decoded.saturating_sub(captured) as f64 / 1000.0;
                self.samples.push(ms);
                self.all.push(ms);
            }
            finished = Some(Finished { update: f.update, mask, captured, decoded });
        }
        // Updates that lost a tile never finish (decoders finish in any order: go by the newest).
        if self.newest.is_none_or(|n| is_newer(f.update, n)) {
            self.newest = Some(f.update);
        }
        if self.pending.len() > 64 {
            let newest = self.newest.unwrap_or(f.update);
            self.pending.retain(|&u, _| newest.wrapping_sub(u) < 32);
        }
        finished
    }

    fn take(&mut self) -> Vec<f64> {
        std::mem::take(&mut self.samples)
    }

    fn take_all(&mut self) -> Vec<f64> {
        std::mem::take(&mut self.all)
    }
}

/// What the probe learns from every decoded tile: when whole updates finish, which source frame
/// each shows, and the trace of them.
#[derive(Default)]
struct Watch {
    updates: UpdateTimes,
    /// Frame Source's strip, once its place is known.
    strip: Option<Strip>,
    /// Until then (given a strip to look for), each tile's latest picture, (picture, tile, stream,
    /// update): read once the place is known, since a square whose tile doesn't change again
    /// (a high bit of the frame number) would otherwise stay unread for long.
    latest: Option<HashMap<u8, (Picture, TileRect, (u32, u32), u32)>>,
    frames: SourceFrames,
    trace: Option<std::io::BufWriter<std::fs::File>>,
}

impl Watch {
    fn tile(&mut self, f: &ProbeFrame<'_>) {
        match (&mut self.strip, &mut self.latest) {
            (Some(strip), _) => strip.sample(f.pixel_buffer, f.tile, f.stream, f.update),
            (None, Some(latest)) => {
                if latest.get(&f.tile.index).is_none_or(|l| is_newer(f.update, l.3)) {
                    latest.insert(f.tile.index, (Picture(f.pixel_buffer.retain()), f.tile, f.stream, f.update));
                }
            }
            (None, None) => {}
        }
        let Some(done) = self.updates.tile(f) else { return };
        let read = self.strip.as_ref().map(|s| (s.read(done.update), s.modulus()));
        let n = read.map(|(n, modulus)| self.frames.finished(n, modulus, done.captured, done.decoded));
        if let Some(trace) = &mut self.trace {
            let (n, new) = match n.flatten() {
                Some((n, new)) => (Some(n), new),
                None => (None, false),
            };
            let line = serde_json::json!({
                "update": done.update,
                "n": n,
                "capture_local_us": done.captured,
                "decoded_us": done.decoded,
                "tiles": done.mask.count_ones(),
                // Not known here yet: the frame probe doesn't see the encoded size.
                "bytes": Value::Null,
                "new": new,
                "full": done.mask & (1 << FULL_FRAME_TILE) != 0,
                "motion": done.mask & (1 << MOTION_FRAME_TILE) != 0,
            });
            let _ = writeln!(trace, "{line}");
        }
    }

    /// Puts the strip in place, and reads it off the tiles decoded before, oldest first.
    fn place_strip(&mut self, mut strip: Strip) {
        let mut latest: Vec<_> = self.latest.take().unwrap_or_default().into_values().collect();
        // Update numbers wrap: order them by how far they are from one of them.
        let reference = latest.first().map_or(0, |l| l.3);
        latest.sort_by_key(|l| l.3.wrapping_sub(reference) as i32);
        for (picture, tile, stream, update) in &latest {
            strip.sample(&picture.0, *tile, *stream, *update);
        }
        self.strip = Some(strip);
    }
}

/// LanKVM Frame Source's frame number strip (scripts/frame-source.swift): a row of squares, each
/// white or black, that spell the frame number in Gray code, bit 0 leftmost. Every decoded tile
/// that covers a square says what that square shows from its update on; an update's frame number
/// is what the squares show as of that update. Decoders finish in any order, so that is the
/// newest sample from an update not newer than it, not the latest sample.
struct Strip {
    /// Where it is (x, y, width, height), and the size of the display it's on, in that display's
    /// points.
    rect: (f64, f64, f64, f64),
    display: (f64, f64),
    squares: usize,
    /// The stream the samples are of: another size starts over.
    stream: (u32, u32),
    /// Per square: recent samples, (update, white or black, or None when it's neither).
    seen: Vec<Vec<(u32, Option<bool>)>>,
}

/// Samples kept per square: enough for the updates in flight, with the newest of a square that
/// rarely changes among them (only the tiles over a square sample it).
const STRIP_SAMPLES: usize = 32;

/// How far from mid-grey (128) a square must be to count as white or black: anything else (the
/// desktop before the source draws, a half-decoded picture) is no frame number.
const STRIP_MARGIN: f64 = 48.0;

impl Strip {
    fn new(rect: (f64, f64, f64, f64), display: (f64, f64), squares: usize) -> Self {
        let squares = squares.clamp(1, 31);
        Self { rect, display, squares, stream: (0, 0), seen: vec![Vec::new(); squares] }
    }

    /// The strip's numbers count modulo this.
    fn modulus(&self) -> u64 {
        1 << self.squares
    }

    /// Square `i`'s inner 60% (clear of its edges, which compression and scaling blur) in pixels
    /// of a `stream`-sized stream, as (x0, y0, x1, y1).
    fn square(&self, i: usize, stream: (u32, u32)) -> (f64, f64, f64, f64) {
        let (fx, fy) = (f64::from(stream.0) / self.display.0, f64::from(stream.1) / self.display.1);
        let side = self.rect.2 / self.squares as f64;
        let (x, y, h) = (self.rect.0 + side * i as f64, self.rect.1, self.rect.3);
        ((x + side * 0.2) * fx, (y + h * 0.2) * fy, (x + side * 0.8) * fx, (y + h * 0.8) * fy)
    }

    /// Samples the squares that `tile`, decoded for `update` of a `stream`-sized stream, covers.
    /// A whole-picture frame's tile is the whole stream, however small its picture (the motion
    /// stream's): `part_luma` maps through the tile's rectangle.
    fn sample(&mut self, pixel_buffer: &CVPixelBuffer, tile: TileRect, stream: (u32, u32), update: u32) {
        if stream != self.stream {
            self.stream = stream;
            self.seen.iter_mut().for_each(Vec::clear);
        }
        let (tx, ty) = (f64::from(tile.x), f64::from(tile.y));
        let (tx1, ty1) = (tx + f64::from(tile.width), ty + f64::from(tile.height));
        for i in 0..self.squares {
            let (x0, y0, x1, y1) = self.square(i, stream);
            let part = (x0.max(tx), y0.max(ty), x1.min(tx1), y1.min(ty1));
            if part.2 - part.0 < 1.0 || part.3 - part.1 < 1.0 {
                continue;
            }
            // A square is one colour: any part of it says which.
            let white = part_luma(pixel_buffer, &tile, part).and_then(|luma| {
                if luma >= 128.0 + STRIP_MARGIN {
                    Some(true)
                } else if luma <= 128.0 - STRIP_MARGIN {
                    Some(false)
                } else {
                    None
                }
            });
            let seen = &mut self.seen[i];
            seen.push((update, white));
            if seen.len() > STRIP_SAMPLES {
                // The oldest goes: the furthest back from this update.
                let oldest = (0..seen.len()).max_by_key(|&j| update.wrapping_sub(seen[j].0) as i32).expect("not empty");
                seen.remove(oldest);
            }
        }
    }

    /// The frame number (modulo [`Strip::modulus`]) shown as of `update`, if every square has
    /// been seen and is white or black.
    fn read(&self, update: u32) -> Option<u32> {
        let mut gray = 0u32;
        for (i, seen) in self.seen.iter().enumerate() {
            // The newest sample not newer than `update`; two of one update (a square across tiles)
            // must agree.
            let mut best: Option<(u32, Option<bool>)> = None;
            for &(u, white) in seen.iter().filter(|(u, _)| !is_newer(*u, update)) {
                best = match best {
                    Some((b, _)) if is_newer(b, u) => best,
                    Some((b, w)) if b == u && w != white => Some((b, None)),
                    _ => Some((u, white)),
                };
            }
            if best?.1? {
                gray |= 1 << i;
            }
        }
        Some(gray_decode(gray))
    }
}

/// The number whose Gray code is `gray`.
fn gray_decode(gray: u32) -> u32 {
    let (mut n, mut shift) = (gray, gray >> 1);
    while shift != 0 {
        n ^= shift;
        shift >>= 1;
    }
    n
}

/// A jump in frame numbers bigger than this, either way, is no frame skipped or late: the source
/// started over (or the first number read wasn't one), and counting starts again from there.
const MAX_FRAME_JUMP: u64 = 1024;

/// The source frames finished updates showed, in the order they finished.
#[derive(Default)]
struct SourceFrames {
    /// The newest frame shown, counted from the source's start (the strip's number wraps).
    newest: Option<u64>,
    /// Frames as they first showed: (frame, its update's capture time, decoded time; µs).
    shown: Vec<(u64, Option<u64>, u64)>,
    /// Frames passed over: shown by the source, in no update here (or not in time for one).
    skipped: u64,
    /// Updates that showed an older frame than one already shown: they finished late.
    late: u64,
    /// Updates whose frame number couldn't be read.
    unread: u64,
    /// Times counting started over (see [`MAX_FRAME_JUMP`]).
    restarts: u64,
}

impl SourceFrames {
    /// An update finished showing frame `n` (modulo `modulus`), or one that couldn't be read.
    /// Returns the frame counted from the source's start, and whether it's the first update
    /// showing it.
    fn finished(&mut self, n: Option<u32>, modulus: u64, captured: Option<u64>, decoded: u64) -> Option<(u64, bool)> {
        let Some(n) = n else {
            self.unread += 1;
            return None;
        };
        let n = u64::from(n);
        let Some(newest) = self.newest else {
            self.newest = Some(n);
            self.shown.push((n, captured, decoded));
            return Some((n, true));
        };
        // The step from the newest, the short way round.
        let step = (n + modulus - newest % modulus) % modulus;
        let back = modulus - step;
        if step == 0 {
            Some((newest, false))
        } else if step <= MAX_FRAME_JUMP {
            let frame = newest + step;
            self.skipped += step - 1;
            self.newest = Some(frame);
            self.shown.push((frame, captured, decoded));
            Some((frame, true))
        } else if back <= MAX_FRAME_JUMP && back <= newest {
            self.late += 1;
            Some((newest - back, false))
        } else {
            self.restarts += 1;
            self.newest = Some(n);
            self.shown.push((n, captured, decoded));
            Some((n, true))
        }
    }
}

/// LanKVM Frame Source's log, read as it grows (the probe starts before the source does): where
/// its strip is, and when each frame reached the screen.
struct SourceLog {
    path: String,
    /// Bytes of complete lines read so far.
    read: u64,
    /// The strip's place from the latest "window" line: rectangle and screen size in points,
    /// and its squares.
    strip: Option<((f64, f64, f64, f64), (f64, f64), usize)>,
    /// Frame → when it was presented (0: it never reached the screen) and committed, in µs of
    /// the mach clock this probe's times are on too.
    frames: HashMap<u64, (u64, u64)>,
    /// Whether the screen reports presentation times at all: a virtual display doesn't.
    presents: bool,
}

impl SourceLog {
    fn new(path: &str) -> Self {
        Self { path: path.to_string(), read: 0, strip: None, frames: HashMap::new(), presents: false }
    }

    /// When frame `n` reached the screen: its presentation, or on a screen that doesn't report
    /// those, its commit (the source finished it; it shows from the next refresh or so on).
    fn on_screen(&self, n: u64) -> Option<u64> {
        let &(presented, committed) = self.frames.get(&n)?;
        if self.presents { (presented > 0).then_some(presented) } else { Some(committed) }
    }

    /// What [`SourceLog::on_screen`] goes by.
    fn basis(&self) -> &'static str {
        if self.presents { "presented" } else { "committed" }
    }

    /// On screen → decoded, and → captured, in ms, for the frames in `shown` (as in
    /// [`SourceFrames::shown`]) this log has by now.
    fn delays(&self, shown: &[(u64, Option<u64>, u64)]) -> (Vec<f64>, Vec<f64>) {
        let since = |at: u64, t: u64| (t as i64 - at as i64) as f64 / 1000.0;
        let mut decoded = Vec::new();
        let mut captured = Vec::new();
        for &(n, capture, decode) in shown {
            let Some(at) = self.on_screen(n) else { continue };
            decoded.push(since(at, decode));
            captured.extend(capture.map(|c| since(at, c)));
        }
        (decoded, captured)
    }

    /// Reads the lines written since the last time.
    fn poll(&mut self) {
        let Ok(mut file) = std::fs::File::open(&self.path) else { return };
        if file.metadata().is_ok_and(|m| m.len() < self.read) {
            *self = Self::new(&self.path); // a new run of the source
        }
        let mut text = Vec::new();
        if file.seek(std::io::SeekFrom::Start(self.read)).is_err() || file.read_to_end(&mut text).is_err() {
            return;
        }
        // The last line may be half written: it waits for the next time.
        let Some(end) = text.iter().rposition(|&b| b == b'\n') else { return };
        for line in text[..end].split(|&b| b == b'\n') {
            let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
            match v["type"].as_str() {
                Some("frame") => {
                    if let Some(n) = v["n"].as_u64() {
                        let (presented, committed) = (v["presented_us"].as_u64().unwrap_or(0), v["commit_us"].as_u64().unwrap_or(0));
                        self.presents |= presented > 0;
                        self.frames.insert(n, (presented, committed));
                    }
                }
                Some("window") => {
                    let numbers = |key: &str| v[key].as_array().map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>()).unwrap_or_default();
                    let (rect, screen) = (numbers("strip_points"), numbers("screen_points"));
                    if rect.len() == 4 && screen.len() == 2 {
                        let squares = v["squares"].as_u64().unwrap_or(16) as usize;
                        self.strip = Some(((rect[0], rect[1], rect[2], rect[3]), (screen[0], screen[1]), squares));
                    }
                }
                _ => {}
            }
        }
        self.read += end as u64 + 1;
    }
}

/// The latest window layout Input Lab logged: its named parts, as (x, y, w, h) in points.
fn lab_layout(path: &str) -> Result<HashMap<String, (f64, f64, f64, f64)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let window = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "window")
        .last()
        .ok_or(format!("no window line in {path}"))?;
    let mut out = HashMap::new();
    for name in ["frame", "content", "patch", "text", "scroll"] {
        if let Some(r) = window[name].as_array().filter(|r| r.len() == 4) {
            let v: Vec<f64> = r.iter().filter_map(Value::as_f64).collect();
            out.insert(name.to_string(), (v[0], v[1], v[2], v[3]));
        }
    }
    Ok(out)
}

impl Probe {
    fn run(&mut self, args: &Args) -> i32 {
        if let Err(code) = self.connect() {
            return code;
        }
        if !args.virtual_displays.is_empty() {
            let size = Arc::new(Mutex::new(None));
            let (sink, watch) = (size.clone(), self.watch.clone());
            self.core.set_frame_probe(
                self.id,
                Some(Box::new(move |f| {
                    // The stream's size, as long as the tile decoded to its own size (the motion
                    // stream's pictures are smaller by design: scaled up to the stream's size).
                    let decoded = platform_mac::gpu::frame_size(f.pixel_buffer);
                    let whole = decoded == (f.tile.width, f.tile.height) || f.tile.index == MOTION_FRAME_TILE;
                    *sink.lock().unwrap() = Some(if whole { f.stream } else { decoded });
                    watch.lock().unwrap().tile(f);
                })),
            );
            let steps = args.virtual_displays.iter().map(|spec| DisplayChoice::Virtual(*spec));
            for choice in steps.chain(args.then_main.then_some(DisplayChoice::Main)) {
                if let Err(code) = self.show(choice, args, &size) {
                    return code;
                }
            }
            if !args.control {
                return 0;
            }
        }
        if args.control {
            match self.request_control(true) {
                (true, _) => {
                    println!("control granted");
                    self.report_cursor();
                }
                (false, reason) => {
                    println!("FAIL: control refused: {}", reason.unwrap_or_else(|| "no reason".into()));
                    return 1;
                }
            }
            let display = args.display.unwrap_or_else(main_display_points);
            let lab = match args.lab.as_deref().map(lab_layout).transpose() {
                Ok(l) => l.unwrap_or_default(),
                Err(e) => {
                    println!("FAIL: {e}");
                    return 1;
                }
            };
            self.lab = lab;
            if let Some(path) = &args.script {
                let played = self.play_script(path, display);
                self.report_cursor();
                match played {
                    Ok(n) => println!("PASS: played {n} script steps"),
                    Err(e) => {
                        println!("FAIL: script: {e}");
                        return 1;
                    }
                }
            }
            let rect = args.rect.or_else(|| self.lab.get("patch").copied());
            if let (Some(n), Some(rect)) = (args.latency, rect) {
                return self.input_latency(n, rect, display);
            }
            if args.script.is_some() {
                return 0;
            }
        }
        self.watch_video(args)
    }

    /// Asks the host to show `choice`, watches it, and checks the decoded frames have its size.
    fn show(&mut self, choice: DisplayChoice, args: &Args, size: &Mutex<Option<(u32, u32)>>) -> Result<(), i32> {
        // The newest frame decoded counts, from before the answer: the host announces the switch
        // once the first new frame is decoded, and an idle display may send no more.
        let started = Instant::now();
        let request = self.core.set_display(self.id, choice);
        let want = match self.display(request, Duration::from_secs(30)) {
            Some(shown) if shown.reason == 0 => {
                println!("{shown} after {:.0} ms", started.elapsed().as_secs_f64() * 1000.0);
                let want = (shown.stream.0, shown.stream.1);
                self.shown = Some(shown);
                want
            }
            Some(shown) => {
                println!("FAIL: {shown}");
                return Err(1);
            }
            None => {
                println!("FAIL: no answer about the display");
                return Err(1);
            }
        };
        let code = self.watch_video(args);
        let got = *size.lock().unwrap();
        if got != Some(want) {
            println!("FAIL: decoded frames are {got:?}, not {want:?}");
            return Err(1);
        }
        println!("PASS: decoded frames are {}x{}", want.0, want.1);
        if code == 0 { Ok(()) } else { Err(code) }
    }

    /// Waits for the `display` event answering `request`.
    fn display(&mut self, request: u32, timeout: Duration) -> Option<Shown> {
        let answer = |e: &Event| matches!(e, Event::Display { request: r, .. } if *r == request);
        let shown = |e: Event| match e {
            Event::Display { info, reason, message, .. } => Shown {
                kind: info.display.kind,
                stream: (info.width, info.height, info.fps),
                pixels: (info.display.width, info.display.height),
                hidpi: info.display.hidpi,
                arrangement: info.display.arrangement,
                reason,
                message,
                unavailable: info.display_unavailable,
            },
            _ => unreachable!(),
        };
        if let Some(i) = self.pending.iter().position(answer) {
            return Some(shown(self.pending.remove(i)));
        }
        let deadline = Instant::now() + timeout;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.events.recv_timeout(left) {
                Ok(e) if answer(&e) => return Some(shown(e)),
                Ok(e) => self.pending.push(e),
                Err(_) => break,
            }
        }
        None
    }

    fn connect(&mut self) -> Result<(), i32> {
        let started = Instant::now();
        loop {
            let Some(left) = CONNECT_TIMEOUT.checked_sub(started.elapsed()) else {
                println!("FAIL: not connected after {}s", CONNECT_TIMEOUT.as_secs());
                return Err(1);
            };
            match self.events.recv_timeout(left) {
                Ok(Event::PinNeeded { .. }) => {
                    println!("PIN needed: type the code shown on the host");
                    let mut pin = String::new();
                    if std::io::stdin().lock().read_line(&mut pin).unwrap_or(0) == 0 {
                        println!("FAIL: no PIN on stdin");
                        return Err(1);
                    }
                    self.core.submit_pin(self.id, pin.trim());
                }
                Ok(Event::Connected { info, .. }) => {
                    println!(
                        "connected in {:.0} ms: {} at {}, {}x{} @ {} fps, {:?}",
                        started.elapsed().as_secs_f64() * 1000.0,
                        info.host_name,
                        info.address,
                        info.width,
                        info.height,
                        info.fps,
                        info.codec
                    );
                    let shown = Shown {
                        kind: info.display.kind,
                        stream: (info.width, info.height, info.fps),
                        pixels: (info.display.width, info.display.height),
                        hidpi: info.display.hidpi,
                        arrangement: info.display.arrangement,
                        reason: 0,
                        message: String::new(),
                        unavailable: info.display_unavailable,
                    };
                    println!("{shown}");
                    self.shown = Some(shown);
                    return Ok(());
                }
                Ok(Event::Ended { error, .. }) => {
                    println!("FAIL: session ended before connecting: {}", error.as_deref().unwrap_or("closed"));
                    return Err(1);
                }
                Ok(_) => {}
                Err(_) => {
                    println!("FAIL: not connected after {}s", CONNECT_TIMEOUT.as_secs());
                    return Err(1);
                }
            }
        }
    }

    fn request_control(&mut self, on: bool) -> (bool, Option<String>) {
        let request = self.core.set_control(self.id, on, false);
        let deadline = Instant::now() + Duration::from_secs(5);
        let answer = |e: &Event| matches!(e, Event::Control { request: r, .. } if *r == request);
        if let Some(i) = self.pending.iter().position(answer) {
            if let Event::Control { active, message, .. } = self.pending.remove(i) {
                return (active, Some(message).filter(|m| !m.is_empty()));
            }
        }
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.events.recv_timeout(left) {
                Ok(e) if answer(&e) => {
                    let Event::Control { active, message, .. } = e else { unreachable!() };
                    return (active, Some(message).filter(|m| !m.is_empty()));
                }
                Ok(e) => self.pending.push(e),
                Err(_) => break,
            }
        }
        (false, Some("no answer from the host".into()))
    }

    /// What the host said about its cursor right after granting control (it is drawn locally
    /// by a controlling viewer): the shapes and the current state.
    fn report_cursor(&mut self) {
        let deadline = Instant::now() + Duration::from_millis(700);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.events.recv_timeout(left) {
                Ok(e) => self.pending.push(e),
                Err(_) => break,
            }
        }
        for e in std::mem::take(&mut self.pending) {
            match e {
                Event::CursorShape { id, png, width, height, hot_x, hot_y, .. } => {
                    println!("cursor shape {id}: {width}x{height} pt, hot spot ({hot_x}, {hot_y}), {} byte PNG", png.len())
                }
                Event::Cursor { state, id, .. } => println!("cursor {state} {}", id.map_or(String::new(), |i| i.to_string())),
                other => self.pending.push(other),
            }
        }
    }

    fn send(&self, msg: InputMsg) {
        self.core.send_input(self.id, msg);
    }

    fn play_script(&mut self, path: &str, display: (f64, f64)) -> Result<usize, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
        let mut n = 0;
        let mut last = (0u16, 0u16);
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            let step: Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
            self.step(&step, display, &mut last).map_err(|e| format!("line {}: {e}", i + 1))?;
            n += 1;
            // Like a person: a few ms between steps, so the host and the target app see distinct events.
            std::thread::sleep(Duration::from_millis(8));
        }
        Ok(n)
    }

    fn step(&mut self, step: &Value, display: (f64, f64), last: &mut (u16, u16)) -> Result<(), String> {
        let lab = self.lab.clone();
        // [x, y] in points, or the name of an Input Lab part (its middle), optionally with an
        // offset: "text", "scroll+20,40".
        let at = |v: &Value| -> Result<(u16, u16), String> {
            if let Some(name) = v.as_str() {
                let (name, offset) = name.split_once('+').map_or((name, (0.0, 0.0)), |(n, o)| (n, pair_of::<f64>(o, ',').unwrap_or((0.0, 0.0))));
                let (x, y, w, h) = *lab.get(name).ok_or(format!("unknown lab part {name:?} (pass --lab)"))?;
                let (px, py) = if offset == (0.0, 0.0) { (x + w / 2.0, y + h / 2.0) } else { (x + offset.0, y + offset.1) };
                return Ok((norm(px, display.0), norm(py, display.1)));
            }
            let x = v.get(0).and_then(Value::as_f64).ok_or("position needs [x, y] or a lab part")?;
            let y = v.get(1).and_then(Value::as_f64).ok_or("position needs [x, y] or a lab part")?;
            Ok((norm(x, display.0), norm(y, display.1)))
        };
        let button = step.get("button").and_then(Value::as_u64).unwrap_or(0) as u8;
        if let Some(p) = step.get("move") {
            let (x, y) = at(p)?;
            *last = (x, y);
            self.send(InputMsg::MouseMove { x, y });
        } else if let Some(p) = step.get("click") {
            let (x, y) = at(p)?;
            *last = (x, y);
            let clicks = step.get("clicks").and_then(Value::as_u64).unwrap_or(1) as u8;
            self.click(button, clicks, x, y);
        } else if let Some(p) = step.get("double").or_else(|| step.get("triple")) {
            let (x, y) = at(p)?;
            *last = (x, y);
            let times = if step.get("triple").is_some() { 3 } else { 2 };
            for clicks in 1..=times {
                self.click(button, clicks, x, y);
                std::thread::sleep(Duration::from_millis(40));
            }
        } else if let Some(p) = step.get("down").or_else(|| step.get("up")) {
            let (x, y) = at(p)?;
            *last = (x, y);
            let down = step.get("down").is_some();
            self.send(InputMsg::MouseButton { button, down, clicks: 1, x, y });
        } else if let Some(path) = step.get("drag") {
            let (from, to) = (at(&path[0])?, at(&path[1])?);
            let steps = step.get("steps").and_then(Value::as_u64).unwrap_or(12).max(1);
            self.send(InputMsg::MouseButton { button, down: true, clicks: 1, x: from.0, y: from.1 });
            for i in 1..=steps {
                let t = i as f64 / steps as f64;
                let x = (f64::from(from.0) + (f64::from(to.0) - f64::from(from.0)) * t).round() as u16;
                let y = (f64::from(from.1) + (f64::from(to.1) - f64::from(from.1)) * t).round() as u16;
                self.send(InputMsg::MouseMove { x, y });
                std::thread::sleep(Duration::from_millis(16));
            }
            self.send(InputMsg::MouseButton { button, down: false, clicks: 1, x: to.0, y: to.1 });
            *last = to;
        } else if let Some(p) = step.get("scroll") {
            let (x, y) = at(p)?;
            *last = (x, y);
            let dy = step.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
            let dx = step.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
            let steps = step.get("steps").and_then(Value::as_u64).unwrap_or(6).max(1);
            let base = ScrollInput { x, y, continuous: true, ..Default::default() };
            self.send(InputMsg::Scroll(ScrollInput { phase: 1, ..base }));
            for _ in 0..steps {
                let (py, px) = (dy / steps as f64, dx / steps as f64);
                self.send(InputMsg::Scroll(ScrollInput {
                    phase: 2,
                    pixels_y: py.round() as i32,
                    pixels_x: px.round() as i32,
                    fixed_y: (py / 10.0) as f32,
                    fixed_x: (px / 10.0) as f32,
                    lines_y: (py / 10.0).round() as i32,
                    lines_x: (px / 10.0).round() as i32,
                    ..base
                }));
                std::thread::sleep(Duration::from_millis(16));
            }
            self.send(InputMsg::Scroll(ScrollInput { phase: 4, ..base }));
        } else if let Some(p) = step.get("wheel") {
            let (x, y) = at(p)?;
            *last = (x, y);
            let lines = step.get("lines").and_then(Value::as_i64).unwrap_or(-1) as i32;
            self.send(InputMsg::Scroll(ScrollInput { x, y, lines_y: lines, fixed_y: lines as f32, pixels_y: lines * 10, ..Default::default() }));
        } else if let Some(m) = step.get("mods") {
            let names: Vec<&str> = m.as_array().ok_or("mods needs a list")?.iter().filter_map(Value::as_str).collect();
            self.send(InputMsg::Modifiers { flags: mod_flags(&names)? });
        } else if let Some(k) = step.get("key") {
            let (code, shift) = key_of(k)?;
            self.type_key(code, shift);
        } else if let Some(k) = step.get("keydown").or_else(|| step.get("keyup")) {
            let (code, _) = key_of(k)?;
            self.send(InputMsg::Key { code, down: step.get("keydown").is_some(), repeat: false });
        } else if let Some(t) = step.get("text").and_then(Value::as_str) {
            for ch in t.chars() {
                let (code, shift) = key_of(&Value::String(ch.to_string()))?;
                self.type_key(code, shift);
                std::thread::sleep(Duration::from_millis(12));
            }
        } else if let Some(k) = step.get("hold") {
            let (code, _) = key_of(k)?;
            let ms = step.get("ms").and_then(Value::as_u64).unwrap_or(1000);
            let heartbeat = step.get("heartbeat").and_then(Value::as_bool).unwrap_or(true);
            self.send(InputMsg::Key { code, down: true, repeat: false });
            let until = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < until {
                std::thread::sleep(Duration::from_millis(protocol::HEARTBEAT_INTERVAL_MS));
                if heartbeat {
                    self.send(InputMsg::Heartbeat);
                }
            }
            self.send(InputMsg::Key { code, down: false, repeat: false });
        } else if let Some(ms) = step.get("sleep").and_then(Value::as_u64) {
            std::thread::sleep(Duration::from_millis(ms));
        } else if step.get("assert_idle").is_some() {
            // Nothing may be left held on this Mac (the host runs here in same-Mac tests).
            std::thread::sleep(Duration::from_millis(300));
            let (flags, buttons) = platform_mac::inject::session_input_state();
            let held = flags & platform_mac::keys::MODIFIER_MASK & !platform_mac::keys::CAPS_LOCK;
            if held != 0 || !buttons.is_empty() {
                return Err(format!("left held: modifier flags {held:#x}, mouse buttons {buttons:?}"));
            }
            println!("idle: no modifiers or mouse buttons held (flags {flags:#x})");
        } else if let Some(depth) = step.get("relayed").and_then(Value::as_u64) {
            self.send(InputMsg::Relayed { depth: depth.min(255) as u8 });
        } else if step.get("release").is_some() {
            self.send(InputMsg::ReleaseAll);
        } else if let Some(on) = step.get("control").and_then(Value::as_bool) {
            let (active, reason) = self.request_control(on);
            if active != on {
                return Err(format!("control {on} answered {active}: {reason:?}"));
            }
        } else if step.get("disconnect").is_some() {
            self.core.disconnect(self.id);
            std::thread::sleep(Duration::from_millis(300));
        } else if let Some(msgs) = gesture_step(step, &at)? {
            for (i, msg) in msgs.into_iter().enumerate() {
                if i > 0 {
                    std::thread::sleep(GESTURE_UPDATE_INTERVAL);
                }
                self.send(msg);
            }
        } else {
            return Err(format!("unknown step {step}"));
        }
        Ok(())
    }

    fn click(&self, button: u8, clicks: u8, x: u16, y: u16) {
        self.send(InputMsg::MouseButton { button, down: true, clicks, x, y });
        std::thread::sleep(Duration::from_millis(20));
        self.send(InputMsg::MouseButton { button, down: false, clicks, x, y });
    }

    fn type_key(&self, code: u16, shift: bool) {
        if shift {
            self.send(InputMsg::Modifiers { flags: 0x0002_0002 });
        }
        self.send(InputMsg::Key { code, down: true, repeat: false });
        std::thread::sleep(Duration::from_millis(10));
        self.send(InputMsg::Key { code, down: false, repeat: false });
        if shift {
            self.send(InputMsg::Modifiers { flags: 0 });
        }
    }

    /// Clicks the middle of `rect` (points) `n` times and times each click until the decoded
    /// video shows the region change brightness. Clicks before the region was ever decoded are
    /// warm-ups: they only establish what it looks like.
    fn input_latency(&mut self, n: usize, rect: (f64, f64, f64, f64), display: (f64, f64)) -> i32 {
        let (x, y, w, h) = rect;
        // Look at the middle of the region, away from its edges.
        let region = ((x + w * 0.25) / display.0, (y + h * 0.25) / display.1, (x + w * 0.75) / display.0, (y + h * 0.75) / display.1);
        let samples: Arc<Mutex<Vec<(u64, f64, Option<u64>)>>> = Arc::default();
        let sink = samples.clone();
        let parts = Mutex::new(RegionParts::default());
        self.core.set_frame_probe(
            self.id,
            Some(Box::new(move |f| {
                let mut parts = parts.lock().unwrap();
                if let Some(luma) = parts.update(f, region) {
                    sink.lock().unwrap().push((f.timing.decoded_us, luma, f.timing.capture_local_us));
                }
            })),
        );
        let (cx, cy) = (norm(x + w / 2.0, display.0), norm(y + h / 2.0, display.1));
        // Park the cursor on the target, and get a first frame to compare against.
        self.send(InputMsg::MouseMove { x: cx, y: cy });
        std::thread::sleep(Duration::from_millis(500));
        let mut totals = Vec::new();
        let mut to_capture = Vec::new();
        let (mut clicks, mut warm_ups) = (0, 0);
        while clicks < n && warm_ups < MAX_WARM_UPS {
            // Without a picture of the region from before the click, any frame after it would look
            // like the change: such a click only gets one, and isn't counted.
            let before = samples.lock().unwrap().last().map(|s| s.1);
            let t0 = clock::now_us();
            // Distinct clicks, not a double click: the app sees each as clickCount 1.
            self.send(InputMsg::MouseButton { button: 0, down: true, clicks: 1, x: cx, y: cy });
            self.send(InputMsg::MouseButton { button: 0, down: false, clicks: 1, x: cx, y: cy });
            let deadline = Instant::now() + Duration::from_secs(1);
            let mut seen = None;
            while Instant::now() < deadline && seen.is_none() {
                std::thread::sleep(Duration::from_micros(200));
                let s = samples.lock().unwrap();
                seen = s.iter().rev().take_while(|f| f.0 > t0).find(|f| before.is_none_or(|b| (f.1 - b).abs() > 60.0)).copied();
            }
            match (before, seen) {
                (None, seen) => {
                    warm_ups += 1;
                    match seen {
                        Some((_, luma, _)) => println!("warm-up: the region's luma is {luma:.0} (no picture of it before this click: not counted)"),
                        None => println!("warm-up: no picture of the region within 1 s"),
                    }
                }
                (Some(before), Some((decoded_us, luma, captured))) => {
                    clicks += 1;
                    let total = decoded_us.saturating_sub(t0) as f64 / 1000.0;
                    let capture = captured.map(|c| c.saturating_sub(t0) as f64 / 1000.0);
                    println!(
                        "{clicks:>3}: {total:>6.1} ms click → decoded   (captured after {} ms, luma {before:.0} → {luma:.0})",
                        capture.map_or("-".into(), |c| format!("{c:.1}")),
                    );
                    totals.push(total);
                    if let Some(c) = capture {
                        to_capture.push(c);
                    }
                }
                (Some(_), None) => {
                    clicks += 1;
                    println!("{clicks:>3}: no change seen within 1 s");
                }
            }
            std::thread::sleep(Duration::from_millis(350));
        }
        self.core.set_frame_probe(self.id, None);
        if totals.is_empty() {
            println!("FAIL: the region never changed; is the target under --rect and frontmost?");
            return 1;
        }
        let stats = self.core.session_stats(self.id);
        println!(
            "input → photon (decoded): p50 {:.1} ms, p90 {:.1} ms, max {:.1} ms over {} clicks; input → captured p50 {:.1} ms; input → injected {} ms (network {} + host {})",
            percentile(&totals, 0.5),
            percentile(&totals, 0.9),
            percentile(&totals, 1.0),
            totals.len(),
            percentile(&to_capture, 0.5),
            stats.as_ref().and_then(|s| s.input_ms).map_or("-".into(), |v| format!("{v:.2}")),
            stats.as_ref().and_then(|s| s.input_network_ms).map_or("-".into(), |v| format!("{v:.2}")),
            stats.as_ref().and_then(|s| s.input_inject_ms).map_or("-".into(), |v| format!("{v:.2}"))
        );
        if totals.len() == n { 0 } else { 1 }
    }

    fn watch_video(&mut self, args: &Args) -> i32 {
        let ms = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.1}"));
        if args.virtual_displays.is_empty() {
            let watch = self.watch.clone();
            self.core.set_frame_probe(self.id, Some(Box::new(move |f| watch.lock().unwrap().tile(f))));
        }
        {
            // This watch's own numbers: an earlier step's were reported with it.
            let mut watch = self.watch.lock().unwrap();
            watch.updates.take_all();
            watch.frames = SourceFrames::default();
            if matches!(args.barcode, Some(Barcode::Rect(_))) {
                watch.strip = None; // placed on the display shown now
            }
            if args.barcode.is_some() && watch.strip.is_none() {
                watch.latest.get_or_insert_default();
            }
        }
        self.find_strip(args);
        // Per-second deltas of the source frames' counts.
        let mut before = (0, 0, 0, 0);
        let streaming = Instant::now();
        let mut ended = None;
        for second in 1..=args.seconds {
            let deadline = streaming + Duration::from_secs(second);
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                // The source starts after the probe: look for its strip often until it's there.
                let looking = matches!(args.barcode, Some(Barcode::Log(_))) && self.watch.lock().unwrap().strip.is_none();
                match self.events.recv_timeout(if looking { left.min(STRIP_LOOK_INTERVAL) } else { left }) {
                    Ok(Event::Ended { error, .. }) => {
                        ended = Some(error.unwrap_or_else(|| "closed by host".into()));
                        break;
                    }
                    Ok(_) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => self.find_strip(args),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            if ended.is_some() {
                break;
            }
            let Some(s) = self.core.session_stats(self.id) else { break };
            let pipeline = match (s.capture_ms, s.encode_ms, s.network_ms, s.decode_ms) {
                (Some(c), Some(e), Some(n), Some(d)) => Some(c + e + n + d),
                _ => None,
            };
            let whole = self.watch.lock().unwrap().updates.take();
            let whole = if whole.is_empty() {
                "-".to_string()
            } else {
                format!("{:.1}/{:.1}", percentile(&whole, 0.5), percentile(&whole, 0.9))
            };
            println!(
                "{second:>3}s  {:>3.0} fps  {:>6.1} Mbps  update capture→decoded p50/p90 {whole} ms  per tile {:>5} ms (capture {} + encode {} + network {} + decode {})  rtt {} ms  decoded {}  lost {}  keyframe requests {}",
                s.fps,
                s.mbps,
                ms(pipeline),
                ms(s.capture_ms),
                ms(s.encode_ms),
                ms(s.network_ms),
                ms(s.decode_ms),
                ms(s.rtt_ms),
                s.frames_decoded,
                s.frames_lost,
                s.keyframe_requests
            );
            self.report_second(&mut before);
        }

        self.report_frames(args);
        let decoded = self.core.session_stats(self.id).map_or(0, |s| s.frames_decoded);
        match ended {
            Some(reason) => {
                println!("FAIL: session ended after {:.1}s: {reason} ({decoded} frames decoded)", streaming.elapsed().as_secs_f64());
                1
            }
            None if decoded == 0 => {
                println!("FAIL: connected, but no frames were decoded");
                1
            }
            None => {
                println!("PASS: {decoded} frames decoded in {}s", args.seconds);
                0
            }
        }
    }

    /// Puts Frame Source's strip in place once its place is known (at once for `--barcode`; from
    /// the source's log for `--barcode-log`, which may come later or change).
    fn find_strip(&mut self, args: &Args) {
        let place = match &args.barcode {
            None => return,
            Some(Barcode::Rect(rect)) => {
                if self.watch.lock().unwrap().strip.is_some() {
                    return;
                }
                (*rect, self.display_points(args), 16)
            }
            Some(Barcode::Log(_)) => {
                let Some(source) = &mut self.source else { return };
                source.poll();
                let Some(place) = source.strip else { return };
                place
            }
        };
        let mut watch = self.watch.lock().unwrap();
        if watch.strip.as_ref().is_some_and(|s| (s.rect, s.display, s.squares) == place) {
            return;
        }
        let ((x, y, w, h), (dw, dh), squares) = place;
        println!("frame numbers: {squares} squares at {x},{y} ({w}x{h} pt) on a {dw}x{dh} pt display");
        watch.place_strip(Strip::new(place.0, place.1, place.2));
    }

    /// The captured display's size in points, for `--barcode`: `--display`; else the display
    /// shown, its pixels over `--barcode-scale` (default 2 for a Retina virtual display, 1 for
    /// another, and this Mac's main display's own).
    fn display_points(&self, args: &Args) -> (f64, f64) {
        if let Some(display) = args.display {
            return display;
        }
        match &self.shown {
            Some(shown) if shown.kind == "virtual" => {
                let scale = args.barcode_scale.unwrap_or(if shown.hidpi { 2.0 } else { 1.0 });
                (f64::from(shown.pixels.0) / scale, f64::from(shown.pixels.1) / scale)
            }
            _ => match args.barcode_scale {
                Some(scale) => {
                    let main = platform_mac::capture::main_display_bounds();
                    (f64::from(main.width) / scale, f64::from(main.height) / scale)
                }
                None => main_display_points(),
            },
        }
    }

    /// The source frames of the last second: how many showed, were skipped or late, and source on
    /// screen → decoded for those in the source's log by now. `before`: the counts at the last
    /// report (frames shown, skipped, late, unread).
    fn report_second(&mut self, before: &mut (usize, u64, u64, u64)) {
        if let Some(source) = &mut self.source {
            source.poll();
        }
        let watch = self.watch.lock().unwrap();
        if watch.strip.is_none() {
            return;
        }
        let f = &watch.frames;
        let new = &f.shown[before.0.min(f.shown.len())..];
        let delays = self.source.as_ref().map(|s| s.delays(new).0).unwrap_or_default();
        let delays = if delays.is_empty() {
            "-".to_string()
        } else {
            format!("{:.1}/{:.1}/{:.1}", percentile(&delays, 0.5), percentile(&delays, 0.95), percentile(&delays, 0.99))
        };
        let basis = self.source.as_ref().map_or("presented", SourceLog::basis);
        println!(
            "      source frames {:>3} new (n {}), {} skipped, {} late, {} unread  {basis}→decoded p50/p95/p99 {delays} ms",
            new.len(),
            f.newest.map_or("-".into(), |n| n.to_string()),
            f.skipped - before.1,
            f.late - before.2,
            f.unread - before.3,
        );
        *before = (f.shown.len(), f.skipped, f.late, f.unread);
    }

    /// The whole watch: updates' latency, and with the strip, the source frames that showed.
    fn report_frames(&mut self, args: &Args) {
        if let Some(source) = &mut self.source {
            source.poll();
        }
        let mut watch = self.watch.lock().unwrap();
        if let Some(trace) = &mut watch.trace {
            let _ = trace.flush();
        }
        let all = watch.updates.take_all();
        if !all.is_empty() {
            println!(
                "updates: {} finished, capture→decoded p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms",
                all.len(),
                percentile(&all, 0.5),
                percentile(&all, 0.95),
                percentile(&all, 0.99)
            );
        }
        if args.barcode.is_none() {
            return;
        }
        let f = &watch.frames;
        let (Some(&(first, _, first_decoded)), Some(&(_, _, last_decoded)), Some(newest)) = (f.shown.first(), f.shown.last(), f.newest) else {
            println!("source frames: none read (is Frame Source drawing on the display shown, and is the strip's place right?)");
            return;
        };
        let span = last_decoded.saturating_sub(first_decoded) as f64 / 1e6;
        println!(
            "source frames: {} shown, {:.1} fps over {span:.1} s; {} skipped, {} late, {} unread updates{}; n {first}..{newest}",
            f.shown.len(),
            if span > 0.0 { (f.shown.len() - 1) as f64 / span } else { 0.0 },
            f.skipped,
            f.late,
            f.unread,
            if f.restarts > 0 { format!(", counting restarted {} times", f.restarts) } else { String::new() },
        );
        let Some(source) = &self.source else { return };
        let (decoded, captured) = source.delays(&f.shown);
        if !decoded.is_empty() {
            println!(
                "  source {} → decoded p50 {:.1} ms, p95 {:.1} ms, p99 {:.1} ms over {} frames; → captured p50 {:.1} ms{}",
                source.basis(),
                percentile(&decoded, 0.5),
                percentile(&decoded, 0.95),
                percentile(&decoded, 0.99),
                decoded.len(),
                percentile(&captured, 0.5),
                if source.presents { "" } else { " (its screen reports no presentation times)" }
            );
        }
        // What the source put on screen in that range and never showed here: skipped, or lost.
        let seen: std::collections::HashSet<u64> = f.shown.iter().map(|s| s.0).collect();
        let on_screen: Vec<u64> = (first..=newest).filter(|&n| source.on_screen(n).is_some()).collect();
        println!(
            "  the source put {} frames on screen in that range ({} drawn in all), {} of them never showed here",
            on_screen.len(),
            source.frames.len(),
            on_screen.iter().filter(|n| !seen.contains(n)).count()
        );
    }
}

/// How often to look for Frame Source's strip in its log until it's there.
const STRIP_LOOK_INTERVAL: Duration = Duration::from_millis(100);

/// The mean luma of a region of the stream, which may span several tiles: each tile's part is
/// measured when that tile is decoded, and the region's mean is the area-weighted mean of the
/// latest part from every tile it covers. A full frame ([`FULL_FRAME_TILE`]) covers the whole
/// region: it is kept, and each tile decoded after it replaces its part of it.
#[derive(Default)]
struct RegionParts {
    stream: (u32, u32),
    /// The newest full frame, where it is (the whole stream), its mean over the region, and its
    /// update.
    full: Option<(Picture, TileRect, f64, u32)>,
    /// Tile index → its latest part, since the full frame if there is one.
    parts: HashMap<u8, Part>,
}

/// A decoded picture kept for later (the probe runs on the decoders' threads).
struct Picture(CFRetained<CVPixelBuffer>);

// SAFETY: CVPixelBuffer is a thread-safe, reference-counted CoreFoundation object.
unsafe impl Send for Picture {}

struct Part {
    /// Its update: decoders finish in any order, so newer ones win, not later ones.
    update: u32,
    /// The tile it came from.
    tile: protocol::TileRect,
    /// Where it is (x0, y0, x1, y1 in stream pixels).
    rect: (f64, f64, f64, f64),
    luma: f64,
    /// Pixels.
    area: f64,
    /// The full frame's mean over the same pixels (0 without one).
    under: f64,
}

/// Mean luma of `pixel_buffer`, a picture of `tile`, over `part` (x0, y0, x1, y1 in stream pixels).
fn part_luma(pixel_buffer: &CVPixelBuffer, tile: &TileRect, part: (f64, f64, f64, f64)) -> Option<f64> {
    let (tx, ty, tw, th) = (f64::from(tile.x), f64::from(tile.y), f64::from(tile.width), f64::from(tile.height));
    platform_mac::gpu::mean_luma(pixel_buffer, (part.0 - tx) / tw, (part.1 - ty) / th, (part.2 - tx) / tw, (part.3 - ty) / th)
}

impl RegionParts {
    /// Takes in a decoded tile; returns the region's mean once every part of it has been seen
    /// and this tile is one of them. `region` is (x0, y0, x1, y1), normalized to the stream.
    fn update(&mut self, f: &ProbeFrame<'_>, region: (f64, f64, f64, f64)) -> Option<f64> {
        if f.stream != self.stream {
            self.stream = f.stream;
            self.full = None;
            self.parts.clear();
        }
        let (sw, sh) = (f64::from(f.stream.0), f64::from(f.stream.1));
        let (tx, ty, tw, th) = (f64::from(f.tile.x), f64::from(f.tile.y), f64::from(f.tile.width), f64::from(f.tile.height));
        let clamp = |v: f64, n: f64| v.clamp(0.0, 1.0) * n;
        let (x0, y0, x1, y1) = (clamp(region.0, sw), clamp(region.1, sh), clamp(region.2, sw), clamp(region.3, sh));
        // The part of the region in this tile, in stream pixels.
        let part = (x0.max(tx), y0.max(ty), x1.min(tx + tw), y1.min(ty + th));
        if part.0 >= part.2 || part.1 >= part.3 {
            return None;
        }
        let luma = part_luma(f.pixel_buffer, &f.tile, part)?;
        let area = (part.2 - part.0) * (part.3 - part.1);
        let whole = (x1 - x0) * (y1 - y0);
        if f.tile.index == FULL_FRAME_TILE {
            if self.full.as_ref().is_some_and(|full| !is_newer(f.update, full.3)) {
                return None; // a newer full frame decoded first
            }
            // Parts of older updates show what it shows now; newer ones stay over it, in place of
            // what it has there.
            self.parts.retain(|_, p| is_newer(p.update, f.update));
            for p in self.parts.values_mut() {
                p.under = part_luma(f.pixel_buffer, &f.tile, p.rect)?;
            }
            self.full = Some((Picture(f.pixel_buffer.retain()), f.tile, luma, f.update));
            return self.mean(whole);
        }
        if self.parts.get(&f.tile.index).is_some_and(|p| p.tile != f.tile) {
            // The host laid the tiles out anew (same size, fewer tiles): the old parts are gone.
            self.parts.clear();
        }
        if self.full.as_ref().is_some_and(|full| !is_newer(f.update, full.3))
            || self.parts.get(&f.tile.index).is_some_and(|p| !is_newer(f.update, p.update))
        {
            return None; // something newer already shows there
        }
        let under = match &self.full {
            Some((full, rect, _, _)) => part_luma(&full.0, rect, part)?,
            None => 0.0,
        };
        self.parts.insert(f.tile.index, Part { update: f.update, tile: f.tile, rect: part, luma, area, under });
        self.mean(whole)
    }

    /// The region's mean: the full frame with the newer parts in place of what it had there; or,
    /// without one, the parts once they cover the region.
    fn mean(&self, whole: f64) -> Option<f64> {
        if let Some((_, _, full_luma, _)) = self.full {
            // The full frame, with the parts decoded since then in place of what it had there.
            return Some((full_luma * whole + self.parts.values().map(|p| (p.luma - p.under) * p.area).sum::<f64>()) / whole);
        }
        let area: f64 = self.parts.values().map(|p| p.area).sum();
        if area < whole * 0.999 {
            return None; // some tile of the region hasn't been decoded yet
        }
        Some(self.parts.values().map(|p| p.luma * p.area).sum::<f64>() / area)
    }
}

/// Clicks at most to get a first picture of the input-latency region.
const MAX_WARM_UPS: usize = 3;

/// Between a gesture's updates, as from a trackpad.
const GESTURE_UPDATE_INTERVAL: Duration = Duration::from_millis(16);

/// The messages of a trackpad gesture step (see the doc at the top), or None if `step` isn't
/// one. `at` turns a step's position into the wire's.
fn gesture_step(step: &Value, at: &dyn Fn(&Value) -> Result<(u16, u16), String>) -> Result<Option<Vec<InputMsg>>, String> {
    use GesturePhase::{Began, Cancelled, Changed, Ended};
    let steps = step.get("steps").and_then(Value::as_u64).unwrap_or(8).max(1);
    let number = |key: &str, default: f64| step.get(key).and_then(Value::as_f64).unwrap_or(default);
    let msgs = if let Some(p) = step.get("pinch") {
        let (x, y) = at(p)?;
        let amount = number("amount", 0.5);
        if amount <= -1.0 {
            return Err("pinch amount must be above -1 (it scales by 1 + amount)".into());
        }
        // Magnifications compose: each step scales by the same factor.
        let delta = ((1.0 + amount).powf(1.0 / steps as f64) - 1.0) as f32;
        let pinch = |phase, delta| InputMsg::Gesture(GestureInput::Magnify { x, y, phase, delta });
        phased(pinch(Began, 0.0), (0..steps).map(|_| pinch(Changed, delta)), Some(pinch(Ended, 0.0)))
    } else if let Some(p) = step.get("rotate") {
        let (x, y) = at(p)?;
        let per_step = (number("degrees", 45.0) / steps as f64) as f32;
        let turn = |phase, degrees| InputMsg::Gesture(GestureInput::Rotate { x, y, phase, degrees });
        phased(turn(Began, 0.0), (0..steps).map(|_| turn(Changed, per_step)), Some(turn(Ended, 0.0)))
    } else if let Some(p) = step.get("smart_magnify") {
        let (x, y) = at(p)?;
        vec![InputMsg::Gesture(GestureInput::SmartMagnify { x, y })]
    } else if let Some(p) = step.get("swipe") {
        let (x, y) = at(p)?;
        let direction = |key| step.get(key).and_then(Value::as_i64).unwrap_or(0).signum() as i8;
        let (dx, dy) = (direction("dx"), direction("dy"));
        if (dx, dy) == (0, 0) {
            return Err("swipe needs dx or dy".into());
        }
        vec![InputMsg::Gesture(GestureInput::NavigationSwipe { x, y, dx, dy })]
    } else if let Some(axis) = step.get("dock") {
        let axis = match axis.as_str() {
            Some("horizontal") => DockAxis::Horizontal,
            Some("vertical") => DockAxis::Vertical,
            Some("pinch") => DockAxis::Pinch,
            _ => return Err(format!("dock needs horizontal, vertical or pinch, not {axis}")),
        };
        let to = number("to", 1.0);
        let velocity = number("velocity", 0.0) as f32;
        let swipe = |phase, progress: f64, velocity: f32| {
            // A horizontal swipe's exit velocity is along x; the others' along y.
            let (velocity_x, velocity_y) = if axis == DockAxis::Horizontal { (velocity, 0.0) } else { (0.0, velocity) };
            InputMsg::Gesture(GestureInput::DockSwipe { axis, phase, progress: progress as f32, velocity_x, velocity_y, inverted: false })
        };
        let end = match step.get("end").and_then(Value::as_str).unwrap_or("ended") {
            "ended" => Some(swipe(Ended, to, velocity)),
            "cancelled" => Some(swipe(Cancelled, to, 0.0)),
            "none" => None,
            other => return Err(format!("dock end is ended, cancelled or none, not {other:?}")),
        };
        phased(swipe(Began, 0.0, 0.0), (1..=steps).map(|i| swipe(Changed, to * i as f64 / steps as f64, 0.0)), end)
    } else if let Some(name) = step.get("system") {
        let action = match name.as_str() {
            Some("mission_control") => SystemAction::MISSION_CONTROL,
            Some("app_expose") => SystemAction::APP_EXPOSE,
            Some("show_desktop") => SystemAction::SHOW_DESKTOP,
            Some("launchpad") => SystemAction::LAUNCHPAD,
            Some("previous_space") => SystemAction::PREVIOUS_SPACE,
            Some("next_space") => SystemAction::NEXT_SPACE,
            _ => return Err(format!("unknown system action {name}")),
        };
        vec![InputMsg::System(action)]
    } else {
        return Ok(None);
    };
    Ok(Some(msgs))
}

fn phased(began: InputMsg, changes: impl Iterator<Item = InputMsg>, end: Option<InputMsg>) -> Vec<InputMsg> {
    std::iter::once(began).chain(changes).chain(end).collect()
}

fn norm(v: f64, size: f64) -> u16 {
    ((v / size).clamp(0.0, 1.0) * f64::from(POS_MAX)).round() as u16
}

fn percentile(v: &[f64], q: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    s[((s.len() - 1) as f64 * q).round() as usize]
}

/// This Mac's main display in points: right when the host runs on the same Mac.
fn main_display_points() -> (f64, f64) {
    let b = platform_mac::inject::Bounds::of_display(platform_mac::capture::main_display_bounds().id);
    (b.width, b.height)
}

fn mod_flags(names: &[&str]) -> Result<u32, String> {
    names.iter().try_fold(0u32, |acc, n| {
        let bits = match *n {
            "lcmd" | "cmd" => 0x10_0008,
            "rcmd" => 0x10_0010,
            "lshift" | "shift" => 0x2_0002,
            "rshift" => 0x2_0004,
            "lopt" | "opt" => 0x8_0020,
            "ropt" => 0x8_0040,
            "lctrl" | "ctrl" => 0x4_0001,
            "rctrl" => 0x4_2000,
            "caps" => 0x1_0000,
            "fn" => 0x80_0000,
            other => return Err(format!("unknown modifier {other}")),
        };
        Ok(acc | bits)
    })
}

/// A key as a virtual key code (ANSI layout) and whether it needs shift, from a number or a
/// one-character string (letters, digits, punctuation, space, and "return", "tab", "escape"...).
fn key_of(v: &Value) -> Result<(u16, bool), String> {
    if let Some(code) = v.as_u64() {
        return Ok((code as u16, false));
    }
    let s = v.as_str().ok_or("key needs a code or a character")?;
    let named = match s {
        "return" => Some(36),
        "tab" => Some(48),
        "space" => Some(49),
        "delete" => Some(51),
        "escape" => Some(53),
        "left" => Some(123),
        "right" => Some(124),
        "down" => Some(125),
        "up" => Some(126),
        "home" => Some(115),
        "end" => Some(119),
        "f1" => Some(122),
        _ => None,
    };
    if let Some(code) = named {
        return Ok((code, false));
    }
    let mut chars = s.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else { return Err(format!("unknown key {s:?}")) };
    const UNSHIFTED: &str = "asdfhgzxcv\0bqweryt123465=97-80]ou[ip\0lj'k;\\,/nm.\0 `";
    const SHIFTED: &str = "ASDFHGZXCV\0BQWERYT!@#$^%+(&_*)}OU{IP\0LJ\"K:|<?NM>\0 ~";
    if c == '\n' {
        return Ok((36, false));
    }
    if let Some(i) = UNSHIFTED.chars().position(|u| u == c && u != '\0') {
        return Ok((i as u16, false));
    }
    if let Some(i) = SHIFTED.chars().position(|u| u == c && u != '\0') {
        return Ok((i as u16, true));
    }
    Err(format!("no key for {c:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_key_codes() {
        assert_eq!(key_of(&Value::from("a")).unwrap(), (0, false));
        assert_eq!(key_of(&Value::from("A")).unwrap(), (0, true));
        assert_eq!(key_of(&Value::from("q")).unwrap(), (12, false));
        assert_eq!(key_of(&Value::from("1")).unwrap(), (18, false));
        assert_eq!(key_of(&Value::from("!")).unwrap(), (18, true));
        assert_eq!(key_of(&Value::from("5")).unwrap(), (23, false));
        assert_eq!(key_of(&Value::from("=")).unwrap(), (24, false));
        assert_eq!(key_of(&Value::from("0")).unwrap(), (29, false));
        assert_eq!(key_of(&Value::from("l")).unwrap(), (37, false));
        assert_eq!(key_of(&Value::from(",")).unwrap(), (43, false));
        assert_eq!(key_of(&Value::from(".")).unwrap(), (47, false));
        assert_eq!(key_of(&Value::from(" ")).unwrap(), (49, false));
        assert_eq!(key_of(&Value::from("`")).unwrap(), (50, false));
        assert_eq!(key_of(&Value::from("~")).unwrap(), (50, true));
        assert_eq!(key_of(&Value::from("?")).unwrap(), (44, true));
        assert_eq!(key_of(&Value::from(36)).unwrap(), (36, false));
    }

    fn gesture(step: &str) -> Result<Option<Vec<InputMsg>>, String> {
        gesture_step(&serde_json::from_str(step).unwrap(), &|_| Ok((100, 200)))
    }

    fn phases(msgs: &[InputMsg]) -> Vec<GesturePhase> {
        msgs.iter().filter_map(|m| if let InputMsg::Gesture(g) = m { g.phase() } else { None }).collect()
    }

    #[test]
    fn pinch_and_rotate_steps_add_up_to_the_whole_gesture() {
        use GesturePhase::*;
        let pinch = gesture(r#"{"pinch":"patch","amount":0.5,"steps":8}"#).unwrap().unwrap();
        assert_eq!(phases(&pinch), [[Began].as_slice(), &[Changed; 8], &[Ended]].concat());
        let scale: f64 = pinch.iter().map(|m| if let InputMsg::Gesture(GestureInput::Magnify { delta, .. }) = m { 1.0 + f64::from(*delta) } else { 1.0 }).product();
        assert!((scale - 1.5).abs() < 1e-4, "{scale}");
        assert!(matches!(pinch[3], InputMsg::Gesture(GestureInput::Magnify { x: 100, y: 200, .. })));
        let turn = gesture(r#"{"rotate":"patch","degrees":45,"steps":9}"#).unwrap().unwrap();
        let degrees: f32 = turn.iter().map(|m| if let InputMsg::Gesture(GestureInput::Rotate { degrees, .. }) = m { *degrees } else { 0.0 }).sum();
        assert!((degrees - 45.0).abs() < 1e-3, "{degrees}");
        assert!(gesture(r#"{"pinch":"patch","amount":-1}"#).is_err());
    }

    #[test]
    fn dock_steps_end_as_asked() {
        use GesturePhase::*;
        let swipe = gesture(r#"{"dock":"horizontal","to":-1.0,"steps":4,"velocity":-3,"end":"ended"}"#).unwrap().unwrap();
        assert_eq!(phases(&swipe), [Began, Changed, Changed, Changed, Changed, Ended]);
        let InputMsg::Gesture(GestureInput::DockSwipe { axis, progress, velocity_x, velocity_y, .. }) = swipe[5] else { panic!() };
        assert_eq!((axis, progress, velocity_x, velocity_y), (DockAxis::Horizontal, -1.0, -3.0, 0.0));
        assert!(matches!(swipe[2], InputMsg::Gesture(GestureInput::DockSwipe { progress: -0.5, velocity_x: 0.0, .. })));
        let hanging = gesture(r#"{"dock":"vertical","to":0.4,"steps":5,"end":"none"}"#).unwrap().unwrap();
        assert_eq!(phases(&hanging).last(), Some(&Changed));
        let cancelled = gesture(r#"{"dock":"pinch","velocity":2,"end":"cancelled"}"#).unwrap().unwrap();
        assert!(matches!(cancelled.last(), Some(InputMsg::Gesture(GestureInput::DockSwipe { phase: Cancelled, velocity_y: 0.0, .. }))));
        assert!(gesture(r#"{"dock":"diagonal"}"#).is_err());
        assert!(gesture(r#"{"dock":"vertical","end":"later"}"#).is_err());
    }

    #[test]
    fn taps_actions_and_other_steps() {
        assert_eq!(gesture(r#"{"smart_magnify":"patch"}"#).unwrap().unwrap(), [InputMsg::Gesture(GestureInput::SmartMagnify { x: 100, y: 200 })]);
        let swipe = gesture(r#"{"swipe":"patch","dx":-3}"#).unwrap().unwrap();
        assert_eq!(swipe, [InputMsg::Gesture(GestureInput::NavigationSwipe { x: 100, y: 200, dx: -1, dy: 0 })]);
        assert!(gesture(r#"{"swipe":"patch"}"#).is_err());
        assert_eq!(gesture(r#"{"system":"next_space"}"#).unwrap().unwrap(), [InputMsg::System(SystemAction::NEXT_SPACE)]);
        assert!(gesture(r#"{"system":"reboot"}"#).is_err());
        assert_eq!(gesture(r#"{"key":"a"}"#).unwrap(), None);
    }

    /// A `width`×`height` NV12 picture of one luma value.
    fn flat_frame(width: usize, height: usize, luma: u8) -> platform_mac::CFRetained<platform_mac::CVPixelBuffer> {
        use objc2_core_video::{
            CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane,
            CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
        };
        let mut raw = std::ptr::null_mut();
        let status = unsafe { CVPixelBufferCreate(None, width, height, u32::from_be_bytes(*b"420f"), None, std::ptr::NonNull::from(&mut raw)) };
        assert_eq!(status, 0);
        let pb = unsafe { platform_mac::CFRetained::from_raw(std::ptr::NonNull::new(raw).unwrap()) };
        unsafe {
            CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
            for plane in 0..2 {
                let base = CVPixelBufferGetBaseAddressOfPlane(&pb, plane) as *mut u8;
                let len = CVPixelBufferGetBytesPerRowOfPlane(&pb, plane) * CVPixelBufferGetHeightOfPlane(&pb, plane);
                std::ptr::write_bytes(base, if plane == 0 { luma } else { 128 }, len);
            }
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
        }
        pb
    }

    #[test]
    fn region_luma_combines_the_tiles_it_spans() {
        let tile = |index: u8, y: u32| protocol::TileRect { index, x: 0, y, width: 200, height: 100 };
        let mut parts = RegionParts::default();
        let mut update = 0;
        let mut luma = |pb: &platform_mac::CVPixelBuffer, tile, stream, region| {
            update += 1;
            parts.update(&ProbeFrame { pixel_buffer: pb, tile, stream, update, update_mask: 0, timing: Default::default() }, region)
        };
        let near = |got: Option<f64>, want: f64| got.is_some_and(|v| (v - want).abs() < 1e-9);
        // Rows 75..225 of 300: 25 in tile 0, 100 in tile 1, 25 in tile 2.
        let (stream, region) = ((200, 300), (0.25, 0.25, 0.75, 0.75));
        let (black, white, grey) = (flat_frame(200, 100, 0), flat_frame(200, 100, 200), flat_frame(200, 100, 100));
        assert_eq!(luma(&black, tile(0, 0), stream, region), None, "tiles 1 and 2 not seen yet");
        assert_eq!(luma(&white, tile(1, 100), stream, region), None);
        assert!(near(luma(&black, tile(2, 200), stream, region), 200.0 * 100.0 / 150.0));
        assert!(near(luma(&grey, tile(0, 0), stream, region), (100.0 * 25.0 + 200.0 * 100.0) / 150.0), "the newest part of each tile counts");
        let elsewhere = protocol::TileRect { index: 3, x: 0, y: 0, width: 40, height: 40 };
        assert_eq!(luma(&black, elsewhere, stream, region), None, "a tile outside the region");
        // A stream of another size starts over: rows 100..200 of 400 are all in tile 1.
        assert!(near(luma(&white, tile(1, 100), (200, 400), (0.0, 0.25, 1.0, 0.5)), 200.0));
    }

    #[test]
    fn region_luma_takes_tiles_over_a_full_frame() {
        let tile = |index: u8, y: u32| protocol::TileRect { index, x: 0, y, width: 200, height: 100 };
        let full = protocol::TileRect { index: FULL_FRAME_TILE, x: 0, y: 0, width: 200, height: 300 };
        let mut parts = RegionParts::default();
        let mut update = 0;
        let mut luma = |pb: &platform_mac::CVPixelBuffer, tile, region| {
            update += 1;
            parts.update(&ProbeFrame { pixel_buffer: pb, tile, stream: (200, 300), update, update_mask: 0, timing: Default::default() }, region)
        };
        let near = |got: Option<f64>, want: f64| got.is_some_and(|v| (v - want).abs() < 1e-9);
        // Rows 75..225 of 300: 25 in tile 0, 100 in tile 1, 25 in tile 2.
        let region = (0.25, 0.25, 0.75, 0.75);
        let (black, white) = (flat_frame(200, 100, 0), flat_frame(200, 100, 200));
        assert_eq!(luma(&black, tile(0, 0), region), None);
        // A full frame covers the whole region at once.
        assert!(near(luma(&flat_frame(200, 300, 100), full, region), 100.0));
        // A tile decoded after it shows its part instead.
        assert!(near(luma(&white, tile(1, 100), region), (100.0 * 50.0 + 200.0 * 100.0) / 150.0));
        assert!(near(luma(&black, tile(2, 200), region), (100.0 * 25.0 + 200.0 * 100.0) / 150.0));
        // A newer full frame replaces them all.
        assert!(near(luma(&flat_frame(200, 300, 50), full, region), 50.0));
        assert!(near(luma(&black, tile(0, 0), region), 50.0 * 125.0 / 150.0));
    }

    /// Decoders finish in any order: a full frame of an older update decoded after a newer tile
    /// leaves that tile's part in place.
    #[test]
    fn region_luma_follows_updates_not_decode_order() {
        let tile = protocol::TileRect { index: 1, x: 0, y: 100, width: 200, height: 100 };
        let full = protocol::TileRect { index: FULL_FRAME_TILE, x: 0, y: 0, width: 200, height: 300 };
        let mut parts = RegionParts::default();
        let mut luma = |pb: &platform_mac::CVPixelBuffer, tile, update| {
            parts.update(&ProbeFrame { pixel_buffer: pb, tile, stream: (200, 300), update, update_mask: 0, timing: Default::default() }, (0.0, 0.0, 1.0, 1.0))
        };
        let near = |got: Option<f64>, want: f64| got.is_some_and(|v| (v - want).abs() < 1e-9);
        assert!(near(luma(&flat_frame(200, 300, 60), full, 1), 60.0));
        // Update 3's tile decodes before update 2's full frame.
        assert!(near(luma(&flat_frame(200, 100, 240), tile, 3), (60.0 * 200.0 + 240.0 * 100.0) / 300.0));
        assert!(near(luma(&flat_frame(200, 300, 0), full, 2), 240.0 / 3.0), "the full frame under the newer tile");
        // An older tile than what's shown there changes nothing.
        assert_eq!(luma(&flat_frame(200, 100, 9), tile, 2), None);
    }

    /// A `width`×`height` NV12 picture whose luma at (x, y) is `luma(x, y)`.
    fn picture(width: usize, height: usize, luma: impl Fn(usize, usize) -> u8) -> platform_mac::CFRetained<platform_mac::CVPixelBuffer> {
        use objc2_core_video::{
            CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
            CVPixelBufferUnlockBaseAddress,
        };
        let pb = flat_frame(width, height, 0);
        unsafe {
            CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
            let base = CVPixelBufferGetBaseAddressOfPlane(&pb, 0) as *mut u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, 0);
            for y in 0..height {
                for x in 0..width {
                    *base.add(y * stride + x) = luma(x, y);
                }
            }
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags::empty());
        }
        pb
    }

    /// The test stream: 1280×256 pixels of a 640×128 point display, Frame Source's strip at
    /// (16, 16) points, 16 squares of 32 points, showing frame `n`, on mid-grey.
    const STREAM: (u32, u32) = (1280, 256);

    fn test_strip() -> Strip {
        Strip::new((16.0, 16.0, 512.0, 32.0), (640.0, 128.0), 16)
    }

    fn stream_luma(n: u32, x: usize, y: usize) -> u8 {
        if !(32..96).contains(&y) || !(32..32 + 16 * 64).contains(&x) {
            return 128;
        }
        let gray = n ^ (n >> 1);
        if (gray >> ((x - 32) / 64)) & 1 == 1 { 235 } else { 16 }
    }

    /// Two tiles side by side; the strip's square 9 straddles them.
    fn half(index: u8) -> TileRect {
        TileRect { index, x: u32::from(index) * 640, y: 0, width: 640, height: 256 }
    }

    /// Tile `tile`'s picture of frame `n`.
    fn tile_of(tile: TileRect, n: u32) -> platform_mac::CFRetained<platform_mac::CVPixelBuffer> {
        picture(tile.width as usize, tile.height as usize, |x, y| stream_luma(n, tile.x as usize + x, tile.y as usize + y))
    }

    fn probe_frame(pixel_buffer: &platform_mac::CVPixelBuffer, tile: TileRect, update: u32, update_mask: u64) -> ProbeFrame<'_> {
        ProbeFrame { pixel_buffer, tile, stream: STREAM, update, update_mask, timing: Default::default() }
    }

    #[test]
    fn gray_codes_decode() {
        for n in (0..=0xFFFF_u32).step_by(7).chain([0xBEEF, 0xFFFF]) {
            assert_eq!(gray_decode(n ^ (n >> 1)), n);
        }
    }

    #[test]
    fn strip_reads_the_frame_as_of_each_update() {
        let mut strip = test_strip();
        let sample = |strip: &mut Strip, tile: TileRect, n: u32, update: u32| strip.sample(&tile_of(tile, n), tile, STREAM, update);
        sample(&mut strip, half(0), 1000, 1);
        assert_eq!(strip.read(1), None, "the right half's squares not seen yet");
        sample(&mut strip, half(1), 1000, 1);
        assert_eq!(strip.read(1), Some(1000));
        // Update 3 finishes before update 2's right half is decoded: each reads as of itself.
        sample(&mut strip, half(0), 1001, 2);
        sample(&mut strip, half(0), 1002, 3);
        sample(&mut strip, half(1), 1002, 3);
        assert_eq!(strip.read(3), Some(1002));
        sample(&mut strip, half(1), 1001, 2);
        assert_eq!(strip.read(2), Some(1001));
        assert_eq!(strip.read(3), Some(1002));
        // A square that isn't white or black (the desktop under the source) reads as nothing.
        let grey = picture(640, 256, |_, _| 128);
        strip.sample(&grey, half(0), STREAM, 4);
        assert_eq!(strip.read(4), None);
        assert_eq!(strip.read(3), Some(1002), "older updates keep their samples");
        // Another stream size starts over.
        strip.sample(&grey, half(0), (2560, 512), 5);
        assert_eq!(strip.read(3), None);
    }

    /// The whole-picture streams' pictures cover the stream, the motion stream's scaled down.
    #[test]
    fn strip_reads_whole_pictures_at_any_scale() {
        let mut strip = test_strip();
        let whole = |index| TileRect { index, x: 0, y: 0, width: STREAM.0, height: STREAM.1 };
        let full = picture(1280, 256, |x, y| stream_luma(0xBEEF, x, y));
        strip.sample(&full, whole(FULL_FRAME_TILE), STREAM, 1);
        assert_eq!(strip.read(1), Some(0xBEEF));
        for (n, (w, h)) in [(0xBEF0, (640, 128)), (0xBEF1, (853, 170))] {
            let scaled = picture(w, h, |x, y| stream_luma(n, x * 1280 / w, y * 256 / h));
            strip.sample(&scaled, whole(MOTION_FRAME_TILE), STREAM, n);
            assert_eq!(strip.read(n), Some(n), "{w}x{h}");
        }
        // A tile after it replaces only its own squares.
        strip.sample(&tile_of(half(1), 0), half(1), STREAM, 0xBEF2);
        let left = 0xBEF1 ^ (0xBEF1 >> 1);
        let mixed = (left & 0x01FF) | ((0 ^ (0 >> 1)) & 0xFE00);
        assert_eq!(strip.read(0xBEF2), Some(gray_decode(mixed)), "square 9 straddles both: the newest of them wins");
    }

    #[test]
    fn watch_reads_and_traces_each_finished_update() {
        let path = std::env::temp_dir().join(format!("lankvm-probe-trace-{}.jsonl", std::process::id()));
        let trace = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        let mut watch = Watch { latest: Some(HashMap::new()), trace: Some(trace), ..Default::default() };
        let both = half(0).bit() | half(1).bit();
        let tile = |watch: &mut Watch, half: TileRect, n: u32, update: u32, mask: u64| {
            watch.tile(&probe_frame(&tile_of(half, n), half, update, mask));
        };
        // Decoded before the strip's place was known: read from once it is.
        tile(&mut watch, half(0), 7, 1, both);
        tile(&mut watch, half(1), 7, 1, both);
        watch.place_strip(test_strip());
        // Frame 8 changes a square on the left only; frames 9 and 10 never arrive.
        tile(&mut watch, half(0), 8, 2, half(0).bit());
        tile(&mut watch, half(0), 11, 3, both);
        tile(&mut watch, half(1), 11, 3, both);
        let shown: Vec<u64> = watch.frames.shown.iter().map(|s| s.0).collect();
        assert_eq!((shown, watch.frames.skipped), (vec![8, 11], 2));
        drop(watch);
        let lines: Vec<Value> = std::fs::read_to_string(&path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let _ = std::fs::remove_file(&path);
        let field = |key: &str| lines.iter().map(|l| l[key].clone()).collect::<Vec<_>>();
        assert_eq!(field("update"), [1, 2, 3]);
        assert_eq!(field("n"), [Value::Null, 8.into(), 11.into()], "no strip yet when update 1 finished");
        assert_eq!(field("new"), [false, true, true]);
        assert_eq!(field("tiles"), [2, 1, 2]);
    }

    #[test]
    fn source_frames_count_skipped_late_and_wrapped_frames() {
        let modulus = 1 << 16;
        let mut f = SourceFrames::default();
        assert_eq!(f.finished(Some(10), modulus, Some(1), 2), Some((10, true)));
        assert_eq!(f.finished(Some(11), modulus, Some(3), 4), Some((11, true)));
        assert_eq!(f.finished(Some(11), modulus, Some(5), 6), Some((11, false)), "the same frame again");
        assert_eq!(f.finished(Some(14), modulus, Some(7), 8), Some((14, true)), "12 and 13 skipped");
        assert_eq!(f.finished(Some(13), modulus, Some(9), 10), Some((13, false)), "finished late");
        assert_eq!(f.finished(None, modulus, None, 11), None);
        assert_eq!((f.shown.len(), f.skipped, f.late, f.unread, f.restarts), (3, 2, 1, 1, 0));
        assert_eq!(f.shown[2], (14, Some(7), 8));
        // The strip's number wraps; the count goes on.
        let mut f = SourceFrames { newest: Some(65_534), ..Default::default() };
        assert_eq!(f.finished(Some(65_535), modulus, None, 1), Some((65_535, true)));
        assert_eq!(f.finished(Some(1), modulus, None, 2), Some((65_537, true)));
        assert_eq!(f.skipped, 1);
        // A jump too big to be frames skipped starts the count over: the source started over.
        assert_eq!(f.finished(Some(30_000), modulus, None, 3), Some((30_000, true)));
        assert_eq!((f.restarts, f.skipped, f.late), (1, 1, 0));
        assert_eq!(f.finished(Some(30_001), modulus, None, 4), Some((30_001, true)));
    }

    #[test]
    fn source_log_is_read_as_it_grows() {
        let path = std::env::temp_dir().join(format!("lankvm-probe-test-{}.jsonl", std::process::id()));
        let window = r#"{"type":"window","display_id":3,"scale":2,"screen_points":[1280,800],"strip_points":[16,16,512,32],"squares":16}"#;
        let frame = |n: u64, presented: u64| format!(r#"{{"type":"frame","n":{n},"commit_us":{},"presented_us":{presented}}}"#, 100 * n);
        std::fs::write(&path, format!("{window}\n{}\n{}", frame(0, 1000), &frame(1, 0)[..20])).unwrap();
        let mut log = SourceLog::new(path.to_str().unwrap());
        log.poll();
        assert_eq!(log.strip, Some(((16.0, 16.0, 512.0, 32.0), (1280.0, 800.0), 16)));
        assert_eq!((log.frames.len(), log.on_screen(0)), (1, Some(1000)), "the half-written line waits");
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}\n{}", &frame(1, 0)[20..], frame(2, 9000)).unwrap();
        log.poll();
        assert_eq!(log.frames.len(), 3);
        assert_eq!((log.on_screen(1), log.on_screen(2)), (None, Some(9000)), "frame 1 never reached the screen");
        let shown = [(0, Some(1500), 4000), (1, Some(2000), 5000), (2, None, 12_500)];
        assert_eq!(log.delays(&shown), (vec![3.0, 3.5], vec![0.5]));
        // A new run of the source starts the log over; on a screen without presentation times
        // (a virtual display), frames count from their commit.
        std::fs::write(&path, format!("{}\n{}\n", frame(0, 0), frame(1, 0))).unwrap();
        log.poll();
        assert_eq!((log.strip, log.frames.len(), log.on_screen(1), log.presents), (None, 2, Some(100), false));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_gesture_scenarios_are_valid() {
        let scripts = [include_str!("../../../scripts/e2e/gestures.jsonl"), include_str!("../../../scripts/e2e/gesture-silence.jsonl")];
        for line in scripts.iter().flat_map(|s| s.lines()).map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let step: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(gesture_step(&step, &|_| Ok((1, 2))).is_ok(), "{line}");
        }
    }
}
