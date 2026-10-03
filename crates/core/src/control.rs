//! Host side of remote control: who may control this Mac, and the thread that turns a viewer's
//! input stream into injected events.
//!
//! Each controlling viewer gets its own input thread. It reads the viewer's input stream, merges
//! any backlog of moves, and injects the result through [`InputState`], so nothing can be left
//! held: the thread releases every key and button when control is switched off, when the viewer
//! goes silent, and whenever it exits for any reason.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use platform_mac::{clock, keys};
use platform_mac::inject::{self, Bounds, InjectGuard, InputState, MouseKind, Poster, Scroll, Synth};
use protocol::{HostMsg, INPUT_ACK_INTERVAL_US, InputMsg, MAX_RELAY_DEPTH, POS_MAX};
use quinn::RecvStream;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use transport::framing::FrameBuffer;

/// A viewer that sent nothing (not even a heartbeat) for this long while holding keys or buttons
/// is assumed gone or hung: its keys are released rather than left down until QUIC notices (8 s).
const SILENCE_RELEASE: Duration = Duration::from_millis(protocol::INPUT_SILENCE_RELEASE_MS);
const SILENCE_CHECK: Duration = Duration::from_millis(100);
/// How often remote activity keeps the host's display awake.
const USER_ACTIVITY_INTERVAL: Duration = Duration::from_secs(1);

/// Host-wide settings, stored in the data directory (not in user defaults, which every copy of
/// the app with the same bundle id would share).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct HostSettings {
    /// Whether paired Macs may control this one, or only view it.
    pub allow_control: bool,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self { allow_control: true }
    }
}

impl HostSettings {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?).with_context(|| format!("write {}", path.display()))
    }
}

/// Where injected input goes. `LANKVM_INJECT` picks it, for tests:
/// - unset or `hid`: post real events (needs Accessibility);
/// - `pid:<pid>`: post them to that process only (needs Accessibility);
/// - `record:<path>`: append each event to a JSON-lines file and post nothing;
/// - `deny`: behave as if Accessibility weren't granted.
#[derive(Clone, Debug, PartialEq)]
pub enum Backend {
    Hid,
    Pid(i32),
    Record(PathBuf),
    Deny,
}

impl Backend {
    pub fn from_env() -> Self {
        Self::parse(std::env::var("LANKVM_INJECT").ok().as_deref())
    }

    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("deny") => Backend::Deny,
            Some(v) if v.starts_with("record:") && v.len() > "record:".len() => Backend::Record(PathBuf::from(&v["record:".len()..])),
            Some(v) if v.starts_with("pid:") => v["pid:".len()..].parse().map_or(Backend::Hid, Backend::Pid),
            _ => Backend::Hid,
        }
    }

    /// Whether this Mac lets the backend inject (recording always can).
    pub fn permitted(&self) -> bool {
        match self {
            Backend::Hid | Backend::Pid(_) => platform_mac::permissions::input_control_allowed(),
            Backend::Record(_) => true,
            Backend::Deny => false,
        }
    }

    fn open(&self, tag: i64) -> Result<Sink> {
        Ok(match self {
            Backend::Hid | Backend::Deny => Sink::Hid(Poster::new(tag)?),
            Backend::Pid(pid) => Sink::Hid(Poster::for_pid(tag, *pid)?),
            Backend::Record(path) => Sink::Record(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("open {}", path.display()))?,
            ),
        })
    }
}

enum Sink {
    Hid(Poster),
    Record(File),
}

impl Sink {
    fn post(&mut self, synth: &Synth, depth: u8) {
        match self {
            Sink::Hid(poster) => poster.post(synth, depth),
            Sink::Record(file) => {
                let mut event = synth_json(synth);
                event["t_us"] = clock::now_us().into();
                if depth != 0 {
                    event["depth"] = depth.into();
                }
                // One write per line, so a reader never sees half of one.
                let _ = file.write_all(format!("{event}\n").as_bytes());
            }
        }
    }
}

