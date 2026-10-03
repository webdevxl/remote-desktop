//! Remote control end to end over real QUIC on loopback: a viewer core controls a host core
//! that records the events it would inject instead of posting them. Needs no permissions and
//! touches neither the screen nor the user's mouse and keyboard.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lankvm_core::control::Backend;
use lankvm_core::{Core, CoreOptions, Event};
use platform_mac::inject::Bounds;
use protocol::{InputMsg, POS_MAX, ScrollInput};
use serde_json::Value;
use transport::identity::DeviceIdentity;

const WAIT: Duration = Duration::from_secs(5);

struct Peer {
    core: Arc<Core>,
    events: Receiver<Event>,
    /// Events received while waiting for something else, for later waits.
    pending: RefCell<Vec<Event>>,
}

impl Peer {
    fn start(dir: &Path, backend: Backend) -> Self {
        let (tx, events) = channel();
        let tx = Mutex::new(tx);
        let options = CoreOptions {
            data_dir: dir.to_path_buf(),
            port: 0,
            video: false,
            backend,
            allow_same_mac_control: true,
            guard_pid: None,
            control_ttl: None,
        };
        let core = Core::start_with(Arc::new(move |e| drop(tx.lock().unwrap().send(e))), options).unwrap();
        Self { core, events, pending: RefCell::new(Vec::new()) }
    }

