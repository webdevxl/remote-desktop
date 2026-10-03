//! Host side of remote control: who may control this Mac, and the thread that turns a viewer's
//! input stream into injected events.
//!
//! Each controlling viewer gets its own input thread. It reads the viewer's input stream, merges
//! any backlog of moves, and injects the result through [`InputState`], so nothing can be left
//! held: the thread releases every key and button, and cancels a trackpad gesture in progress,
//! when control is switched off, when the viewer goes silent, and whenever it exits for any
//! reason.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use platform_mac::{clock, gesture, keys};
use platform_mac::inject::{
    self, AppGestureKind, Bounds, DockAxis, Gesture, GesturePhase, InjectGuard, InputState, MouseKind, Poster, Scroll, Synth, SystemAction,
};
use protocol::{GestureInput, HostMsg, INPUT_ACK_INTERVAL_US, InputMsg, MAX_RELAY_DEPTH, POS_MAX};
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
/// System actions (Mission Control, a Space left...) injected per second at most, whether the
/// viewer asked for them or a discrete Dock swipe did. Each one animates for a good part of a
/// second, so faster ones would only queue up behind it or toggle it back; the rest of that
/// second's are dropped.
const MAX_SYSTEM_ACTIONS_PER_SEC: u32 = 4;

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

    /// Posts what the poster has come due (the end of a Dock swipe, once more). A recording has
    /// nothing timed, so the same input always records the same lines.
    fn poll(&mut self, now: Instant) {
        if let Sink::Hid(poster) = self {
            poster.poll(now);
        }
    }

    /// Posts what the poster has pending once it comes due, waiting for it (at most 200 ms, and
    /// only if something is pending): for when nothing will poll any more.
    fn post_pending(&mut self) {
        if let Sink::Hid(poster) = self {
            poster.post_pending();
        }
    }
}

fn phase_name(phase: GesturePhase) -> &'static str {
    match phase {
        GesturePhase::Began => "began",
        GesturePhase::Changed => "changed",
        GesturePhase::Ended => "ended",
        GesturePhase::Cancelled => "cancelled",
    }
}

fn axis_name(axis: DockAxis) -> &'static str {
    match axis {
        DockAxis::Horizontal => "horizontal",
        DockAxis::Vertical => "vertical",
        DockAxis::Pinch => "pinch",
    }
}