fn synth_json(synth: &Synth) -> serde_json::Value {
    use serde_json::json;
    match synth {
        Synth::Mouse { kind, at, button, click_state, event_number, dx, dy, flags } => json!({
            "type": match kind { MouseKind::Moved => "move", MouseKind::Down => "down", MouseKind::Up => "up", MouseKind::Dragged => "drag" },
            "x": at.x, "y": at.y, "button": button, "clicks": click_state, "number": event_number,
            "dx": dx, "dy": dy, "flags": flags,
        }),
        Synth::Key { code, down, autorepeat, flags } => json!({
            "type": if *down { "keydown" } else { "keyup" }, "code": code, "repeat": autorepeat, "flags": flags,
        }),
        Synth::Scroll { at, scroll, flags } => json!({
            "type": "scroll", "x": at.x, "y": at.y,
            "lines": [scroll.lines_x, scroll.lines_y], "fixed": [scroll.fixed_x, scroll.fixed_y],
            "pixels": [scroll.pixels_x, scroll.pixels_y], "continuous": scroll.continuous,
            "phase": scroll.phase, "momentum": scroll.momentum, "inverted": scroll.inverted, "flags": flags,
        }),
    }
}

/// Applies one input message to the injection state.
pub fn apply(state: &mut InputState, msg: InputMsg, bounds: &Bounds, host_caps_lock: impl Fn() -> bool, out: &mut Vec<Synth>) {
    let at = |x: u16, y: u16| bounds.point_at(f64::from(x) / f64::from(POS_MAX), f64::from(y) / f64::from(POS_MAX));
    match msg {
        InputMsg::MouseMove { x, y } => state.move_to(at(x, y), out),
        InputMsg::MouseButton { button, down, clicks, x, y } => state.button(button, down, clicks, at(x, y), out),
        InputMsg::Scroll(s) => {
            let finite = |v: f32| if v.is_finite() { f64::from(v) } else { 0.0 };
            let scroll = Scroll {
                lines_y: s.lines_y,
                lines_x: s.lines_x,
                fixed_y: finite(s.fixed_y),
                fixed_x: finite(s.fixed_x),
                pixels_y: s.pixels_y,
                pixels_x: s.pixels_x,
                continuous: s.continuous,
                phase: s.phase,
                momentum: s.momentum,
                inverted: s.inverted,
            };
            state.scroll(scroll, at(s.x, s.y), out);
        }
        InputMsg::Key { code, down, repeat } => state.key(code, down, repeat, out),
        InputMsg::Modifiers { flags } => state.set_modifiers(u64::from(flags), out),
        InputMsg::ReleaseAll => state.release_all(host_caps_lock(), out),
        // The worker keeps the depth (see `Worker::inject`).
        InputMsg::Heartbeat | InputMsg::Relayed { .. } => {}
    }
}

/// Whether `msg` needs a position on the display.
fn is_pointer(msg: &InputMsg) -> bool {
    matches!(msg, InputMsg::MouseMove { .. } | InputMsg::MouseButton { .. } | InputMsg::Scroll(_))
}

/// Shared between a session and its input thread.
pub struct InputShared {
    /// Input is injected only while this is set; otherwise it is read and dropped.
    pub active: AtomicBool,
    /// Host clock (µs) of the viewer's last ping (kept for diagnostics).
    pub last_ping_us: AtomicU64,
}

impl Default for InputShared {
    fn default() -> Self {
        Self { active: AtomicBool::new(false), last_ping_us: AtomicU64::new(clock::now_us()) }
    }
}

pub enum InputCmd {
    /// Let go of everything held (control switched off). Signals the sender when done.
    ReleaseAll(Option<std::sync::mpsc::Sender<()>>),
}

pub type InputCmdSender = mpsc::UnboundedSender<InputCmd>;