    /// Waits for an event `pick` accepts (returns Ok); keeps the others for later waits.
    fn wait<T>(&self, what: &str, mut pick: impl FnMut(Event) -> Result<T, Event>) -> T {
        let earlier = std::mem::take(&mut *self.pending.borrow_mut());
        let mut found = None;
        for e in earlier {
            match (found.is_none(), e) {
                (true, e) => match pick(e) {
                    Ok(v) => found = Some(v),
                    Err(e) => self.pending.borrow_mut().push(e),
                },
                (false, e) => self.pending.borrow_mut().push(e),
            }
        }
        if let Some(v) = found {
            return v;
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| panic!("timed out waiting for {what}"));
            match self.events.recv_timeout(left) {
                Ok(e) => match pick(e) {
                    Ok(v) => return v,
                    Err(e) => self.pending.borrow_mut().push(e),
                },
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }

    fn connect(&self, host: &Peer) -> u64 {
        let id = self.core.connect(&format!("127.0.0.1:{}", host.core.this_mac().port), (1920, 1080), 60);
        self.wait("connected", |e| match e {
            Event::Connected { session, .. } if session == id => Ok(()),
            Event::Ended { session, error } if session == id => panic!("session ended: {error:?}"),
            e => Err(e),
        });
        id
    }

    /// Asks for control and returns the host's answer: (active, reason).
    fn request_control(&self, id: u64, on: bool) -> (bool, Option<String>) {
        self.request(id, on, false).into()
    }

    /// Asks for control and returns the host's full answer.
    fn request(&self, id: u64, on: bool, take_over: bool) -> Answer {
        let request = self.core.set_control(id, on, take_over);
        self.wait("control answer", |e| match e {
            Event::Control { session, request: r, active, reason, message, .. } if session == id && r == request => {
                Ok(Answer { active, reason, message })
            }
            e => Err(e),
        })
    }

    fn host_said(&self, id: u64) -> Answer {
        self.wait("control state", |e| match e {
            Event::Control { session, active, reason, message, .. } if session == id => Ok(Answer { active, reason, message }),
            e => Err(e),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Answer {
    active: bool,
    reason: u16,
    message: String,
}

impl From<Answer> for (bool, Option<String>) {
    fn from(a: Answer) -> Self {
        (a.active, Some(a.message).filter(|m| !m.is_empty()))
    }
}

impl Peer {
}

/// A fresh data directory with an identity, plus its fingerprint.
fn device(root: &Path, name: &str) -> (PathBuf, String) {
    let dir = root.join(name);
    let id = DeviceIdentity::load_or_create(&dir).unwrap();
    (dir, id.fingerprint.iter().map(|b| format!("{b:02x}")).collect())
}

fn trust(dir: &Path, file: &str, entries: &[(&str, &str)]) {
    let text: String = entries.iter().map(|(fp, name)| format!("{fp} {name}\n")).collect();
    std::fs::write(dir.join(file), text).unwrap();
}

/// Reads the host's record of injected events once at least `n` have arrived.
fn recorded(path: &Path, n: usize) -> Vec<Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let lines: Vec<Value> = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad record line {l:?}: {e}")))
            .collect();
        if lines.len() >= n || Instant::now() > deadline {
            return lines;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn kinds(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| {
            let t = e["type"].as_str().unwrap();
            match t {
                "keydown" | "keyup" => format!("{t}{}", e["code"]),
                "down" | "up" | "drag" => format!("{t}{}", e["button"]),
                _ => t.to_string(),
            }
        })
        .collect()
}

fn pos(v: f64) -> u16 {
    (v * f64::from(POS_MAX)).round() as u16
}

struct Setup {
    _root: TempDir,
    host: Peer,
    viewer: Peer,
    record: PathBuf,
    viewer_dir: PathBuf,
    host_fp: String,
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn setup(name: &str) -> Setup {
    let root = std::env::temp_dir().join(format!("lankvm-control-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (host_dir, host_fp) = device(&root, "host");
    let (viewer_dir, viewer_fp) = device(&root, "viewer");
    trust(&host_dir, "trusted-viewers.txt", &[(&viewer_fp, "viewer")]);
    trust(&viewer_dir, "trusted-hosts.txt", &[(&host_fp, "host")]);
    let record = root.join("injected.jsonl");
    let host = Peer::start(&host_dir, Backend::Record(record.clone()));
    let viewer = Peer::start(&viewer_dir, Backend::Hid);
    Setup { _root: TempDir(root), host, viewer, record, viewer_dir, host_fp }
}

#[test]
fn full_input_path_is_injected_in_order() {
    let s = setup("path");
    let id = s.viewer.connect(&s.host);
    assert_eq!(s.viewer.request_control(id, true), (true, None));

    let send = |m| s.viewer.core.send_input(id, m);
    send(InputMsg::MouseMove { x: pos(0.25), y: pos(0.5) });
    send(InputMsg::MouseButton { button: 0, down: true, clicks: 2, x: pos(0.25), y: pos(0.5) });
    send(InputMsg::MouseMove { x: pos(0.3), y: pos(0.5) });
    send(InputMsg::MouseButton { button: 0, down: false, clicks: 2, x: pos(0.3), y: pos(0.5) });
    send(InputMsg::MouseButton { button: 1, down: true, clicks: 1, x: pos(0.3), y: pos(0.5) });
    send(InputMsg::MouseButton { button: 1, down: false, clicks: 1, x: pos(0.3), y: pos(0.5) });
    send(InputMsg::Modifiers { flags: 0x0002_0002 }); // left shift
    send(InputMsg::Key { code: 0, down: true, repeat: false });
    send(InputMsg::Key { code: 0, down: true, repeat: true });
    send(InputMsg::Key { code: 0, down: false, repeat: false });
    send(InputMsg::Modifiers { flags: 0 });
    let scroll = ScrollInput { x: pos(0.3), y: pos(0.5), pixels_y: -10, fixed_y: -1.0, continuous: true, phase: 1, ..Default::default() };
    send(InputMsg::Scroll(scroll));

    let events = recorded(&s.record, 12);
    assert_eq!(
        kinds(&events),
        [
            "move", "down0", "drag0", "up0", "down1", "up1", "keydown56", "keydown0", "keydown0", "keyup0", "keyup56", "scroll",
        ],
        "{events:#?}"
    );
    // Positions: normalized on the viewer, display points on the host.
    let bounds = Bounds::of_display(platform_mac::capture::main_display_bounds().id);
    let at = bounds.point_at(0.25, 0.5);
    assert!((events[0]["x"].as_f64().unwrap() - at.x).abs() < 0.1 && (events[0]["y"].as_f64().unwrap() - at.y).abs() < 0.1);
    // The double click keeps its count through the drag and the release.
    assert!(events[1..4].iter().all(|e| e["clicks"] == 2), "{events:#?}");
    // Shift reaches the key events (generic + left device bit).
    assert_eq!(events[7]["flags"].as_u64().unwrap() & 0x2_0002, 0x2_0002);
    assert_eq!(events[8]["repeat"], true);
    assert_eq!(events[11]["phase"], 1);
    assert_eq!(events[11]["pixels"][1], -10);
}

#[test]
fn input_before_control_is_dropped_and_view_mode_releases() {
    let s = setup("gate");
    let id = s.viewer.connect(&s.host);
    // Not controlling: dropped on the viewer (and the host would ignore it too).
    s.viewer.core.send_input(id, InputMsg::Key { code: 1, down: true, repeat: false });
    assert_eq!(s.viewer.request_control(id, true), (true, None));
    s.viewer.core.send_input(id, InputMsg::Key { code: 2, down: true, repeat: false });
    s.viewer.core.send_input(id, InputMsg::MouseButton { button: 0, down: true, clicks: 1, x: 0, y: 0 });
    // A click away from the cursor moves there first.
    assert_eq!(kinds(&recorded(&s.record, 3)), ["keydown2", "move", "down0"]);
    // Switching to View lets go of what was held.
    assert_eq!(s.viewer.request_control(id, false), (false, None));
    let events = recorded(&s.record, 5);
    assert_eq!(kinds(&events[3..]), ["keyup2", "up0"]);
}

#[test]
fn disconnecting_mid_press_releases_everything() {
    let s = setup("disconnect");
    let id = s.viewer.connect(&s.host);
    s.viewer.request_control(id, true);
    s.viewer.core.send_input(id, InputMsg::Modifiers { flags: 0x0010_0008 }); // left command
    s.viewer.core.send_input(id, InputMsg::Key { code: 8, down: true, repeat: false }); // c
    s.viewer.core.send_input(id, InputMsg::MouseButton { button: 0, down: true, clicks: 1, x: 100, y: 100 });
    assert_eq!(kinds(&recorded(&s.record, 4)), ["keydown55", "keydown8", "move", "down0"]);
    s.viewer.core.disconnect(id);
    let events = recorded(&s.record, 7);
    // Buttons go up while ⌘ is still down, so a drag drops the way it was meant to.
    assert_eq!(kinds(&events[4..]), ["keyup8", "up0", "keyup55"], "{events:#?}");
}

#[test]
fn host_can_stop_control_and_forbid_it() {
    let s = setup("policy");
    let id = s.viewer.connect(&s.host);
    s.viewer.request_control(id, true);
    s.viewer.core.send_input(id, InputMsg::Key { code: 3, down: true, repeat: false });
    assert_eq!(recorded(&s.record, 1).len(), 1);

    // Stop control from the host UI: released, viewer told why.
    let viewer = s.host.core.host_status().viewers[0].id;
    s.host.core.stop_control(viewer);
    let stopped = s.viewer.host_said(id);
    assert!(!stopped.active && stopped.reason == 5 && stopped.message.contains("stopped"), "{stopped:?}");
    assert_eq!(kinds(&recorded(&s.record, 2)[1..]), ["keyup3"]);

    // The viewer may ask again...
    assert_eq!(s.viewer.request_control(id, true), (true, None));
    // ...until control is turned off on the host, which also ends it right away.
    s.host.core.set_allow_control(false);
    let off = s.viewer.host_said(id);
    assert!(!off.active && off.reason == 1 && off.message.contains("turned off"), "{off:?}");
    let refused = s.viewer.request(id, true, false);
    assert!(!refused.active && refused.reason == 1, "{refused:?}");
    // The setting is saved.
    s.host.core.set_allow_control(true);
    assert_eq!(s.viewer.request_control(id, true), (true, None));
    let _ = &s.host_fp;
}

#[test]
fn one_controller_at_a_time() {
    let s = setup("takeover");
    let root = s.viewer_dir.parent().unwrap();
    let (other_dir, other_fp) = device(root, "other");
    let viewer_fp = std::fs::read_to_string(root.join("host/trusted-viewers.txt")).unwrap();
    // Trust lists are read at start, so a host that trusts both needs a restart: use a second host.
    let (host2_dir, host2_fp) = device(root, "host2");
    std::fs::write(host2_dir.join("trusted-viewers.txt"), format!("{viewer_fp}{other_fp} other\n")).unwrap();
    trust(&s.viewer_dir, "trusted-hosts.txt", &[(&s.host_fp, "host"), (&host2_fp, "host2")]);
    trust(&other_dir, "trusted-hosts.txt", &[(&host2_fp, "host2")]);
    let record2 = root.join("injected2.jsonl");
    let host2 = Peer::start(&host2_dir, Backend::Record(record2.clone()));
    let viewer = Peer::start(&s.viewer_dir, Backend::Hid);
    let other = Peer::start(&other_dir, Backend::Hid);

    let a = viewer.connect(&host2);
    let b = other.connect(&host2);
    assert_eq!(viewer.request_control(a, true), (true, None));
    // Another device is refused while someone controls...
    let refused = other.request(b, true, false);
    assert!(!refused.active && refused.reason == 3 && refused.message.contains("is controlling"), "{refused:?}");
    // ...but the same device in a second window takes over.
    let a2 = viewer.connect(&host2);
    assert_eq!(viewer.request_control(a2, true), (true, None));
    let taken = viewer.host_said(a);
    assert!(!taken.active && taken.reason == 4, "{taken:?}");
    // Another device may take over explicitly; the one it displaced is told why.
    let took = other.request(b, true, true);
    assert!(took.active, "{took:?}");
    let lost = viewer.host_said(a2);
    assert!(!lost.active && lost.reason == 4 && lost.message.contains("took control"), "{lost:?}");
    // Once nobody controls, anyone may.
    other.request_control(b, false);
    assert_eq!(viewer.request_control(a, true), (true, None));
}

#[test]
fn input_latency_on_loopback() {
    let s = setup("latency");
    let id = s.viewer.connect(&s.host);
    s.viewer.request_control(id, true);
    // One event at a time, like a person: each must be injected within a few milliseconds.
    let mut worst = Duration::ZERO;
    let mut total = Duration::ZERO;
    let n = 200;
    for i in 0..n {
        let start = Instant::now();
        s.viewer.core.send_input(id, InputMsg::MouseMove { x: 1000 + i, y: 1000 });
        let deadline = start + Duration::from_secs(1);
        loop {
            let count = std::fs::read_to_string(&s.record).map(|t| t.lines().count()).unwrap_or(0);
            if count > i as usize {
                break;
            }
            assert!(Instant::now() < deadline, "event {i} not injected within 1 s");
            std::hint::spin_loop();
        }
        let took = start.elapsed();
        worst = worst.max(took);
        total += took;
    }
    let avg = total / n as u32;
    println!("loopback viewer input → host injection: avg {avg:?}, worst {worst:?} over {n} events");
    assert!(avg < Duration::from_millis(5), "average {avg:?}");
}

#[test]
fn a_silent_viewer_holding_keys_is_released_but_heartbeats_keep_them() {
    let s = setup("silence");
    let id = s.viewer.connect(&s.host);
    s.viewer.request_control(id, true);
    s.viewer.core.send_input(id, InputMsg::Key { code: 4, down: true, repeat: false });
    assert_eq!(recorded(&s.record, 1).len(), 1);
    // Heartbeats keep a long press alive...
    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(protocol::HEARTBEAT_INTERVAL_MS));
        s.viewer.core.send_input(id, InputMsg::Heartbeat);
    }
    assert_eq!(recorded(&s.record, 1).len(), 1, "released despite heartbeats");
    // ...and without them the host lets go within about a second.
    let silent = Instant::now();
    let events = recorded(&s.record, 2);
    assert_eq!(kinds(&events[1..]), ["keyup4"]);
    let after = silent.elapsed();
    assert!(after >= Duration::from_millis(800) && after < Duration::from_millis(2000), "released after {after:?}");
    // A late repeat of the released key doesn't press it again.
    s.viewer.core.send_input(id, InputMsg::Key { code: 4, down: true, repeat: true });
    s.viewer.core.send_input(id, InputMsg::Key { code: 5, down: true, repeat: false });
    assert_eq!(kinds(&recorded(&s.record, 3)[2..]), ["keydown5"]);
}

#[test]
fn same_mac_control_is_refused_without_the_override() {
    let root = std::env::temp_dir().join(format!("lankvm-control-samemac-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _cleanup = TempDir(root.clone());
    let (host_dir, host_fp) = device(&root, "host");
    let (viewer_dir, viewer_fp) = device(&root, "viewer");
    trust(&host_dir, "trusted-viewers.txt", &[(&viewer_fp, "viewer")]);
    trust(&viewer_dir, "trusted-hosts.txt", &[(&host_fp, "host")]);
    let (tx, events) = channel();
    let tx = Mutex::new(tx);
    let options = CoreOptions {
        data_dir: host_dir,
        port: 0,
        video: false,
        backend: Backend::Record(root.join("rec.jsonl")),
        allow_same_mac_control: false,
        guard_pid: None,
        control_ttl: None,
    };
    let core = Core::start_with(Arc::new(move |e| drop(tx.lock().unwrap().send(e))), options).unwrap();
    let host = Peer { core, events, pending: RefCell::new(Vec::new()) };
    let viewer = Peer::start(&viewer_dir, Backend::Hid);
    let id = viewer.connect(&host);
    let (active, reason) = viewer.request_control(id, true);
    assert!(!active && reason.as_deref().unwrap().contains("That's this Mac"), "{reason:?}");
}

fn start_host(dir: &Path, backend: Backend, ttl: Option<Duration>) -> Peer {
    let (tx, events) = channel();
    let tx = Mutex::new(tx);
    let options = CoreOptions {
        data_dir: dir.to_path_buf(),
        port: 0,
        video: false,
        backend,
        allow_same_mac_control: true,
        guard_pid: None,
        control_ttl: ttl,
    };
    let core = Core::start_with(Arc::new(move |e| drop(tx.lock().unwrap().send(e))), options).unwrap();
    Peer { core, events, pending: RefCell::new(Vec::new()) }
}

/// Like [`setup`] but with a host built by `host`.
fn setup_with(name: &str, host: impl FnOnce(&Path, &Path) -> Peer) -> Setup {
    let root = std::env::temp_dir().join(format!("lankvm-control-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (host_dir, host_fp) = device(&root, "host");
    let (viewer_dir, viewer_fp) = device(&root, "viewer");
    trust(&host_dir, "trusted-viewers.txt", &[(&viewer_fp, "viewer")]);
    trust(&viewer_dir, "trusted-hosts.txt", &[(&host_fp, "host")]);
    let record = root.join("injected.jsonl");
    let host = host(&host_dir, &record);
    let viewer = Peer::start(&viewer_dir, Backend::Hid);
    Setup { _root: TempDir(root), host, viewer, record, viewer_dir, host_fp }
}

#[test]
fn stale_answers_are_ignored() {
    let s = setup("stale");
    let id = s.viewer.connect(&s.host);
    // on, off, on back to back: only the last answer counts, and it's "controlling".
    s.viewer.core.set_control(id, true, false);
    s.viewer.core.set_control(id, false, false);
    let last = s.viewer.request(id, true, false);
    assert!(last.active, "{last:?}");
    // No earlier (stale) answer reached the app.
    assert!(s.viewer.pending.borrow().iter().all(|e| !matches!(e, Event::Control { .. })));
    s.viewer.core.send_input(id, InputMsg::Key { code: 9, down: true, repeat: false });
    assert_eq!(kinds(&recorded(&s.record, 1)), ["keydown9"]);
}

#[test]
fn missing_permission_is_reported_with_its_reason() {
    let s = setup_with("deny", |dir, _| start_host(dir, Backend::Deny, None));
    let id = s.viewer.connect(&s.host);
    let a = s.viewer.request(id, true, false);
    assert!(!a.active && a.reason == 2 && a.message.contains("Accessibility"), "{a:?}");
}

#[test]
fn control_ends_when_the_test_time_runs_out() {
    let s = setup_with("ttl", |dir, record| start_host(dir, Backend::Record(record.to_path_buf()), Some(Duration::from_millis(400))));
    let id = s.viewer.connect(&s.host);
    assert!(s.viewer.request(id, true, false).active);
    s.viewer.core.send_input(id, InputMsg::Key { code: 6, down: true, repeat: false });
    let ended = s.viewer.host_said(id);
    assert!(!ended.active && ended.reason == 11, "{ended:?}");
    assert_eq!(kinds(&recorded(&s.record, 2)), ["keydown6", "keyup6"]);
}

#[test]
fn a_flood_of_presses_ends_control() {
    let s = setup("flood");
    let id = s.viewer.connect(&s.host);
    s.viewer.request_control(id, true);
    // A runaway loop: thousands of presses a second.
    let start = Instant::now();
    let mut i = 0u32;
    while start.elapsed() < Duration::from_millis(2600) {
        for _ in 0..50 {
            let code = (i % 40) as u16;
            s.viewer.core.send_input(id, InputMsg::Key { code, down: true, repeat: false });
            s.viewer.core.send_input(id, InputMsg::Key { code, down: false, repeat: false });
            i += 1;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let ended = s.viewer.host_said(id);
    assert!(!ended.active && ended.reason == 9, "{ended:?}");
    // The host gave up on that input stream; asking again works, over a new one.
    std::thread::sleep(Duration::from_millis(200));
    let before = recorded(&s.record, 0).len();
    assert!(s.viewer.request(id, true, false).active);
    s.viewer.core.send_input(id, InputMsg::Key { code: 50, down: true, repeat: false });
    let after = recorded(&s.record, before + 1);
    assert_eq!(kinds(&after[before..]), ["keydown50"]);
}

#[test]
fn quitting_the_host_releases_held_input() {
    let s = setup("shutdown");
    let id = s.viewer.connect(&s.host);
    s.viewer.request_control(id, true);
    s.viewer.core.send_input(id, InputMsg::Key { code: 7, down: true, repeat: false });
    assert_eq!(recorded(&s.record, 1).len(), 1);
    s.host.core.shutdown();
    assert_eq!(kinds(&recorded(&s.record, 2)), ["keydown7", "keyup7"]);
}

#[test]
fn garbage_on_the_input_stream_ends_control_but_not_the_session() {
    use std::io::Write as _;
    // Speak raw QUIC to the host: handshake as the trusted viewer, ask for control, then send
    // nonsense on the input stream.
    let s = setup("garbage");
    let host_port = s.host.core.this_mac().port;
    let identity = DeviceIdentity::load_or_create(&s.viewer_dir).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (active, ended) = rt.block_on(async move {
        let endpoint = transport::endpoint::make_endpoint("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let conn = endpoint.connect(format!("127.0.0.1:{host_port}").parse().unwrap(), "lankvm").unwrap().await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let hello = protocol::ClientMsg::Hello {
            version: protocol::PROTOCOL_VERSION,
            device_name: "raw".into(),
            max_width: 100,
            max_height: 100,
            fps: 30,
            trusts_host: true,
        };
        transport::framing::write_msg(&mut send, &hello).await.unwrap();
        let welcome: protocol::HostMsg = transport::framing::read_msg(&mut recv).await.unwrap().unwrap();
        assert!(matches!(welcome, protocol::HostMsg::Welcome { .. }), "{welcome:?}");
        transport::framing::write_msg(&mut send, &protocol::ClientMsg::SetControl { on: true, request: 1, take_over: false }).await.unwrap();
        let mut answers = Vec::new();
        loop {
            match transport::framing::read_msg::<protocol::HostMsg>(&mut recv).await.unwrap().unwrap() {
                protocol::HostMsg::Control(c) => {
                    answers.push(c.clone());
                    if c.active {
                        break;
                    }
                }
                _ => {}
            }
        }
        let mut input = conn.open_uni().await.unwrap();
        let mut junk = Vec::new();
        junk.write_all(&9999u32.to_le_bytes()).unwrap(); // longer than any input message
        junk.extend_from_slice(&[0xAB; 64]);
        input.write_all(&junk).await.unwrap();
        let ended = loop {
            match transport::framing::read_msg::<protocol::HostMsg>(&mut recv).await.unwrap().unwrap() {
                protocol::HostMsg::Control(c) if !c.active => break c,
                _ => {}
            }
        };
        // The session (video, control stream) is still up: a ping is answered.
        transport::framing::write_msg(&mut send, &protocol::ClientMsg::Ping { client_time_us: 1 }).await.unwrap();
        loop {
            if let protocol::HostMsg::Pong { .. } = transport::framing::read_msg::<protocol::HostMsg>(&mut recv).await.unwrap().unwrap() {
                break;
            }
        }
        (answers.last().unwrap().active, ended)
    });
    assert!(active);
    assert_eq!(ended.reason, protocol::ControlReason::BAD_INPUT, "{ended:?}");
    assert!(std::fs::read_to_string(&s.record).unwrap_or_default().is_empty(), "nothing injected");
}

#[test]
fn a_viewer_spamming_control_requests_is_disconnected() {
    let s = setup("spam");
    let id = s.viewer.connect(&s.host);
    // Each request costs the host real work; no person toggles this fast.
    for i in 0..200 {
        s.viewer.core.set_control(id, i % 2 == 0, false);
    }
    s.viewer.wait("session ended", |e| match e {
        Event::Ended { session, .. } if session == id => Ok(()),
        e => Err(e),
    });
}

#[test]
fn input_that_went_around_a_loop_of_macs_dies_out() {
    let s = setup("relay");
    let id = s.viewer.connect(&s.host);
    assert!(s.viewer.request(id, true, false).active);
    // Relayed once (a Mac controlling the viewer's Mac typed it): injected, stamped with depth 2.
    s.viewer.core.send_input(id, InputMsg::Relayed { depth: 1 });
    s.viewer.core.send_input(id, InputMsg::Key { code: 1, down: true, repeat: false });
    // Past the limit (it has been around a loop): dropped, though releases still apply.
    s.viewer.core.send_input(id, InputMsg::Relayed { depth: protocol::MAX_RELAY_DEPTH + 1 });
    s.viewer.core.send_input(id, InputMsg::Key { code: 2, down: true, repeat: false });
    s.viewer.core.send_input(id, InputMsg::ReleaseAll);
    // Made on the viewer's own Mac again.
    s.viewer.core.send_input(id, InputMsg::Relayed { depth: 0 });
    s.viewer.core.send_input(id, InputMsg::Key { code: 3, down: true, repeat: false });
    let events = recorded(&s.record, 3);
    assert_eq!(kinds(&events), ["keydown1", "keyup1", "keydown3"], "{events:#?}");
    assert_eq!(events[0]["depth"], 1);
    assert!(events[2].get("depth").is_none());
}
