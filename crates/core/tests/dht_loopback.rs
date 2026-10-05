//! Meeting through the BitTorrent DHT end to end, with no LanKVM server: two cores on loopback
//! (which stands in for the internet), a DHT of in-process nodes (`transport::test_dht`), and a
//! viewer that knows about the host only the key it got on the local network and an address on
//! the internet that leads nowhere (a router that doesn't forward the port).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lankvm_core::control::Backend;
use lankvm_core::{AudioBackend, ClipboardBackend, Core, CoreOptions, Event};
use serde_json::{Value, json};
use transport::identity::DeviceIdentity;
use transport::test_dht::TestDht;

const WAIT: Duration = Duration::from_secs(5);
/// The DHT's part: the host polls every 2 s, then punches and answers.
const DHT_WAIT: Duration = Duration::from_secs(25);
/// Longer than the host's poll interval: anything it still sends to the DHT comes within this.
const POLL_QUIET: Duration = Duration::from_millis(2500);

struct Peer {
    core: Arc<Core>,
    events: Receiver<Event>,
    pending: RefCell<Vec<Event>>,
}

impl Peer {
    /// A core with no LanKVM server, joining the DHT through `bootstrap` (none: no DHT).
    fn start(dir: &Path, bootstrap: &[String]) -> Self {
        let (tx, events) = channel();
        let tx = Mutex::new(tx);
        let options = CoreOptions {
            data_dir: dir.to_path_buf(),
            port: 0,
            video: false,
            backend: Backend::Record(dir.join("injected.jsonl")),
            allow_same_mac_control: true,
            guard_pid: None,
            control_ttl: None,
            loopback_is_internet: false,
            rendezvous: Some(String::new()),
            force_relay: false,
            dht_bootstrap: Some(bootstrap.to_vec()),
            clipboard: ClipboardBackend::Off,
            audio: AudioBackend::OFF,
        };
        let core = Core::start_with(Arc::new(move |e| drop(tx.lock().unwrap().send(e))), options).unwrap();
        Self { core, events, pending: RefCell::new(Vec::new()) }
    }

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

    /// Connects to `target`: Ok (session id, over the internet, relayed) or Err (the error shown).
    fn connect(&self, target: &str, timeout: Duration) -> Result<(u64, bool, bool), String> {
        let id = self.core.connect(target, (1920, 1080), 60);
        self.wait("connected or ended", timeout, |e| match e {
            Event::Connected { session, info } if session == id => Ok(Ok((id, info.internet, info.relayed))),
            Event::Ended { session, error } if session == id => Ok(Err(error.unwrap_or_default())),
            e => Err(e),
        })
    }

    fn status(&self) -> Value {
        serde_json::to_value(self.core.host_status()).unwrap()
    }

    fn target(&self) -> String {
        format!("127.0.0.1:{}", self.core.this_mac().port)
    }
}

fn device(root: &Path, name: &str) -> (PathBuf, String) {
    let dir = root.join(name);
    let id = DeviceIdentity::load_or_create(&dir).unwrap();
    (dir, id.fingerprint.iter().map(|b| format!("{b:02x}")).collect())
}

fn trust(dir: &Path, file: &str, entries: &[(&str, &str)]) {
    let text: String = entries.iter().map(|(fp, name)| format!("{fp} {name}\n")).collect();
    std::fs::write(dir.join(file), text).unwrap();
}