/// The input thread of one session. Dropping it stops the thread, which releases everything.
pub struct InputThread {
    cmd: InputCmdSender,
    stop: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

/// Why an input thread gave up on its stream while the viewer was still connected.
#[derive(Debug)]
pub enum InputFailure {
    /// The viewer sent something unacceptable (unreadable, or a flood).
    BadInput(String),
    /// Injection couldn't start on this Mac.
    Unavailable(String),
}

/// Marks the thread finished however it ends (panics included).
struct Finished(Arc<AtomicBool>);

impl Finished {
    fn set(&self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Drop for Finished {
    fn drop(&mut self) {
        self.set();
    }
}

/// How an input thread is set up.
pub struct InputConfig {
    pub display_id: u32,
    pub backend: Backend,
    pub tag: i64,
    /// Tests: inject only into this process's windows (see `InjectGuard`).
    pub guard_pid: Option<i32>,
    /// Called (once) if the thread gives up while the viewer is still connected: its input was
    /// broken or flooded, or injection couldn't start. By then the thread has released
    /// everything, dropped the stream (so the viewer opens a new one with its next input) and
    /// counts as finished, so the session can attach that new stream.
    pub on_failure: Box<dyn FnOnce(InputFailure) + Send>,
}

impl InputThread {
    pub fn spawn(
        rt: tokio::runtime::Handle,
        recv: RecvStream,
        config: InputConfig,
        shared: Arc<InputShared>,
        out: mpsc::Sender<HostMsg>,
    ) -> Result<Self> {
        let (cmd, cmd_rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (thread_stop, thread_finished) = (stop.clone(), Finished(finished.clone()));
        std::thread::Builder::new().name("lankvm-input".into()).spawn(move || {
            platform_mac::system::set_thread_interactive();
            let InputConfig { display_id, backend, tag, guard_pid, on_failure } = config;
            let sink = match backend.open(tag) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("input: {e:#}");
                    thread_finished.set();
                    drop(recv);
                    on_failure(InputFailure::Unavailable(format!("{e:#}")));
                    return;
                }
            };
            // Recording posts nothing: start from a fixed state (pointer mid-display, Caps Lock
            // off), so the same input records the same events on any Mac.
            let recording = matches!(backend, Backend::Record(_));
            let (pos, caps_lock): (_, fn() -> bool) = if recording {
                (Bounds::of_display(display_id).point_at(0.5, 0.5), || false)
            } else {
                (inject::cursor_position(), inject::host_caps_lock)
            };
            let mut worker = Worker {
                sink,
                guard: guard_pid.map(InjectGuard::new),
                state: InputState::new(pos, caps_lock(), inject::mouse_event_number_seed()),
                display_id,
                shared,
                out,
                synth: Vec::new(),
                depth: 0,
                host_caps_lock: caps_lock,
                seq: 0,
                last_ack_us: 0,
                last_activity: None,
                flood: FloodGuard::default(),
                display_warned: false,
            };
            let mut recv = recv;
            let result = rt.block_on(worker.run(&mut recv, cmd_rx, &thread_stop));
            worker.release_all();
            if let Some(guard) = &worker.guard {
                tracing::info!(dropped = guard.dropped, "inject guard");
            }
            // Finished before the stream goes, so the stream the viewer opens next (once it
            // sees this one stopped) finds the session ready to take it.
            thread_finished.set();
            drop(recv);
            match result {
                Ok(()) => {}
                Err(Stop::Bad(why)) => {
                    tracing::warn!("input stream rejected: {why}");
                    on_failure(InputFailure::BadInput(why));
                }
                Err(Stop::Ended(e)) => tracing::info!("input stream ended: {e:#}"),
            }
        })?;
        Ok(Self { cmd, stop, finished })
    }

    /// Whether the thread has stopped (its stream is gone; a new one needs a new thread).
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub fn release_all(&self) {
        let _ = self.cmd.send(InputCmd::ReleaseAll(None));
    }

    /// A handle to ask for a release from elsewhere (e.g. when the app quits).
    pub fn commands(&self) -> InputCmdSender {
        self.cmd.clone()
    }
}

/// Asks every input thread in `cmds` to release what it holds and waits until they all did, or
/// until `timeout`.
pub fn release_all_and_wait(cmds: &[InputCmdSender], timeout: Duration) {
    let (done, wait) = std::sync::mpsc::channel();
    let asked = cmds.iter().filter(|c| c.send(InputCmd::ReleaseAll(Some(done.clone()))).is_ok()).count();
    let deadline = Instant::now() + timeout;
    for _ in 0..asked {
        let left = deadline.saturating_duration_since(Instant::now());
        if wait.recv_timeout(left).is_err() {
            break;
        }
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Closing the channel wakes the thread, which then releases everything and exits.
        let (dead, _) = mpsc::unbounded_channel();
        self.cmd = dead;
    }
}

/// Why an input thread stopped reading.
enum Stop {
    /// The stream ended or the connection failed.
    Ended(anyhow::Error),
    /// The viewer sent something unacceptable.
    Bad(String),
}

/// More presses than any person (or sane app) makes, kept up for a while, means a loop or a
/// broken client: stop injecting rather than hammer the Mac.
#[derive(Default)]
struct FloodGuard {
    window_start_us: u64,
    downs: u32,
    bad_windows: u32,
}

impl FloodGuard {
    const MAX_DOWNS_PER_SEC: u32 = 1000;
    const BAD_WINDOWS: u32 = 2;