fn action_name(action: SystemAction) -> &'static str {
    match action {
        SystemAction::MissionControl => "mission_control",
        SystemAction::AppExpose => "app_expose",
        SystemAction::ShowDesktop => "show_desktop",
        SystemAction::Launchpad => "launchpad",
        SystemAction::PreviousSpace => "previous_space",
        SystemAction::NextSpace => "next_space",
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
        Synth::DockSwipe { axis, phase, progress, velocity_x, velocity_y, inverted } => json!({
            "type": "gesture", "gesture": "dock", "axis": axis_name(*axis), "phase": phase_name(*phase),
            "progress": progress, "velocity": [velocity_x, velocity_y], "inverted": inverted,
        }),
        Synth::AppGesture { at, phase, kind: AppGestureKind::Magnify, value, flags } => json!({
            "type": "gesture", "gesture": "magnify", "x": at.x, "y": at.y, "phase": phase_name(*phase), "delta": value, "flags": flags,
        }),
        Synth::AppGesture { at, phase, kind: AppGestureKind::Rotate, value, flags } => json!({
            "type": "gesture", "gesture": "rotate", "x": at.x, "y": at.y, "phase": phase_name(*phase), "degrees": value, "flags": flags,
        }),
        Synth::SmartMagnify { at, flags } => json!({
            "type": "gesture", "gesture": "smart_magnify", "x": at.x, "y": at.y, "flags": flags,
        }),
        Synth::NavigationSwipe { at, dx, dy, flags } => json!({
            "type": "gesture", "gesture": "swipe", "x": at.x, "y": at.y, "dx": dx, "dy": dy, "flags": flags,
        }),
        Synth::System(action) => json!({ "type": "system", "action": action_name(*action) }),
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
        InputMsg::Gesture(GestureInput::DockSwipe { axis, phase, progress, velocity_x, velocity_y, inverted }) => {
            // From the wire's direction convention to this Mac's (see `GestureInput::DockSwipe`).
            // The factor comes with the state, so a recording uses the same one on any Mac.
            let direction = state.dock_modes().direction(axis);
            let swipe = Gesture::DockSwipe {
                axis,
                progress: f64::from(progress) * direction,
                velocity_x: f64::from(velocity_x) * direction,
                velocity_y: f64::from(velocity_y) * direction,
                inverted,
            };
            state.gesture(phase, swipe, None, out);
        }
        InputMsg::Gesture(GestureInput::Magnify { x, y, phase, delta }) => {
            state.gesture(phase, Gesture::Magnify { delta: f64::from(delta) }, Some(at(x, y)), out);
        }
        InputMsg::Gesture(GestureInput::Rotate { x, y, phase, degrees }) => {
            state.gesture(phase, Gesture::Rotate { degrees: f64::from(degrees) }, Some(at(x, y)), out);
        }
        InputMsg::Gesture(GestureInput::SmartMagnify { x, y }) => state.smart_magnify(at(x, y), out),
        InputMsg::Gesture(GestureInput::NavigationSwipe { x, y, dx, dy }) => state.navigation_swipe(at(x, y), dx, dy, out),
        // Actions this version doesn't know never get here (see `InputMsg::sanitized`).
        InputMsg::System(action) => {
            if let Some(action) = SystemAction::from_wire(action) {
                state.system(action, out);
            }
        }
    }
}

/// Whether `msg` needs a position on the display.
fn is_pointer(msg: &InputMsg) -> bool {
    match msg {
        InputMsg::MouseMove { .. } | InputMsg::MouseButton { .. } | InputMsg::Scroll(_) => true,
        InputMsg::Gesture(g) => g.position().is_some(),
        _ => false,
    }
}

/// Whether `msg` starts something (a press, a gesture, a tap or an action): what the flood
/// guard counts.
fn is_press(msg: &InputMsg) -> bool {
    match msg {
        InputMsg::Key { down: true, repeat: false, .. } | InputMsg::MouseButton { down: true, .. } | InputMsg::System(_) => true,
        // Taps have no phase.
        InputMsg::Gesture(g) => g.phase().is_none_or(|p| p == GesturePhase::Began),
        _ => false,
    }
}

/// Shared between a session and its input thread.
pub struct InputShared {
    /// Input is injected only while this is set; otherwise it is read and dropped.
    pub active: AtomicBool,
    /// Host clock (µs) of the viewer's last ping (kept for diagnostics).
    pub last_ping_us: AtomicU64,
    /// The display the viewer sees, which pointer positions are on.
    display_id: AtomicU32,
    /// Counts display switches, so the input thread notices one even back to the same display.
    display_epoch: AtomicU64,
}

impl Default for InputShared {
    fn default() -> Self {
        Self {
            active: AtomicBool::new(false),
            last_ping_us: AtomicU64::new(clock::now_us()),
            display_id: AtomicU32::new(0),
            display_epoch: AtomicU64::new(0),
        }
    }
}