fn stored(dir: &Path, file: &str) -> Value {
    std::fs::read(dir.join(file)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or(Value::Null)
}

fn wait_until(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The test DHT, on a runtime of its own (these tests are synchronous).
struct Net {
    dht: TestDht,
    _rt: tokio::runtime::Runtime,
}

impl Net {
    fn start(nodes: usize) -> Self {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dht = rt.block_on(TestDht::start(nodes)).unwrap();
        Self { dht, _rt: rt }
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Setup {
    _root: TempDir,
    host: Peer,
    viewer: Peer,
    viewer_dir: PathBuf,
    host_fp: String,
    viewer_fp: String,
    /// Where the viewer thinks the host is on the internet: a socket that never answers.
    _dead: std::net::UdpSocket,
}

impl Setup {
    fn paired_target(&self) -> String {
        format!("lankvm:{}", self.host_fp)
    }
}

/// A host with internet access on (and the DHT option as `host_dht` says) and no LanKVM server,
/// and a paired viewer that got its key in one session on the local network, then forgot every
/// address of the host's: the DHT is the only way left. With `had_internet`, the viewer
/// remembers the host as on the internet, at an address that leads nowhere (the host had
/// internet access on last time, behind a router that doesn't forward the port); without, as a
/// Mac only ever met on the local network. Both treat loopback as the internet.
fn setup(name: &str, net: &Net, host_dht: bool) -> Setup {
    setup_with(name, net, host_dht, true)
}

fn setup_with(name: &str, net: &Net, host_dht: bool, had_internet: bool) -> Setup {
    let root = std::env::temp_dir().join(format!("lankvm-dht-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (host_dir, host_fp) = device(&root, "host");
    let (viewer_dir, viewer_fp) = device(&root, "viewer");
    trust(&host_dir, "trusted-viewers.txt", &[(&viewer_fp, "viewer")]);
    trust(&viewer_dir, "trusted-hosts.txt", &[(&host_fp, "host")]);
    let bootstrap = net.dht.bootstrap();
    let host = Peer::start(&host_dir, &bootstrap);
    host.core.set_dht(host_dht);
    let viewer = Peer::start(&viewer_dir, &bootstrap);

    let (id, internet, _) = viewer.connect(&host.target(), WAIT).expect("connect on the local network");
    assert!(!internet);
    let host_entry = || stored(&viewer_dir, "internet-hosts.json")[&host_fp].clone();
    wait_until("the viewer to store its key", WAIT, || !host_entry()["key"].is_null());
    viewer.core.disconnect(id);
    wait_until("the viewer to leave", WAIT, || host.status()["viewers"].as_array().unwrap().is_empty());
    drop(viewer);

    // Forget where the host is: on the internet, and on the local network.
    let dead = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let path = viewer_dir.join("internet-hosts.json");
    let mut hosts: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    hosts[&host_fp]["announced"] = if had_internet { json!([dead.local_addr().unwrap().to_string()]) } else { json!([]) };
    hosts[&host_fp]["used"] = json!([]);
    hosts[&host_fp]["lan"] = json!([]);
    std::fs::write(&path, serde_json::to_vec(&hosts).unwrap()).unwrap();
    let _ = std::fs::remove_file(viewer_dir.join("address-book.json"));

    host.core.set_loopback_is_internet(true);
    host.core.set_internet_access(true);
    let viewer = Peer::start(&viewer_dir, &bootstrap);
    viewer.core.set_loopback_is_internet(true);
    Setup { _root: TempDir(root), host, viewer, viewer_dir, host_fp, viewer_fp, _dead: dead }
}

#[test]
fn a_paired_mac_is_found_through_the_dht_with_no_server() {
    let net = Net::start(24);
    let s = setup("found", &net, true);
    wait_until("the host's notes to be on the DHT", DHT_WAIT, || s.host.status()["internet"]["dht"]["state"] == "listed");
    let dht = &s.host.status()["internet"]["dht"];
    assert_eq!(dht["enabled"], true);
    assert_eq!(dht["observed"], s.host.target().as_str(), "DHT nodes say where they see it: {dht}");
    assert_eq!(dht["symmetric"], false);

    // Only once the host noticed the request: it punches toward the viewer, and answers in its
    // note. (On loopback the address in its note would work straight away.)
    s.viewer.core.set_dht_wait_for_answer(true);
    let started = Instant::now();
    let (_, internet, relayed) = s.viewer.connect(&s.paired_target(), DHT_WAIT).expect("connect through the DHT");
    assert!(internet && !relayed);
    eprintln!("connected through the test DHT in {} ms", started.elapsed().as_millis());
    wait_until("the host to list the viewer", WAIT, || s.host.status()["viewers"][0]["internet"] == true);
    assert!(net.dht.puts() > 0 && net.dht.gets() > 0);
    // Nodes of a test DHT aren't kept for the next start: they'd be no use on the real one.
    assert!(!s.viewer_dir.join("dht-nodes.txt").exists());

    // With no paired Mac left to keep a note for, there's nothing to do on the DHT.
    s.host.core.forget_device("viewer", &s.viewer_fp);
    wait_until("the host to have no notes to keep", WAIT, || s.host.status()["internet"]["dht"]["state"] == "idle");
}

#[test]
fn the_address_in_the_hosts_note_is_tried_at_once() {
    let net = Net::start(24);
    let s = setup("at-once", &net, true);
    wait_until("the host's notes to be on the DHT", DHT_WAIT, || s.host.status()["internet"]["dht"]["state"] == "listed");
    // The host stops watching for requests (its note stays on the DHT a while): only the
    // address in the note, tried straight away, gets the viewer in.
    s.host.core.set_dht(false);
    let (_, internet, relayed) = s.viewer.connect(&s.paired_target(), DHT_WAIT).expect("connect at the note's address");
    assert!(internet && !relayed);
}

#[test]
fn a_host_with_the_dht_off_isnt_found_there() {
    let net = Net::start(16);
    let s = setup("off", &net, false);
    assert_eq!(s.host.status()["internet"]["dht"]["state"], "off");
    let gets = net.dht.gets();
    let error = s.viewer.connect(&s.paired_target(), DHT_WAIT).expect_err("nothing to find");
    assert!(net.dht.gets() > gets, "the viewer looked");
    // What the address it knows came to is the news, not that the host isn't on the DHT (it may
    // never use it).
    assert!(error.contains("No answer from"), "{error}");
}

#[test]
fn a_mac_only_ever_met_on_the_local_network_isnt_looked_for_there() {
    let net = Net::start(16);
    let s = setup_with("lan-only", &net, true, false);
    let queries = net.dht.queries();
    std::thread::sleep(POLL_QUIET);
    let host_polls = net.dht.queries() - queries;
    let queries = net.dht.queries();
    let error = s.viewer.connect(&s.paired_target(), DHT_WAIT).expect_err("no way known");
    assert!(error.contains("doesn't know where"), "{error}");
    assert!(net.dht.queries() - queries <= host_polls + 4, "the viewer looked on the DHT");
}

#[test]
fn a_viewer_with_the_dht_off_doesnt_look_there() {
    let net = Net::start(16);
    let s = setup("viewer-off", &net, true);
    wait_until("the host's notes to be on the DHT", DHT_WAIT, || s.host.status()["internet"]["dht"]["state"] == "listed");
    // The host's own polling stops, so whatever the DHT hears after is the viewer's.
    s.host.core.set_dht(false);
    std::thread::sleep(POLL_QUIET);
    s.viewer.core.set_dht(false);
    let queries = net.dht.queries();
    let error = s.viewer.connect(&s.paired_target(), DHT_WAIT).expect_err("no way left");
    assert!(error.contains("No answer from"), "{error}");
    assert_eq!(net.dht.queries(), queries, "the viewer asked the DHT");
}

#[test]
fn turning_internet_access_off_takes_the_host_off_the_dht() {
    let net = Net::start(16);
    let s = setup("access-off", &net, true);
    wait_until("the host's notes to be on the DHT", DHT_WAIT, || s.host.status()["internet"]["dht"]["state"] == "listed");
    s.host.core.set_internet_access(false);
    assert_eq!(s.host.status()["internet"]["dht"]["state"], "off");
    // It stops watching the DHT at once.
    std::thread::sleep(POLL_QUIET);
    let queries = net.dht.queries();
    std::thread::sleep(POLL_QUIET);
    assert_eq!(net.dht.queries(), queries, "the host still asks the DHT");
    // Its note is still there, but nobody answers the request, and its gate is shut.
    let error = s.viewer.connect(&s.paired_target(), DHT_WAIT).expect_err("internet access is off");
    assert!(error.contains("didn't answer"), "{error}");
}