    /// Counts one key or button press at `now_us`; true once the limit is exceeded long enough.
    fn press(&mut self, now_us: u64) -> bool {
        if now_us.saturating_sub(self.window_start_us) >= 1_000_000 {
            let was_bad = self.downs > Self::MAX_DOWNS_PER_SEC;
            self.bad_windows = if was_bad { self.bad_windows + 1 } else { 0 };
            self.window_start_us = now_us;
            self.downs = 0;
        }
        self.downs += 1;
        self.bad_windows + u32::from(self.downs > Self::MAX_DOWNS_PER_SEC) >= Self::BAD_WINDOWS
    }
}

struct Worker {
    sink: Sink,
    guard: Option<InjectGuard>,
    flood: FloodGuard,
    display_warned: bool,
    state: InputState,
    display_id: u32,
    shared: Arc<InputShared>,
    out: mpsc::Sender<HostMsg>,
    synth: Vec<Synth>,
    /// Relay depth of the input being injected (see `InputMsg::Relayed`).
    depth: u8,
    /// This Mac's own Caps Lock state, which a release puts back.
    host_caps_lock: fn() -> bool,
    seq: u64,
    last_ack_us: u64,
    last_activity: Option<Instant>,
}

impl Worker {
    async fn run(&mut self, recv: &mut RecvStream, mut cmd_rx: mpsc::UnboundedReceiver<InputCmd>, stop: &AtomicBool) -> Result<(), Stop> {
        let mut frames = FrameBuffer::with_limit(protocol::MAX_INPUT_MSG_LEN);
        let mut buf = vec![0u8; 64 * 1024];
        let mut batch: Vec<InputMsg> = Vec::new();
        let mut silence = tokio::time::interval(SILENCE_CHECK);
        let mut last_heard_us = clock::now_us();
        loop {
            tokio::select! {
                read = recv.read(&mut buf) => {
                    let Some(n) = read.context("read input stream").map_err(Stop::Ended)? else { return Ok(()) };
                    let received_us = clock::now_us();
                    last_heard_us = received_us;
                    frames.extend(&buf[..n]);
                    // Everything that arrived together is applied together; a backlog of moves
                    // (after a network stall) collapses to the latest position.
                    loop {
                        let msg = match frames.next::<InputMsg>() {
                            Ok(Some(msg)) => msg,
                            Ok(None) => break,
                            Err(e) => return Err(Stop::Bad(format!("unreadable input: {e:#}"))),
                        };
                        self.seq += 1;
                        let Some(msg) = msg.sanitized() else { continue };
                        let press = matches!(msg, InputMsg::Key { down: true, repeat: false, .. } | InputMsg::MouseButton { down: true, .. });
                        if press && self.flood.press(received_us) {
                            return Err(Stop::Bad("more than 1000 key and button presses a second".into()));
                        }
                        if !batch.last_mut().is_some_and(|last| last.coalesce(&msg)) {
                            batch.push(msg);
                        }
                    }
                    if self.shared.active.load(Ordering::Acquire) {
                        self.inject(batch.drain(..), received_us);
                    } else {
                        batch.clear();
                    }
                }
                cmd = cmd_rx.recv() => match cmd {
                    Some(InputCmd::ReleaseAll(done)) => {
                        self.release_all();
                        if let Some(done) = done {
                            let _ = done.send(());
                        }
                    }
                    None => return Ok(()),
                },
                _ = silence.tick() => {
                    if stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let silent_us = clock::now_us().saturating_sub(last_heard_us);
                    if self.state.holds_anything() && silent_us > SILENCE_RELEASE.as_micros() as u64 {
                        tracing::warn!(silent_ms = silent_us / 1000, "viewer went silent while holding input; releasing it");
                        self.release_all();
                    }
                }
            }
        }
    }

