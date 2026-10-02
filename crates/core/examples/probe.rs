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

use std::io::BufRead;
use std::process::ExitCode;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use lankvm_core::{Core, Event};

/// Long enough for someone to read the PIN off the host (which allows 120 s).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(150);

struct Args {
    target: String,
    seconds: u64,
    max: (u32, u32),
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let (mut target, mut seconds, mut max) = (None, 10, (16384, 16384));
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => seconds = args.next().and_then(|s| s.parse().ok()).ok_or("--seconds needs a number")?,
            "--max" => {
                max = args
                    .next()
                    .and_then(|s| {
                        let (w, h) = s.split_once('x')?;
                        Some((w.parse().ok()?, h.parse().ok()?))
                    })
                    .ok_or("--max needs WIDTHxHEIGHT")?
            }
            _ if target.is_none() && !arg.starts_with('-') => target = Some(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    let target = target.ok_or("usage: probe <host[:port]> [--seconds N] [--max WxH]")?;
    Ok(Args { target, seconds, max })
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
    let id = core.connect(&args.target, args.max);
    let code = run(&core, id, &events, &args);
    core.disconnect(id);
    // Skip tearing down the runtime and capture threads; the OS cleans up.
    std::process::exit(code)
}

fn run(core: &Arc<Core>, id: u64, events: &mpsc::Receiver<Event>, args: &Args) -> i32 {
    let started = Instant::now();
    loop {
        let Some(left) = CONNECT_TIMEOUT.checked_sub(started.elapsed()) else {
            println!("FAIL: not connected after {}s", CONNECT_TIMEOUT.as_secs());
            return 1;
        };
        match events.recv_timeout(left) {
            Ok(Event::PinNeeded { .. }) => {
                println!("PIN needed: type the code shown on the host");
                let mut pin = String::new();
                if std::io::stdin().lock().read_line(&mut pin).unwrap_or(0) == 0 {
                    println!("FAIL: no PIN on stdin");
                    return 1;
                }
                core.submit_pin(id, pin.trim());
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
                break;
            }
            Ok(Event::Ended { error, .. }) => {
                println!("FAIL: session ended before connecting: {}", error.as_deref().unwrap_or("closed"));
                return 1;
            }
            Ok(_) => {}
            Err(_) => {
                println!("FAIL: not connected after {}s", CONNECT_TIMEOUT.as_secs());
                return 1;
            }
        }
    }

    let ms = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.1}"));
    let streaming = Instant::now();
    let mut ended = None;
    for second in 1..=args.seconds {
        let deadline = streaming + Duration::from_secs(second);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match events.recv_timeout(left) {
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
        let Some(s) = core.session_stats(id) else { break };
        let pipeline = match (s.encode_ms, s.network_ms, s.decode_ms) {
            (Some(e), Some(n), Some(d)) => Some(e + n + d),
            _ => None,
        };
        println!(
            "{second:>3}s  {:>3.0} fps  {:>6.1} Mbps  capture→decoded {:>5} ms (encode {} + network {} + decode {})  rtt {} ms  decoded {}  lost {}  keyframe requests {}",
            s.fps,
            s.mbps,
            ms(pipeline),
            ms(s.encode_ms),
            ms(s.network_ms),
            ms(s.decode_ms),
            ms(s.rtt_ms),
            s.frames_decoded,
            s.frames_lost,
            s.keyframe_requests
        );
    }

    let decoded = core.session_stats(id).map_or(0, |s| s.frames_decoded);
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