impl InputShared {
    /// Pointer input goes to `display_id` from now on (the viewer switched displays).
    pub fn set_display(&self, display_id: u32) {
        self.display_id.store(display_id, Ordering::Release);
        self.display_epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub fn display(&self) -> (u32, u64) {
        // The epoch first: a switch seen half-way is seen again next time.
        let epoch = self.display_epoch.load(Ordering::Acquire);
        (self.display_id.load(Ordering::Acquire), epoch)
    }
}

pub enum InputCmd {
    /// Let go of everything held (control switched off). Signals the sender when done, once the
    /// end of a Dock swipe it cancelled has gone out once more too: the sender is about to quit
    /// (see [`release_all_and_wait`]), and nothing would post it after that.
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

/// How an input thread is set up. Pointer positions go to the display in [`InputShared`].
pub struct InputConfig {
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
            let InputConfig { backend, tag, guard_pid, on_failure } = config;
            let (display_id, display_epoch) = shared.display();
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
            let mut state = InputState::new(pos, caps_lock(), inject::mouse_event_number_seed());
            if !recording {
                // How this Mac's Dock takes swipes. The first call self-tests the recipes (in
                // memory, a few milliseconds): better now than in the middle of a swipe.
                state.set_dock_modes(gesture::dock_modes());
            }
            let mut worker = Worker {
                sink,
                guard: guard_pid.map(InjectGuard::new),
                state,
                display_id,
                display_epoch,
                recording,
                bounds: None,
                shared,
                out,
                synth: Vec::new(),
                depth: 0,
                host_caps_lock: caps_lock,
                seq: 0,
                last_ack_us: 0,
                last_activity: None,
                flood: FloodGuard::default(),
                system_actions: RateLimit::default(),
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
            // Nothing polls the poster once this thread is gone: the end of a swipe the release
            // cancelled goes out once more first, when due. Nothing waits on this, and a swipe
            // begun meanwhile (a new thread's) drops it.
            worker.sink.post_pending();
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

/// Counts events in one-second windows.
#[derive(Default)]
pub(crate) struct RateLimit {
    window_start: Option<Instant>,
    count: u32,
}

impl RateLimit {
    /// Counts one event at `now`; false if that makes more than `max` in the current second.
    pub(crate) fn allow(&mut self, now: Instant, max: u32) -> bool {
        if self.window_start.is_none_or(|start| now.duration_since(start) >= Duration::from_secs(1)) {
            self.window_start = Some(now);
            self.count = 0;
        }
        self.count += 1;
        self.count <= max
    }

    /// Whether the event just counted was the first one refused in its second (to say so once).
    fn first_refused(&self, max: u32) -> bool {
        self.count == max + 1
    }
}

struct Worker {
    sink: Sink,
    guard: Option<InjectGuard>,
    flood: FloodGuard,
    system_actions: RateLimit,
    display_warned: bool,
    state: InputState,
    display_id: u32,
    /// The [`InputShared`] display switch this thread last followed.
    display_epoch: u64,
    /// Recording (tests): the pointer is never really anywhere.
    recording: bool,
    /// The display's bounds at the last batch, to notice it moving.
    bounds: Option<Bounds>,
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
                        if is_press(&msg) && self.flood.press(received_us) {
                            return Err(Stop::Bad("more than 1000 presses, gestures and actions a second".into()));
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
                            // The app quits once told (see `InputCmd::ReleaseAll`).
                            self.sink.post_pending();
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
                    self.sink.poll(Instant::now());
                }
            }
        }
    }