    fn inject(&mut self, msgs: impl Iterator<Item = InputMsg>, received_us: u64) {
        let bounds = Bounds::of_display(self.display_id);
        for msg in msgs {
            match msg {
                InputMsg::Relayed { depth } => {
                    // What came before goes out stamped with its own depth.
                    self.flush();
                    self.depth = depth;
                    continue;
                }
                // Input that went around a loop of Macs controlling each other dies here. Releases
                // still apply (only of what is held, so they can't start anything going round).
                InputMsg::ReleaseAll | InputMsg::Heartbeat => {}
                InputMsg::Key { down: false, .. } | InputMsg::MouseButton { down: false, .. } => {}
                InputMsg::Modifiers { flags } if self.depth > MAX_RELAY_DEPTH => {
                    let held = self.state.modifiers();
                    let target = (u64::from(flags) & held & !keys::CAPS_LOCK) | (held & keys::CAPS_LOCK);
                    self.state.set_modifiers(target, &mut self.synth);
                    continue;
                }
                _ if self.depth > MAX_RELAY_DEPTH => continue,
                _ => {}
            }
            if !bounds.is_usable() && is_pointer(&msg) {
                // The streamed display is gone: don't click at (0, 0), let go of buttons instead.
                if !self.display_warned {
                    tracing::warn!(display = self.display_id, "display unavailable; dropping pointer input");
                    self.display_warned = true;
                }
                self.state.release_buttons(&mut self.synth);
                continue;
            }
            apply(&mut self.state, msg, &bounds, self.host_caps_lock, &mut self.synth);
        }
        self.flush();
        let injected_us = clock::now_us();
        if injected_us.saturating_sub(self.last_ack_us) >= INPUT_ACK_INTERVAL_US {
            self.last_ack_us = injected_us;
            // Only statistics: skipped if the viewer isn't reading its control stream.
            let _ = self.out.try_send(HostMsg::InputAck { seq: self.seq, received_us, injected_us });
        }
        if self.last_activity.is_none_or(|t| t.elapsed() >= USER_ACTIVITY_INTERVAL) {
            self.last_activity = Some(Instant::now());
            platform_mac::system::declare_user_activity();
        }
    }

    fn release_all(&mut self) {
        self.state.release_all((self.host_caps_lock)(), &mut self.synth);
        self.flush();
    }

