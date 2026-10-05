//! Internet access end to end over real QUIC on loopback. Both cores treat loopback as the
//! internet (`set_loopback_is_internet`), so connections between them take the internet path:
//! the gate, knocks, and the checks around them. Nothing is asked of the router. LanKVM servers
//! are in-process ones (`transport::test_server`): no test reaches the real one.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lankvm_core::control::Backend;
use lankvm_core::{AudioBackend, ClipboardBackend, Core, CoreOptions, Event};
use protocol::InputMsg;
use serde_json::{Value, json};
use transport::identity::DeviceIdentity;
use transport::rendezvous::{Message, RendezvousId};
use transport::test_server::TestServer;

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
    /// A core with no LanKVM server.
    fn start(dir: &Path, backend: Backend) -> Self {
        Self::start_with(dir, backend, "", false)
    }

    /// A core registering with LanKVM server `rendezvous` as a host ("" for none). With
    /// `force_relay`, it reaches paired hosts it connects to by fingerprint through their
    /// server's relay alone.
    fn start_with(dir: &Path, backend: Backend, rendezvous: &str, force_relay: bool) -> Self {
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
            rendezvous: Some(rendezvous.to_string()),
            force_relay,
            // Never the user's clipboard.
            clipboard: ClipboardBackend::Off,
            // Never the user's microphone.
            audio: AudioBackend::OFF,
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
        self.connect_relayed(target, timeout).map(|(id, internet, _)| (id, internet))
    }

    /// [`Self::connect`], also saying whether the session goes through a LanKVM server's relay.
    fn connect_relayed(&self, target: &str, timeout: Duration) -> Result<(u64, bool, bool), String> {
        let id = self.core.connect(target, (1920, 1080), 60);
        self.wait("connected or ended", timeout, |e| match e {
            Event::Connected { session, info } if session == id => Ok(Ok((id, info.internet, info.relayed))),
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
    setup_with(name, internet_access, "")
}

/// [`setup`], with the host registering with LanKVM server `rendezvous` ("" for none).
fn setup_with(name: &str, internet_access: bool, rendezvous: &str) -> Setup {
    let root = std::env::temp_dir().join(format!("lankvm-internet-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (host_dir, host_fp) = device(&root, "host");
    let (viewer_dir, viewer_fp) = device(&root, "viewer");
    trust(&host_dir, "trusted-viewers.txt", &[(&viewer_fp, "viewer")]);
    trust(&viewer_dir, "trusted-hosts.txt", &[(&host_fp, "host")]);
    let host = Peer::start_with(&host_dir, Backend::Record(root.join("injected.jsonl")), rendezvous, false);
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
    // The address is the host's, known from before, so the message names it.
    assert!(error.starts_with(&format!("No answer from host at {target}.")), "{error}");
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

/// The in-process LanKVM server, on a runtime of its own (these tests are synchronous).
struct Server {
    server: TestServer,
    _rt: tokio::runtime::Runtime,
}

impl Server {
    fn start() -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server = rt.block_on(TestServer::start()).unwrap();
        Self { server, _rt: rt }
    }

    fn addr(&self) -> String {
        self.server.addr().to_string()
    }
}

/// A host registered with `server`, and a viewer that knows no way to it but the server: it
/// learned the server and the host's ID in a session over the internet, then started again with
/// the host's addresses forgotten. With `force_relay`, the viewer goes through the relay alone.
/// Also the host's ID at the server.
fn introduced(name: &str, server: &Server, force_relay: bool) -> (Setup, RendezvousId) {
    let Setup { root, host, viewer, viewer_dir, host_fp, viewer_fp } = setup_with(name, true, &server.addr());
    wait_until("the host to register", || host.status()["internet"]["server"]["state"] == "registered");
    let (session, _) = viewer.connect(&host.target(), WAIT).expect("connect over the internet");
    wait_until("the viewer to learn the server", || internet_host(&viewer_dir, &host_fp)["server"] == server.addr().as_str());
    viewer.core.disconnect(session);
    wait_until("the viewer to leave", || host.status()["viewers"].as_array().unwrap().is_empty());
    let id_hex = internet_host(&viewer_dir, &host_fp)["id"].as_str().unwrap().to_string();
    let id: RendezvousId = (0..16).map(|i| u8::from_str_radix(&id_hex[2 * i..2 * i + 2], 16).unwrap()).collect::<Vec<_>>().try_into().unwrap();
    assert_eq!(server.server.registered(&id), Some(host.target().parse().unwrap()));

    drop(viewer);
    let path = viewer_dir.join("internet-hosts.json");
    let mut stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    stored[&host_fp]["announced"] = json!([]);
    stored[&host_fp]["used"] = json!([]);
    // Its address on the local network too: this Mac is on it, so that would be the way in.
    stored[&host_fp]["lan"] = json!([]);
    std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();
    let viewer = Peer::start_with(&viewer_dir, Backend::Hid, "", force_relay);
    viewer.core.set_loopback_is_internet(true);
    (Setup { root, host, viewer, viewer_dir, host_fp, viewer_fp }, id)
}

impl Setup {
    /// How Paired Devices connects to the host.
    fn paired_target(&self) -> String {
        format!("lankvm:{}", self.host_fp)
    }
}

#[test]
fn a_paired_viewer_connects_through_the_lankvm_server() {
    let server = Server::start();
    let (s, _) = introduced("rendezvous", &server, false);
    let shown = &s.host.status()["internet"]["server"];
    assert_eq!(shown["address"], server.addr().as_str());
    assert_eq!(shown["state"], "registered");
    assert_eq!(shown["observed"], s.host.target().as_str());
    // Paired Devices offers to connect, though no address is known.
    let paired = serde_json::to_value(s.viewer.core.paired_devices()).unwrap();
    assert_eq!(paired["hosts"][0]["reachable"], true, "{paired}");
    assert!(paired["hosts"][0].get("internetAddress").is_none(), "{paired}");

    let (session, internet, relayed) = s.viewer.connect_relayed(&s.paired_target(), WAIT).expect("connect through the server");
    assert!(internet && !relayed);
    wait_until("the host to list the viewer", || s.host.status()["viewers"][0]["internet"] == true);
    assert_eq!(s.host.status()["viewers"][0]["relayed"], false);
    // Punched through: from Mac to Mac, nothing through the server.
    assert_eq!((server.server.relay_sessions(), server.server.stats().relayed), (0, 0));
    // The target is what recent hosts keep; it isn't an address to remember.
    assert_eq!(s.viewer.core.recent_hosts()[0].address, s.paired_target());
    assert_eq!(internet_host(&s.viewer_dir, &s.host_fp)["used"], json!([]));
    s.viewer.core.disconnect(session);
}

#[test]
fn a_public_ip_this_mac_doesnt_know_reaches_the_host_through_its_server() {
    let server = Server::start();
    let (s, _) = introduced("by-ip", &server, false);
    // The host's public IP with a port its router doesn't forward: a socket that never answers
    // stands in for it, so the knock there goes nowhere. The server sees the host at that IP.
    let host: std::net::SocketAddr = s.host.target().parse().unwrap();
    let unforwarded = std::net::UdpSocket::bind((host.ip(), 0)).unwrap();
    let target = format!("{}:{}", host.ip(), unforwarded.local_addr().unwrap().port());
    let (session, internet, relayed) = s.viewer.connect_relayed(&target, WAIT).expect("connect by the public IP");
    assert!(internet && !relayed);
    wait_until("the host to list the viewer", || s.host.status()["viewers"][0]["internet"] == true);
    // Remembered: next time it is a known address of the host.
    assert_eq!(internet_host(&s.viewer_dir, &s.host_fp)["used"], json!([target]));
    s.viewer.core.disconnect(session);
}

#[test]
fn a_viewer_connects_through_the_relay_and_takes_control() {
    let server = Server::start();
    let (s, _) = introduced("relay", &server, true);
    let (session, internet, relayed) = s.viewer.connect_relayed(&s.paired_target(), WAIT).expect("connect through the relay");
    assert!(internet && relayed);
    wait_until("the host to list the viewer", || s.host.status()["viewers"][0]["relayed"] == true);
    assert_eq!(s.host.status()["viewers"][0]["internet"], true);

    let request = s.viewer.core.set_control(session, true, false);
    let active = s.viewer.wait("control answer", WAIT, |e| match e {
        Event::Control { session: id, request: r, active, .. } if id == session && r == request => Ok(active),
        e => Err(e),
    });
    assert!(active);
    s.viewer.core.send_input(session, InputMsg::Key { code: 0, down: true, repeat: false });
    s.viewer.core.send_input(session, InputMsg::Key { code: 0, down: false, repeat: false });
    let record = s.root.0.join("injected.jsonl");
    wait_until("the key to reach the host", || {
        std::fs::read_to_string(&record).unwrap_or_default().lines().any(|l| l.contains("\"keyup\""))
    });
    let stats = server.server.stats();
    assert!(stats.relayed > 10 && stats.dropped == 0, "{stats:?}");
    assert_eq!(server.server.relay_sessions(), 1);
    s.viewer.core.disconnect(session);
    wait_until("the host to see the viewer leave", || s.host.status()["viewers"].as_array().unwrap().is_empty());
}

#[test]
fn turning_internet_access_off_ends_relayed_sessions_cleanly() {
    let server = Server::start();
    let (s, _) = introduced("relay-off", &server, true);
    let (session, _, relayed) = s.viewer.connect_relayed(&s.paired_target(), WAIT).expect("connect through the relay");
    assert!(relayed);
    wait_until("the host to list the viewer", || s.host.status()["viewers"][0]["relayed"] == true);

    // The host's close still goes out through the relay: the session ends at once, without an
    // error, rather than when the viewer gives up waiting (15 s).
    s.host.core.set_internet_access(false);
    let error = s.viewer.wait("session ended", WAIT, |e| match e {
        Event::Ended { session: id, error } if id == session => Ok(error),
        e => Err(e),
    });
    assert_eq!(error, None);
}

#[test]
fn the_relay_takes_over_when_punching_fails() {
    let server = Server::start();
    let (s, _) = introduced("lying", &server, false);
    // The server sends each Mac toward an address that never answers.
    server.server.set_lie_about_endpoints(true);
    let (session, internet, relayed) = s.viewer.connect_relayed(&s.paired_target(), NO_ANSWER_WAIT).expect("connect through the relay");
    assert!(internet && relayed);
    wait_until("the host to list the viewer", || s.host.status()["viewers"][0]["relayed"] == true);
    assert_eq!(server.server.relay_sessions(), 1);
    s.viewer.core.disconnect(session);
}

#[test]
fn a_host_with_internet_access_off_isnt_online_at_the_lankvm_server() {
    let server = Server::start();
    let (s, id) = introduced("offline", &server, false);
    s.host.core.set_internet_access(false);
    assert_eq!(s.host.status()["internet"]["server"]["state"], "off");
    wait_until("the host to unregister", || server.server.registered(&id).is_none());

    let started = Instant::now();
    let error = s.viewer.connect(&s.paired_target(), WAIT).expect_err("not online");
    assert_eq!(error, "host isn't reachable over the internet right now: LanKVM isn't running there, or its internet access is off.");
    assert!(started.elapsed() < Duration::from_secs(2), "the server says so at once");
}

#[test]
fn a_forgotten_viewer_gets_no_answer_through_the_lankvm_server() {
    let server = Server::start();
    let (s, _) = introduced("forgotten", &server, false);
    s.host.core.forget_device("viewer", &s.viewer_fp);

    let error = s.viewer.connect(&s.paired_target(), NO_ANSWER_WAIT).expect_err("no answer");
    assert_eq!(error, "host didn't answer through the LanKVM server.");
    assert!(s.host.status()["viewers"].as_array().unwrap().is_empty());
    assert_eq!(server.server.relay_sessions(), 0);
}

#[test]
fn a_lankvm_server_that_is_down_is_named() {
    let server = Server::start();
    let (s, _) = introduced("down", &server, false);
    let addr = server.addr();
    drop(server);

    // Nothing answers, not even a request any running server answers: the server is down, not
    // the host quiet. (The host notices at its next keepalive, 20 s on.)
    let error = s.viewer.connect(&s.paired_target(), NO_ANSWER_WAIT).expect_err("no server");
    assert_eq!(error, format!("Couldn't reach the LanKVM server at {addr}."));
}

#[test]
fn a_lankvm_server_on_the_local_network_isnt_used() {
    let server = Server::start();
    let (s, _) = introduced("local-server", &server, false);
    // To the viewer, the server (on loopback) is on the local network now: it can't introduce
    // Macs over the internet, and while in use it would keep that address's packets from quinn.
    s.viewer.core.set_loopback_is_internet(false);
    let started = Instant::now();
    let error = s.viewer.connect(&s.paired_target(), WAIT).expect_err("not used");
    assert_eq!(error, format!("host uses a LanKVM server at {}, which isn't on the internet.", server.addr()));
    assert!(started.elapsed() < Duration::from_secs(2), "fails at once, without asking");
}

#[test]
fn a_host_answers_only_the_challenges_it_asked_for() {
    let root = std::env::temp_dir().join(format!("lankvm-internet-challenge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _cleanup = TempDir(root.clone());
    let (dir, _) = device(&root, "host");
    // A server played by hand, to send what a real one wouldn't.
    let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    server.set_read_timeout(Some(WAIT)).unwrap();
    let host = Peer::start_with(&dir, Backend::Hid, &server.local_addr().unwrap().to_string(), false);
    host.core.set_loopback_is_internet(true);
    host.core.set_internet_access(true);
    let mut buf = [0u8; 2048];
    let mut receive = || {
        let (len, from) = server.recv_from(&mut buf).expect("a request");
        (Message::decode(&buf[..len]).expect("a message"), from)
    };
    let (Message::RegisterBegin { .. }, host_addr) = receive() else { panic!("not a REGISTER_BEGIN") };
    server.send_to(&Message::Challenge { cookie: [1; 16] }.encode(), host_addr).unwrap();
    let (Message::Register { cookie: [1, ..], .. }, _) = receive() else { panic!("not a REGISTER with the cookie") };
    server.send_to(&Message::Registered { ttl_secs: 75, observed: host_addr }.encode(), host_addr).unwrap();
    wait_until("the host to register", || host.status()["internet"]["server"]["state"] == "registered");

    // A challenge out of the blue, as anyone passing for the server could send: no registering
    // with its cookie, and the card still says registered.
    server.send_to(&Message::Challenge { cookie: [2; 16] }.encode(), host_addr).unwrap();
    server.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let sent = server.recv_from(&mut buf).ok().and_then(|(len, _)| Message::decode(&buf[..len]));
    assert!(sent.is_none(), "{sent:?}");
    assert_eq!(host.status()["internet"]["server"]["state"], "registered");
}

#[test]
fn a_mac_with_no_way_to_a_paired_host_says_so() {
    let s = setup("no-way", false);
    let started = Instant::now();
    let error = s.viewer.connect(&format!("lankvm:{}", s.host_fp), WAIT).expect_err("no way");
    assert!(error.starts_with("host hasn't told this Mac how to reach it over the internet."), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2), "fails at once, without trying");
    let paired = serde_json::to_value(s.viewer.core.paired_devices()).unwrap();
    assert_eq!(paired["hosts"][0]["reachable"], false, "{paired}");

    let error = s.viewer.connect(&format!("lankvm:{}", "ab".repeat(32)), WAIT).expect_err("not paired");
    assert!(error.starts_with("This Mac isn't paired with that Mac any more."), "{error}");
    let error = s.viewer.connect("lankvm:nonsense", WAIT).expect_err("not a fingerprint");
    assert_eq!(error, "lankvm:nonsense doesn't name a paired Mac.");
}
