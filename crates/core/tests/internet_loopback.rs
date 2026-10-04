//! Internet access end to end over real QUIC on loopback. Both cores treat loopback as the
//! internet (`set_loopback_is_internet`), so connections between them take the internet path:
//! the gate, knocks, and the checks around them. Nothing is asked of the router.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lankvm_core::control::Backend;
use lankvm_core::{Core, CoreOptions, Event};
use serde_json::Value;
use transport::identity::DeviceIdentity;

const WAIT: Duration = Duration::from_secs(5);
/// Longer than the viewer waits for an answer over the internet (10 s).
const NO_ANSWER_WAIT: Duration = Duration::from_secs(20);

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
            loopback_is_internet: false,
        };
        let core = Core::start_with(Arc::new(move |e| drop(tx.lock().unwrap().send(e))), options).unwrap();
        Self { core, events, pending: RefCell::new(Vec::new()) }
    }

    /// Waits up to `timeout` for an event `pick` accepts (returns Ok); keeps the others for later
    /// waits.
    fn wait<T>(&self, what: &str, timeout: Duration, mut pick: impl FnMut(Event) -> Result<T, Event>) -> T {
        let earlier = std::mem::take(&mut *self.pending.borrow_mut());
        let mut found = None;
        for e in earlier {
            if found.is_some() {
                self.pending.borrow_mut().push(e);
                continue;
            }
            match pick(e) {
                Ok(v) => found = Some(v),
                Err(e) => self.pending.borrow_mut().push(e),
            }
        }
        if let Some(v) = found {
            return v;
        }
        let deadline = Instant::now() + timeout;
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

    /// Connects to `target` and waits for the session to start (Ok: its id and whether it went
    /// over the internet) or end (Err: the error shown).
    fn connect(&self, target: &str, timeout: Duration) -> Result<(u64, bool), String> {
        let id = self.core.connect(target, (1920, 1080), 60);
        self.wait("connected or ended", timeout, |e| match e {
            Event::Connected { session, info } if session == id => Ok(Ok((id, info.internet))),
            Event::Ended { session, error } if session == id => Ok(Err(error.unwrap_or_default())),
            e => Err(e),
        })
    }

    fn ended(&self, id: u64) {
        self.wait("session ended", WAIT, |e| match e {
            Event::Ended { session, .. } if session == id => Ok(()),
            e => Err(e),
        });
    }

    fn trust_changed(&self) {
        self.wait("trust changed", WAIT, |e| match e {
            Event::TrustChanged => Ok(()),
            e => Err(e),
        });
    }

    fn status(&self) -> Value {
        serde_json::to_value(self.core.host_status()).unwrap()
    }

    fn target(&self) -> String {
        format!("127.0.0.1:{}", self.core.this_mac().port)
    }
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

/// What the viewer in `dir` stored about reaching `host` over the internet.
fn internet_host(dir: &Path, host: &str) -> Value {
    let json: Value = serde_json::from_slice(&std::fs::read(dir.join("internet-hosts.json")).unwrap()).unwrap();
    json[host].clone()
}

/// Polls `check` until it holds.
fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Setup {
    root: TempDir,
    host: Peer,
    viewer: Peer,
    viewer_dir: PathBuf,
    host_fp: String,
    viewer_fp: String,
}

/// A host and a viewer paired with each other, after one session on the local network (where
/// the viewer got its key), both now treating loopback as the internet. Internet access on the
/// host is as `internet_access` says.
fn setup(name: &str, internet_access: bool) -> Setup {
    let root = std::env::temp_dir().join(format!("lankvm-internet-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (host_dir, host_fp) = device(&root, "host");
    let (viewer_dir, viewer_fp) = device(&root, "viewer");
    trust(&host_dir, "trusted-viewers.txt", &[(&viewer_fp, "viewer")]);
    trust(&viewer_dir, "trusted-hosts.txt", &[(&host_fp, "host")]);
    let host = Peer::start(&host_dir, Backend::Record(root.join("injected.jsonl")));
    let viewer = Peer::start(&viewer_dir, Backend::Hid);

    let (id, internet) = viewer.connect(&host.target(), WAIT).expect("connect on the local network");
    assert!(!internet);
    // The key comes right after the session starts, and it's new to the viewer.
    viewer.trust_changed();
    let stored = internet_host(&viewer_dir, &host_fp);
    assert_eq!(stored["key"].as_str().map(str::len), Some(64), "{stored}");
    assert_eq!(stored["announced"], serde_json::json!([]), "internet access is still off");
    viewer.core.disconnect(id);
    wait_until("the viewer to leave", || host.status()["viewers"].as_array().unwrap().is_empty());

    host.core.set_loopback_is_internet(true);
    viewer.core.set_loopback_is_internet(true);
    if internet_access {
        host.core.set_internet_access(true);
        wait_until("the host to be reachable", || host.status()["internet"]["state"] == "public");
    }
    Setup { root: TempDir(root), host, viewer, viewer_dir, host_fp, viewer_fp }
}

#[test]
fn a_paired_viewer_connects_over_the_internet() {
    let s = setup("connects", true);
    let target = s.host.target();
    let internet = &s.host.status()["internet"];
    assert_eq!(internet["enabled"], true);
    assert_eq!(internet["externalAddress"], target.as_str());
    assert_eq!(internet["announced"], serde_json::json!([target]));
    assert_eq!(internet["problem"], Value::Null);

    let (id, over_internet) = s.viewer.connect(&target, WAIT).expect("connect over the internet");
    assert!(over_internet);
    wait_until("the host to list the viewer", || s.host.status()["viewers"][0]["internet"] == true);

    // The host said where to reach it, and the viewer remembers where it did.
    wait_until("the viewer to store the addresses", || internet_host(&s.viewer_dir, &s.host_fp)["announced"] == serde_json::json!([target]));
    assert_eq!(internet_host(&s.viewer_dir, &s.host_fp)["used"], serde_json::json!([target]));
    let paired = serde_json::to_value(s.viewer.core.paired_devices()).unwrap();
    assert_eq!(paired["hosts"][0]["internetAddress"], target.as_str());
    assert!(paired["viewers"].as_array().unwrap().is_empty());
    let host_paired = serde_json::to_value(s.host.core.paired_devices()).unwrap();
    assert!(host_paired["viewers"][0].get("internetAddress").is_none(), "{host_paired}");

    s.viewer.core.disconnect(id);
    let settings: Value = serde_json::from_slice(&std::fs::read(s.root.0.join("host/host-settings.json")).unwrap()).unwrap();
    assert_eq!(settings["internetAccess"], true);
}

#[test]
fn a_host_with_internet_access_off_doesnt_answer() {
    let s = setup("off", false);
    let target = s.host.target();
    assert_eq!(s.host.status()["internet"]["state"], "off");
    let ignored = s.host.status()["internet"]["ignored"].as_u64().unwrap();

    let error = s.viewer.connect(&target, NO_ANSWER_WAIT).expect_err("no answer");
    assert!(error.starts_with(&format!("No answer from {target}.")), "{error}");
    assert!(error.contains("internet access is on"), "{error}");
    assert!(s.host.status()["internet"]["ignored"].as_u64().unwrap() > ignored, "the gate ignored the knocks");
    assert!(s.host.status()["viewers"].as_array().unwrap().is_empty());
}

#[test]
fn a_mac_that_never_paired_has_no_key_to_knock_with() {
    let s = setup("stranger", true);
    let (stranger_dir, _) = device(&s.root.0, "stranger");
    let stranger = Peer::start(&stranger_dir, Backend::Hid);
    stranger.core.set_loopback_is_internet(true);
    let target = s.host.target();

    let started = Instant::now();
    let error = stranger.connect(&target, WAIT).expect_err("not paired");
    assert!(error.starts_with(&format!("This Mac hasn't paired with a Mac at {target} yet.")), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2), "fails at once, without trying");
    assert!(s.host.status()["viewers"].as_array().unwrap().is_empty());
}

#[test]
fn forgetting_a_viewer_revokes_its_internet_access() {
    let s = setup("forget", true);
    let target = s.host.target();
    let (id, over_internet) = s.viewer.connect(&target, WAIT).expect("connect over the internet");
    assert!(over_internet);

    s.host.core.forget_device("viewer", &s.viewer_fp);
    s.viewer.ended(id);
    let error = s.viewer.connect(&target, NO_ANSWER_WAIT).expect_err("no answer once forgotten");
    assert!(error.starts_with(&format!("No answer from {target}.")), "{error}");
}

#[test]
fn turning_internet_access_off_ends_internet_sessions() {
    let s = setup("turn-off", true);
    let (id, over_internet) = s.viewer.connect(&s.host.target(), WAIT).expect("connect over the internet");
    assert!(over_internet);
    wait_until("the host to list the viewer", || !s.host.status()["viewers"].as_array().unwrap().is_empty());

    s.host.core.set_internet_access(false);
    s.viewer.ended(id);
    wait_until("the viewer to leave", || s.host.status()["viewers"].as_array().unwrap().is_empty());
    let internet = &s.host.status()["internet"];
    assert_eq!((&internet["enabled"], &internet["state"]), (&Value::Bool(false), &Value::from("off")));
    assert_eq!(internet["announced"], serde_json::json!([]));
}

#[test]
fn knocks_go_only_where_the_host_may_be() {
    let s = setup("elsewhere", true);
    let target = s.host.target();
    let (id, over_internet) = s.viewer.connect(&target, WAIT).expect("connect over the internet");
    assert!(over_internet);
    s.viewer.core.disconnect(id);

    // The host is known at 127.0.0.1 now, so its key isn't sent anywhere else: whoever is
    // there could replay the knock to it.
    let elsewhere = format!("127.0.0.2:{}", s.host.core.this_mac().port);
    let started = Instant::now();
    let error = s.viewer.connect(&elsewhere, WAIT).expect_err("no knock elsewhere");
    assert!(error.starts_with(&format!("This Mac doesn't know {elsewhere} as the address of a Mac it paired with.")), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2), "fails at once, without trying");
}

#[test]
fn a_session_from_the_local_network_ends_when_it_moves_to_the_internet() {
    let s = setup("moves", false);
    s.host.core.set_loopback_is_internet(false);
    s.viewer.core.set_loopback_is_internet(false);
    let (id, over_internet) = s.viewer.connect(&s.host.target(), WAIT).expect("connect on the local network");
    assert!(!over_internet);
    wait_until("the host to list the viewer", || !s.host.status()["viewers"].as_array().unwrap().is_empty());

    // To the host, the viewer is now on the internet, as when QUIC follows it to another network.
    s.host.core.set_loopback_is_internet(true);
    s.viewer.ended(id);
    wait_until("the viewer to leave", || s.host.status()["viewers"].as_array().unwrap().is_empty());
}

#[test]
fn a_host_that_takes_the_knock_but_not_the_certificate_turns_the_viewer_away() {
    let Setup { root: _root, host, viewer, viewer_dir, .. } = setup("new-identity", true);
    // The viewer gets a new identity, but keeps its key and still trusts the host.
    drop(viewer);
    for file in ["identity-cert.der", "identity-key.der"] {
        std::fs::remove_file(viewer_dir.join(file)).unwrap();
    }
    let viewer = Peer::start(&viewer_dir, Backend::Hid);
    viewer.core.set_loopback_is_internet(true);

    let target = host.target();
    let error = viewer.connect(&target, WAIT).expect_err("turned away");
    assert!(error.starts_with(&format!("The Mac at {target} turned this Mac away.")), "{error}");
    assert!(host.status()["viewers"].as_array().unwrap().is_empty());
}