    fn flush(&mut self) {
        for synth in self.synth.drain(..) {
            if self.guard.as_mut().is_none_or(|g| g.allows(&synth)) {
                self.sink.post(&synth, self.depth);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ScrollInput;

    fn bounds() -> Bounds {
        Bounds { x: 0.0, y: 0.0, width: 2000.0, height: 1000.0 }
    }

    #[test]
    fn settings_round_trip_and_default_to_allowing_control() {
        let path = std::env::temp_dir().join(format!("lankvm-settings-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(HostSettings::load(&path).allow_control);
        HostSettings { allow_control: false }.save(&path).unwrap();
        assert!(!HostSettings::load(&path).allow_control);
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(HostSettings::load(&path), HostSettings::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn backend_choice() {
        assert_eq!(Backend::parse(None), Backend::Hid);
        assert_eq!(Backend::parse(Some("hid")), Backend::Hid);
        assert_eq!(Backend::parse(Some("record:")), Backend::Hid);
        assert_eq!(Backend::parse(Some("record:/tmp/x.jsonl")), Backend::Record(PathBuf::from("/tmp/x.jsonl")));
        assert_eq!(Backend::parse(Some("pid:4242")), Backend::Pid(4242));
        assert_eq!(Backend::parse(Some("pid:nope")), Backend::Hid);
        assert_eq!(Backend::parse(Some("deny")), Backend::Deny);
        assert!(Backend::Record(PathBuf::from("/tmp/x")).permitted());
        assert!(!Backend::Deny.permitted());
    }

    #[test]
    fn flood_guard_trips_only_on_sustained_floods() {
        let mut g = FloodGuard::default();
        // A fast typist: 20 presses a second for 10 s.
        assert!(!(0..200u64).any(|i| g.press(i * 50_000)));
        // A burst of 1500 in one second, then calm: not sustained.
        let mut g = FloodGuard::default();
        assert!(!(0..1500u64).any(|i| g.press(1_000_000 + i * 600)));
        assert!(!(0..10u64).any(|i| g.press(2_100_000 + i * 100_000)));
        // 1500 a second for 2 s: a loop.
        let mut g = FloodGuard::default();
        assert!((0..3000u64).any(|i| g.press(i * 666)));
    }

    #[test]
    fn normalized_positions_map_to_display_points() {
        let mut s = InputState::new(Default::default(), false, 0);
        let mut out = Vec::new();
        apply(&mut s, InputMsg::MouseMove { x: POS_MAX / 2, y: POS_MAX }, &bounds(), || false, &mut out);
        let Synth::Mouse { at, .. } = out[0] else { panic!() };
        assert!((at.x - 1000.0).abs() < 0.02 && at.y == 999.0, "{at:?}");
    }

    #[test]
    fn a_click_and_a_scroll_land_where_the_viewer_put_them() {
        let mut s = InputState::new(Default::default(), false, 0);
        let mut out = Vec::new();
        apply(&mut s, InputMsg::MouseButton { button: 0, down: true, clicks: 1, x: 0, y: 0 }, &bounds(), || false, &mut out);
        apply(&mut s, InputMsg::MouseButton { button: 0, down: false, clicks: 1, x: 0, y: 0 }, &bounds(), || false, &mut out);
        let scroll = ScrollInput { x: POS_MAX, y: 0, pixels_y: -4, continuous: true, phase: 2, ..Default::default() };
        apply(&mut s, InputMsg::Scroll(scroll), &bounds(), || false, &mut out);
        let kinds: Vec<_> = out.iter().map(|e| synth_json(e)["type"].as_str().unwrap().to_string()).collect();
        assert_eq!(kinds, ["down", "up", "move", "scroll"]);
        assert_eq!(synth_json(&out[3])["x"], 1999.0);
        assert_eq!(synth_json(&out[3])["pixels"], serde_json::json!([0, -4]));
    }

    #[test]
    fn release_all_message_releases() {
        let mut s = InputState::new(Default::default(), false, 0);
        let mut out = Vec::new();
        apply(&mut s, InputMsg::Modifiers { flags: 0x0010_0008 }, &bounds(), || false, &mut out);
        apply(&mut s, InputMsg::Key { code: 0, down: true, repeat: false }, &bounds(), || false, &mut out);
        assert!(s.holds_anything());
        apply(&mut s, InputMsg::ReleaseAll, &bounds(), || false, &mut out);
        assert!(!s.holds_anything());
    }
}
