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
//! `--input-latency N` clicks the middle of `--rect` (for example LanKVM Input Lab's patch,
//! which flips between black and white on every click) N times and times each click until the
//! decoded video shows the change: input → host → app redraw → capture → encode → network →
//! decode. Display (≈1 frame) is not included.

use std::io::BufRead;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use lankvm_core::{Core, Event};
use platform_mac::clock;
use protocol::{DockAxis, GestureInput, GesturePhase, InputMsg, POS_MAX, ScrollInput, SystemAction};
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
    };
    let mut target = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => a.seconds = args.next().and_then(|s| s.parse().ok()).ok_or("--seconds needs a number")?,
            "--max" => a.max = args.next().and_then(|s| pair_of(&s, 'x')).ok_or("--max needs WIDTHxHEIGHT")?,
            "--control" => a.control = true,
            "--script" => a.script = Some(args.next().ok_or("--script needs a file")?),
            "--input-latency" => a.latency = Some(args.next().and_then(|s| s.parse().ok()).ok_or("--input-latency needs a count")?),
            "--rect" => {
                let v: Vec<f64> = args.next().unwrap_or_default().split(',').filter_map(|p| p.trim().parse().ok()).collect();
                if v.len() != 4 {
                    return Err("--rect needs X,Y,W,H".into());
                }
                a.rect = Some((v[0], v[1], v[2], v[3]));
            }
            "--display" => a.display = Some(args.next().and_then(|s| pair_of(&s, 'x')).ok_or("--display needs WIDTHxHEIGHT")?),
            "--lab" => a.lab = Some(args.next().ok_or("--lab needs Input Lab's log file")?),
            "--fps" => a.fps = args.next().and_then(|s| s.parse().ok()).ok_or("--fps needs a number")?,
            _ if target.is_none() && !arg.starts_with('-') => target = Some(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    a.target = target.ok_or("usage: probe <host[:port]> [--seconds N] [--max WxH] [--fps N] [--control [--script FILE] [--input-latency N --rect X,Y,W,H] [--display WxH]]")?;
    if (a.script.is_some() || a.latency.is_some()) && !a.control {
        return Err("--script and --input-latency need --control".into());
    }
    if a.latency.is_some() && a.rect.is_none() && a.lab.is_none() {
        return Err("--input-latency needs --rect (or --lab, to use its patch)".into());
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

    let (tx, events) = mpsc::channel();
    let core = match Core::start(Arc::new(move |e| drop(tx.send(e)))) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("start: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let id = core.connect(&args.target, args.max, args.fps);
    let mut probe = Probe { core: core.clone(), id, events, pending: Vec::new(), lab: Default::default() };
    let code = probe.run(&args);
    core.disconnect(id);
    // Skip tearing down the runtime and capture threads; the OS cleans up.
    std::process::exit(code)
}

struct Probe {
    core: Arc<Core>,
    id: u64,
    events: mpsc::Receiver<Event>,
    /// Events that arrived while waiting for another kind.
    pending: Vec<Event>,
    /// Named rectangles (points, top-left origin) from Input Lab.
    lab: std::collections::HashMap<String, (f64, f64, f64, f64)>,
}

/// The latest window layout Input Lab logged: its named parts, as (x, y, w, h) in points.
fn lab_layout(path: &str) -> Result<std::collections::HashMap<String, (f64, f64, f64, f64)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let window = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["type"] == "window")
        .last()
        .ok_or(format!("no window line in {path}"))?;
    let mut out = std::collections::HashMap::new();
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
    /// video shows the region change brightness.
    fn input_latency(&mut self, n: usize, rect: (f64, f64, f64, f64), display: (f64, f64)) -> i32 {
        let (x, y, w, h) = rect;
        // Look at the middle of the region, away from its edges.
        let region = ((x + w * 0.25) / display.0, (y + h * 0.25) / display.1, (x + w * 0.75) / display.0, (y + h * 0.75) / display.1);
        let samples: Arc<Mutex<Vec<(u64, f64, Option<u64>)>>> = Arc::default();
        let sink = samples.clone();
        self.core.set_frame_probe(
            self.id,
            Some(Box::new(move |pb, timing| {
                if let Some(luma) = platform_mac::gpu::mean_luma(pb, region.0, region.1, region.2, region.3) {
                    sink.lock().unwrap().push((timing.decoded_us, luma, timing.capture_local_us));
                }
            })),
        );
        let (cx, cy) = (norm(x + w / 2.0, display.0), norm(y + h / 2.0, display.1));
        // Park the cursor on the target, and get a first frame to compare against.
        self.send(InputMsg::MouseMove { x: cx, y: cy });
        std::thread::sleep(Duration::from_millis(500));
        let mut totals = Vec::new();
        let mut to_capture = Vec::new();
        for i in 0..n {
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
            match seen {
                Some((decoded_us, luma, captured)) => {
                    let total = decoded_us.saturating_sub(t0) as f64 / 1000.0;
                    let capture = captured.map(|c| c.saturating_sub(t0) as f64 / 1000.0);
                    println!(
                        "{:>3}: {total:>6.1} ms click → decoded   (captured after {} ms, luma {:.0} → {luma:.0})",
                        i + 1,
                        capture.map_or("-".into(), |c| format!("{c:.1}")),
                        before.unwrap_or(f64::NAN)
                    );
                    totals.push(total);
                    if let Some(c) = capture {
                        to_capture.push(c);
                    }
                }
                None => println!("{:>3}: no change seen within 1 s", i + 1),
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
        let streaming = Instant::now();
        let mut ended = None;
        for second in 1..=args.seconds {
            let deadline = streaming + Duration::from_secs(second);
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                match self.events.recv_timeout(left) {
                    Ok(Event::Ended { error, .. }) => {
                        ended = Some(error.unwrap_or_else(|| "closed by host".into()));
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
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
            println!(
                "{second:>3}s  {:>3.0} fps  {:>6.1} Mbps  capture→decoded {:>5} ms (capture {} + encode {} + network {} + decode {})  rtt {} ms  decoded {}  lost {}  keyframe requests {}",
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
        }

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
}

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

    #[test]
    fn the_gesture_scenarios_are_valid() {
        let scripts = [include_str!("../../../scripts/e2e/gestures.jsonl"), include_str!("../../../scripts/e2e/gesture-silence.jsonl")];
        for line in scripts.iter().flat_map(|s| s.lines()).map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let step: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(gesture_step(&step, &|_| Ok((1, 2))).is_ok(), "{line}");
        }
    }
}
