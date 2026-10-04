//! The Sunshine engine's refusals end to end over real QUIC on loopback: a viewer core asks a
//! host core for the Sunshine engine where Sunshine isn't installed (and Moonlight isn't either),
//! and the session carries on with LanKVM's own engine. Needs no permissions, starts no Sunshine
//! or Moonlight, and touches neither the screen nor the user's mouse and keyboard.
//!
//! One test in its own process: it points `LANKVM_SUNSHINE_BIN` and `LANKVM_MOONLIGHT_BIN` at
//! files that don't exist before any core starts.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lankvm_core::control::Backend;
use lankvm_core::{Core, CoreOptions, Event, MoonlightOptions};
use protocol::{DisplayChoice, Engine, SunshineInfo};
use transport::identity::DeviceIdentity;

const WAIT: Duration = Duration::from_secs(5);

struct Peer {
    core: Arc<Core>,
    events: Receiver<Event>,
    pending: Vec<Event>,
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
        Self { core, events, pending: Vec::new() }
    }

    /// Waits for an event `pick` accepts; keeps the others for later waits.
    fn wait<T>(&mut self, what: &str, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
        if let Some((i, v)) = self.pending.iter().enumerate().find_map(|(i, e)| pick(e).map(|v| (i, v))) {
            self.pending.remove(i);
            return v;
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| panic!("timed out waiting for {what}"));
            let e = self.events.recv_timeout(left).unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if let Event::Ended { error, .. } = &e {
                panic!("session ended while waiting for {what}: {error:?}");
            }
            match pick(&e) {
                Some(v) => return v,
                None => self.pending.push(e),
            }
        }
    }

    fn engine(&mut self, id: u64, request: u32) -> (&'static str, Option<u16>, String, Option<SunshineInfo>) {
        self.wait("engine answer", |e| match e {
            Event::Engine { session, request: r, engine, port, message, sunshine } if *session == id && *r == request => {
                Some((*engine, *port, message.clone(), sunshine.clone()))
            }
            _ => None,
        })
    }
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn device(root: &Path, name: &str) -> (PathBuf, String) {
    let dir = root.join(name);
    let id = DeviceIdentity::load_or_create(&dir).unwrap();
    (dir, id.fingerprint.iter().map(|b| format!("{b:02x}")).collect())
}

#[test]
fn sunshine_refused_where_it_isnt_installed_and_lankvm_streams_on() {
    let root = std::env::temp_dir().join(format!("lankvm-sunshine-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let _cleanup = TempDir(root.clone());
    // SAFETY: before any core (or other thread of this test process) starts.
    unsafe {
        std::env::set_var("LANKVM_SUNSHINE_BIN", root.join("no-such-sunshine"));
        std::env::set_var("LANKVM_MOONLIGHT_BIN", root.join("no-such-moonlight"));
    }
    let (host_dir, host_fp) = device(&root, "host");
    let (viewer_dir, viewer_fp) = device(&root, "viewer");
    std::fs::write(host_dir.join("trusted-viewers.txt"), format!("{viewer_fp} viewer\n")).unwrap();
    std::fs::write(viewer_dir.join("trusted-hosts.txt"), format!("{host_fp} host\n")).unwrap();
    let host = Peer::start(&host_dir, Backend::Record(root.join("injected.jsonl")));
    let mut viewer = Peer::start(&viewer_dir, Backend::Hid);

    let status = serde_json::to_value(host.core.host_status()).unwrap();
    assert_eq!(status["sunshine"], serde_json::json!({ "installed": false, "running": false, "viewer": null }));

    let id = viewer.core.connect(&format!("127.0.0.1:{}", host.core.this_mac().port), (1920, 1080), 60);
    viewer.wait("connected", |e| matches!(e, Event::Connected { session, .. } if *session == id).then_some(()));

    // Moonlight isn't installed here: refused on this Mac, the host isn't even asked.
    let request = viewer.core.set_engine(id, Engine::Sunshine, Some(MoonlightOptions::new(false)));
    assert_ne!(request, 0);
    let (engine, port, message, sunshine) = viewer.engine(id, request);
    assert_eq!((engine, port, sunshine), ("lankvm", None, None));
    assert!(message.contains("Moonlight isn't installed on this Mac"), "{message}");

    // Sunshine isn't installed on the host: it refuses, naming itself, and streams on.
    let request = viewer.core.set_engine(id, Engine::Sunshine, None);
    let (engine, port, message, sunshine) = viewer.engine(id, request);
    assert_eq!((engine, port, sunshine), ("lankvm", None, None));
    assert!(message.contains("Sunshine isn't installed on") && message.contains("install.sh"), "{message}");

    // Asking for LanKVM's own engine, which streams already, is answered at once, all well.
    let request = viewer.core.set_engine(id, Engine::LanKvm, None);
    let (engine, _, message, _) = viewer.engine(id, request);
    assert_eq!((engine, message.as_str()), ("lankvm", ""));

    // The session is as it was: control and displays still answer.
    let request = viewer.core.set_control(id, true, false);
    let active = viewer.wait("control answer", |e| match e {
        Event::Control { session, request: r, active, .. } if *session == id && *r == request => Some(*active),
        _ => None,
    });
    assert!(active, "control works on after the refusals");
    let request = viewer.core.set_display(id, DisplayChoice::Main);
    viewer.wait("display answer", |e| matches!(e, Event::Display { session, request: r, .. } if *session == id && *r == request).then_some(()));

    let status = serde_json::to_value(host.core.host_status()).unwrap();
    assert_eq!(status["sunshine"]["running"], false, "nothing started");
    assert_eq!(status["viewers"].as_array().map(Vec::len), Some(1), "still connected");
    viewer.core.disconnect(id);
}