    fn inject(&mut self, msgs: impl Iterator<Item = InputMsg>, received_us: u64) {
        self.follow_display();
        let bounds = Bounds::of_display(self.display_id);
        // The display moved in global coordinates (another display became main, or stopped being
        // main): the pointer moved with it, so the next move mustn't carry the shift as a delta.
        if let Some(before) = self.bounds.filter(|b| b.is_usable() && bounds.is_usable() && (b.x, b.y) != (bounds.x, bounds.y)) {
            let at = self.state.position();
            self.state.rebase(inject::Point { x: at.x + bounds.x - before.x, y: at.y + bounds.y - before.y });
        }
        self.bounds = Some(bounds);
        for msg in msgs {
            match msg {
                InputMsg::Relayed { depth } => {
                    // What came before goes out stamped with its own depth.
                    self.flush();
                    self.depth = depth;
                    continue;
                }
                // Input that went around a loop of Macs controlling each other dies here. Releases
                // still apply (only of what is held, so they can't start anything going round),
                // and so does the end of a gesture (only of the one in progress).
                InputMsg::ReleaseAll | InputMsg::Heartbeat => {}
                InputMsg::Key { down: false, .. } | InputMsg::MouseButton { down: false, .. } => {}
                InputMsg::Gesture(g) if g.ends() => {}
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
                // The streamed display is gone: don't click at (0, 0), let go of buttons (and end
                // a pinch or rotation made there) instead.
                if !self.display_warned {
                    tracing::warn!(display = self.display_id, "display unavailable; dropping pointer input");
                    self.display_warned = true;
                }
                self.state.cancel_positioned_gesture(&mut self.synth);
                self.state.release_buttons(&mut self.synth);
                continue;
            }
            if let InputMsg::System(action) = msg
                && !self.allow_system_action(action)
            {
                continue;
            }
            let ends_swipe = matches!(msg, InputMsg::Gesture(GestureInput::DockSwipe { phase: GesturePhase::Ended, .. }));
            let start = self.synth.len();
            apply(&mut self.state, msg, &bounds, self.host_caps_lock, &mut self.synth);
            // A discrete Dock swipe ends as the action it asks for (see `InputState::gesture`),
            // which counts against the same limit as one the viewer asks for itself.
            if ends_swipe
                && let [.., Synth::System(action)] = self.synth[start..]
                && !self.allow_system_action(action)
            {
                self.synth.pop();
            }
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

    /// Counts one system action against the limit; false if it is over it (said once a second).
    fn allow_system_action(&mut self, action: impl std::fmt::Debug) -> bool {
        let allowed = self.system_actions.allow(Instant::now(), MAX_SYSTEM_ACTIONS_PER_SEC);
        if !allowed && self.system_actions.first_refused(MAX_SYSTEM_ACTIONS_PER_SEC) {
            tracing::warn!(?action, "more than {MAX_SYSTEM_ACTIONS_PER_SEC} system actions a second; dropping the rest");
        }
        allowed
    }

    /// The viewer switched displays: what was pressed or pinched on the old one is let go (its
    /// positions mean nothing on the new one), and moves start from where the pointer is now.
    fn follow_display(&mut self) {
        let (id, epoch) = self.shared.display();
        if epoch == self.display_epoch {
            return;
        }
        self.display_epoch = epoch;
        tracing::info!(from = self.display_id, to = id, "input follows the viewer to another display");
        self.state.cancel_positioned_gesture(&mut self.synth);
        self.state.release_buttons(&mut self.synth);
        self.flush();
        self.display_id = id;
        self.display_warned = false;
        self.bounds = None;
        let pos = if self.recording { Bounds::of_display(id).point_at(0.5, 0.5) } else { inject::cursor_position() };
        self.state.rebase(pos);
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
        apply(&mut s, dock(GesturePhase::Began, 0.0), &bounds(), || false, &mut out);
        apply(&mut s, dock(GesturePhase::Changed, 0.4), &bounds(), || false, &mut out);
        assert!(s.holds_anything());
        out.clear();
        apply(&mut s, InputMsg::ReleaseAll, &bounds(), || false, &mut out);
        assert!(!s.holds_anything());
        assert_eq!(kinds(&out), ["gesture dock cancelled", "keyup", "keyup"], "the swipe ends first: {out:#?}");
        assert!((synth_json(&out[0])["progress"].as_f64().unwrap() - 0.4).abs() < 1e-6, "{out:#?}");
    }

    fn dock(phase: GesturePhase, progress: f32) -> InputMsg {
        InputMsg::Gesture(GestureInput::DockSwipe { axis: DockAxis::Vertical, phase, progress, velocity_x: 0.0, velocity_y: 0.0, inverted: false })
    }

    /// Each event as "type", or "type gesture phase" for gestures.
    fn kinds(out: &[Synth]) -> Vec<String> {
        out.iter()
            .map(|e| {
                let j = synth_json(e);
                [&j["type"], &j["gesture"], &j["phase"]].iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" ")
            })
            .collect()
    }

    #[test]
    fn gestures_land_where_the_viewer_made_them() {
        let mut s = InputState::new(Default::default(), false, 0);
        let mut out = Vec::new();
        let pinch = |phase, delta| InputMsg::Gesture(GestureInput::Magnify { x: POS_MAX / 4, y: POS_MAX / 2, phase, delta });
        apply(&mut s, pinch(GesturePhase::Began, 0.0), &bounds(), || false, &mut out);
        apply(&mut s, pinch(GesturePhase::Changed, 0.05), &bounds(), || false, &mut out);
        apply(&mut s, pinch(GesturePhase::Ended, 0.0), &bounds(), || false, &mut out);
        let tap = InputMsg::Gesture(GestureInput::SmartMagnify { x: POS_MAX, y: 0 });
        apply(&mut s, tap, &bounds(), || false, &mut out);
        let swipe = InputMsg::Gesture(GestureInput::NavigationSwipe { x: POS_MAX, y: 0, dx: 1, dy: 0 });
        apply(&mut s, swipe, &bounds(), || false, &mut out);
        assert_eq!(
            kinds(&out),
            ["move", "gesture magnify began", "gesture magnify changed", "gesture magnify ended", "move", "gesture smart_magnify", "gesture swipe"]
        );
        let pinched = synth_json(&out[2]);
        assert!((pinched["x"].as_f64().unwrap() - 500.0).abs() < 0.1 && (pinched["y"].as_f64().unwrap() - 500.0).abs() < 0.1, "{pinched}");
        assert!((pinched["delta"].as_f64().unwrap() - 0.05).abs() < 1e-6);
        assert_eq!(synth_json(&out[6])["x"], 1999.0);
        assert_eq!(synth_json(&out[6])["dx"], 1);
    }

    #[test]
    fn dock_swipes_are_turned_to_this_macs_direction() {
        let mut s = InputState::new(Default::default(), false, 0);
        let mut out = Vec::new();
        let swipe = |phase, progress, velocity_x| {
            InputMsg::Gesture(GestureInput::DockSwipe { axis: DockAxis::Horizontal, phase, progress, velocity_x, velocity_y: 0.0, inverted: true })
        };
        // The same on every Mac unless set: what recordings use.
        apply(&mut s, swipe(GesturePhase::Began, 0.0, 0.0), &bounds(), || false, &mut out);
        apply(&mut s, swipe(GesturePhase::Ended, 0.75, 3.0), &bounds(), || false, &mut out);
        assert_eq!(
            out[1],
            Synth::DockSwipe { axis: DockAxis::Horizontal, phase: GesturePhase::Ended, progress: 0.75, velocity_x: 3.0, velocity_y: 0.0, inverted: true }
        );
        // A Mac whose Dock reads horizontal swipes the other way round.
        s.set_dock_modes(gesture::DockModes { directions: [-1.0, 1.0, 1.0], ..gesture::DockModes::default() });
        out.clear();
        apply(&mut s, swipe(GesturePhase::Began, 0.0, 0.0), &bounds(), || false, &mut out);
        apply(&mut s, swipe(GesturePhase::Ended, 0.75, 3.0), &bounds(), || false, &mut out);
        assert!(matches!(out[1], Synth::DockSwipe { progress: -0.75, velocity_x: -3.0, inverted: true, .. }), "{out:#?}");
    }

    #[test]
    fn system_actions_are_done_or_become_space_swipes() {
        let mut s = InputState::new(Default::default(), false, 0);
        let mut out = Vec::new();
        apply(&mut s, InputMsg::System(protocol::SystemAction::MISSION_CONTROL), &bounds(), || false, &mut out);
        apply(&mut s, InputMsg::System(protocol::SystemAction::NEXT_SPACE), &bounds(), || false, &mut out);
        // Unknown codes are dropped by sanitizing; one that got here anyway does nothing.
        apply(&mut s, InputMsg::System(protocol::SystemAction(99)), &bounds(), || false, &mut out);
        assert_eq!(kinds(&out), ["system", "gesture dock began", "gesture dock changed", "gesture dock ended"]);
        assert_eq!(synth_json(&out[0]), serde_json::json!({ "type": "system", "action": "mission_control" }));
        assert_eq!(synth_json(&out[3])["axis"], "horizontal");
        assert!(!s.holds_anything());
    }

    #[test]
    fn gesture_lines_are_recorded_with_their_values() {
        use serde_json::json;
        let at = inject::Point { x: 812.0, y: 540.0 };
        let dock = Synth::DockSwipe { axis: DockAxis::Vertical, phase: GesturePhase::Began, progress: 0.25, velocity_x: 0.5, velocity_y: -2.0, inverted: false };
        assert_eq!(
            synth_json(&dock),
            json!({ "type": "gesture", "gesture": "dock", "axis": "vertical", "phase": "began", "progress": 0.25, "velocity": [0.5, -2.0], "inverted": false })
        );
        let rotate = Synth::AppGesture { at, phase: GesturePhase::Cancelled, kind: AppGestureKind::Rotate, value: -3.5, flags: 0x100 };
        assert_eq!(
            synth_json(&rotate),
            json!({ "type": "gesture", "gesture": "rotate", "x": 812.0, "y": 540.0, "phase": "cancelled", "degrees": -3.5, "flags": 0x100 })
        );
        let magnify = Synth::AppGesture { at, phase: GesturePhase::Changed, kind: AppGestureKind::Magnify, value: 0.5, flags: 0 };
        assert_eq!(synth_json(&magnify)["delta"], 0.5);
        assert_eq!(synth_json(&Synth::SmartMagnify { at, flags: 0 })["gesture"], "smart_magnify");
        assert_eq!(
            synth_json(&Synth::NavigationSwipe { at, dx: 0, dy: -1, flags: 0 }),
            json!({ "type": "gesture", "gesture": "swipe", "x": 812.0, "y": 540.0, "dx": 0, "dy": -1, "flags": 0 })
        );
        let names: Vec<_> = [
            SystemAction::MissionControl,
            SystemAction::AppExpose,
            SystemAction::ShowDesktop,
            SystemAction::Launchpad,
            SystemAction::PreviousSpace,
            SystemAction::NextSpace,
        ]
        .map(|a| synth_json(&Synth::System(a))["action"].as_str().unwrap().to_string())
        .into();
        assert_eq!(names, ["mission_control", "app_expose", "show_desktop", "launchpad", "previous_space", "next_space"]);
    }

    #[test]
    fn positioned_gestures_need_the_display_and_starts_count_as_presses() {
        let pinch = |phase| InputMsg::Gesture(GestureInput::Magnify { x: 0, y: 0, phase, delta: 0.0 });
        assert!(is_pointer(&pinch(GesturePhase::Changed)));
        assert!(is_pointer(&InputMsg::Gesture(GestureInput::SmartMagnify { x: 0, y: 0 })));
        assert!(!is_pointer(&dock(GesturePhase::Began, 0.0)), "a Dock swipe isn't tied to a display");
        assert!(!is_pointer(&InputMsg::System(protocol::SystemAction::SHOW_DESKTOP)));
        assert!(is_press(&pinch(GesturePhase::Began)));
        assert!(is_press(&dock(GesturePhase::Began, 0.0)));
        assert!(is_press(&InputMsg::Gesture(GestureInput::NavigationSwipe { x: 0, y: 0, dx: 1, dy: 0 })));
        assert!(is_press(&InputMsg::System(protocol::SystemAction::MISSION_CONTROL)));
        assert!(!is_press(&pinch(GesturePhase::Changed)) && !is_press(&dock(GesturePhase::Ended, 1.0)));
        assert!(is_press(&InputMsg::Key { code: 0, down: true, repeat: false }));
        assert!(!is_press(&InputMsg::Key { code: 0, down: true, repeat: true }));
    }

    #[test]
    fn rate_limit_counts_per_second() {
        let mut r = RateLimit::default();
        let t0 = Instant::now();
        assert!((0..50).all(|_| r.allow(t0, 50)));
        assert!(!r.allow(t0 + Duration::from_millis(999), 50));
        assert!(r.first_refused(50), "the first one over says so");
        assert!(!r.allow(t0 + Duration::from_millis(999), 50));
        assert!(!r.first_refused(50), "once a second");
        assert!(r.allow(t0 + Duration::from_secs(1), 50));
    }

    /// A worker that records to `path`, as an input thread does with `LANKVM_INJECT=record:`.
    fn recording_worker(path: &Path, state: InputState) -> Worker {
        let (out, _) = mpsc::channel(1);
        Worker {
            sink: Sink::Record(File::create(path).unwrap()),
            guard: None,
            flood: FloodGuard::default(),
            system_actions: RateLimit::default(),
            display_warned: false,
            state,
            display_id: 0,
            display_epoch: 0,
            recording: true,
            bounds: None,
            shared: Arc::new(InputShared::default()),
            out,
            synth: Vec::new(),
            depth: 0,
            host_caps_lock: || false,
            seq: 0,
            last_ack_us: 0,
            // As if just declared, so a test doesn't keep this Mac's display awake.
            last_activity: Some(Instant::now()),
        }
    }

    /// An input state whose Dock swipes are all discrete, as on a Mac where no recipe works.
    fn discrete() -> InputState {
        let mut s = InputState::new(Default::default(), false, 0);
        s.set_dock_modes(gesture::DockModes { recipes: [gesture::DockRecipe::Discrete; 3], ..Default::default() });
        s
    }

    fn recorded_actions(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|e| e["type"] == "system")
            .map(|e| e["action"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn discrete_swipes_count_against_the_system_action_limit() {
        let path = std::env::temp_dir().join(format!("lankvm-discrete-rate-{}.jsonl", std::process::id()));
        let mut w = recording_worker(&path, discrete());
        let swipe_up: &[InputMsg] = &[dock(GesturePhase::Began, 0.0), dock(GesturePhase::Ended, 0.6)];
        let asked: &[InputMsg] = &[InputMsg::System(protocol::SystemAction::MISSION_CONTROL)];
        // Two swipes up and two asked for: the limit. One more of each in that second is dropped.
        for msgs in [swipe_up, swipe_up, asked, asked, swipe_up, asked] {
            w.inject(msgs.iter().copied(), 0);
        }
        assert_eq!(recorded_actions(&path), ["mission_control"; 4]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_discrete_swipe_past_the_relay_limit_starts_nothing() {
        let path = std::env::temp_dir().join(format!("lankvm-discrete-relay-{}.jsonl", std::process::id()));
        let mut w = recording_worker(&path, discrete());
        let deep = InputMsg::Relayed { depth: MAX_RELAY_DEPTH + 1 };
        w.inject([deep, dock(GesturePhase::Began, 0.0), dock(GesturePhase::Ended, 0.6)].into_iter(), 0);
        assert!(recorded_actions(&path).is_empty());
        // One that began nearer still ends, at any depth, as a fluid swipe does.
        w.inject([InputMsg::Relayed { depth: 1 }, dock(GesturePhase::Began, 0.0), deep, dock(GesturePhase::Ended, 0.6)].into_iter(), 0);
        assert_eq!(recorded_actions(&path), ["mission_control"]);
        let _ = std::fs::remove_file(&path);
    }
}
