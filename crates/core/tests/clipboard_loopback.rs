//! The shared clipboard end to end over real QUIC on loopback: a viewer core controls a host
//! core, each with a pasteboard of its own (never the user's clipboard), and what is copied on one
//! shows up on the other. Needs no permissions.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lankvm_core::control::Backend;
use lankvm_core::{AudioBackend, ClipboardBackend, Core, CoreOptions, Event};
use platform_mac::clipboard::Pasteboard;
use transport::identity::DeviceIdentity;

const WAIT: Duration = Duration::from_secs(5);
/// Long enough for a copy to have crossed if it were going to: a few polls.
const SETTLE: Duration = Duration::from_millis(900);
const TEXT: &str = "public.utf8-plain-text";

struct Peer {
    core: Arc<Core>,
    events: Receiver<Event>,
    /// The pasteboard the core shares, as the test sees it.
    board: Pasteboard,
}

impl Peer {
    fn start(dir: &Path, backend: Backend, board: &str) -> Self {
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
            rendezvous: Some(String::new()),
            force_relay: false,
            clipboard: ClipboardBackend::Named(board.to_string()),
            // Never the user's microphone.
            audio: AudioBackend::OFF,
        };
        let core = Core::start_with(Arc::new(move |e| drop(tx.lock().unwrap().send(e))), options).unwrap();
        Self { core, events, board: Pasteboard::named(board) }
    }

    fn wait<T>(&self, what: &str, mut pick: impl FnMut(Event) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| panic!("timed out waiting for {what}"));
            let event = self.events.recv_timeout(left).unwrap_or_else(|_| panic!("timed out waiting for {what}"));
            if let Event::Ended { error, .. } = &event {
                panic!("session ended: {error:?}");
            }
            if let Some(v) = pick(event) {
                return v;
            }
        }
    }

    fn connect(&self, host: &Peer) -> u64 {
        let id = self.core.connect(&format!("127.0.0.1:{}", host.core.this_mac().port), (1920, 1080), 60);
        self.wait("connected", |e| matches!(e, Event::Connected { session, .. } if session == id).then_some(()));
        id
    }

    fn control(&self, id: u64, on: bool) {
        let request = self.core.set_control(id, on, false);
        let active = self.wait("control answer", |e| match e {
            Event::Control { session, request: r, active, .. } if session == id && r == request => Some(active),
            _ => None,
        });
        assert_eq!(active, on);
    }

    fn copy(&self, text: &str) {
        self.board.write(&[(TEXT.to_string(), text.as_bytes().to_vec())]);
    }

    fn text(&self) -> Option<String> {
        self.board.read(&[TEXT]).into_iter().next().map(|(_, data)| String::from_utf8(data).unwrap())
    }

    /// Waits until the pasteboard has `text`.
    fn has(&self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while self.text().as_deref() != Some(text) {
            assert!(Instant::now() < deadline, "the clipboard has {:?}, not {text:?}", self.text());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Checks that the pasteboard keeps `text` for a while.
    fn keeps(&self, text: &str) {
        std::thread::sleep(SETTLE);
        assert_eq!(self.text().as_deref(), Some(text));
    }
}

struct Setup {
    root: PathBuf,
    host: Peer,
    viewer: Peer,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn device(root: &Path, name: &str) -> (PathBuf, String) {
    let dir = root.join(name);
    let id = DeviceIdentity::load_or_create(&dir).unwrap();
    (dir, id.fingerprint.iter().map(|b| format!("{b:02x}")).collect())
}

fn setup(name: &str) -> Setup {
    let tag = format!("lankvm-clipboard-{name}-{}", std::process::id());
    let root = std::env::temp_dir().join(&tag);
    let _ = std::fs::remove_dir_all(&root);
    let (host_dir, host_fp) = device(&root, "host");
    let (viewer_dir, viewer_fp) = device(&root, "viewer");
    std::fs::write(host_dir.join("trusted-viewers.txt"), format!("{viewer_fp} viewer\n")).unwrap();
    std::fs::write(viewer_dir.join("trusted-hosts.txt"), format!("{host_fp} host\n")).unwrap();
    let host = Peer::start(&host_dir, Backend::Record(root.join("injected.jsonl")), &format!("{tag}-host"));
    let viewer = Peer::start(&viewer_dir, Backend::Hid, &format!("{tag}-viewer"));
    Setup { root, host, viewer }
}

#[test]
fn copies_go_both_ways_only_while_controlling() {
    let s = setup("both");
    s.host.copy("the host's");
    std::thread::sleep(Duration::from_millis(20));
    s.viewer.copy("copied before connecting");
    let id = s.viewer.connect(&s.host);
    // Only viewing: nothing is shared.
    s.host.keeps("the host's");

    // Control: the viewer's copy is the later one, and goes over.
    s.viewer.control(id, true);
    s.host.has("copied before connecting");
    s.viewer.keeps("copied before connecting");

    s.host.copy("copied on the host");
    s.viewer.has("copied on the host");
    s.viewer.copy("copied on the viewer");
    s.host.has("copied on the viewer");
    // Nothing comes back changed: each side keeps what it has.
    s.viewer.keeps("copied on the viewer");

    // View only: no longer shared.
    s.viewer.control(id, false);
    s.viewer.copy("private again");
    s.host.keeps("copied on the viewer");
}

#[test]
fn as_control_starts_the_later_copy_wins() {
    let s = setup("later");
    s.viewer.copy("the viewer's, earlier");
    // While nothing shares it, a Mac looks at its clipboard once a second.
    std::thread::sleep(Duration::from_millis(1200));
    s.host.copy("the host's, later");
    let id = s.viewer.connect(&s.host);
    s.viewer.control(id, true);
    s.viewer.has("the host's, later");
    s.host.keeps("the host's, later");
}

#[test]
fn the_viewer_can_turn_sharing_off() {
    let s = setup("off");
    let id = s.viewer.connect(&s.host);
    s.viewer.control(id, true);
    s.viewer.copy("shared");
    s.host.has("shared");

    s.viewer.core.set_share_clipboard(false);
    std::thread::sleep(Duration::from_millis(200));
    s.viewer.copy("not for the host");
    // While nothing shares it, a Mac looks at its clipboard once a second.
    std::thread::sleep(Duration::from_millis(1200));
    s.host.copy("not for the viewer");
    s.host.keeps("not for the viewer");
    s.viewer.keeps("not for the host");

    // On again: the later copy (the host's) wins on both.
    s.viewer.core.set_share_clipboard(true);
    s.viewer.has("not for the viewer");
}

#[test]
fn rich_text_and_images_go_files_stay() {
    let s = setup("rich");
    let id = s.viewer.connect(&s.host);
    s.viewer.control(id, true);
    let png = b"\x89PNG\r\n\x1a\n not really, but the clipboard doesn't mind".to_vec();
    let items = [
        ("public.html", b"<b>bold</b>".to_vec()),
        ("public.utf8-plain-text", b"bold".to_vec()),
        ("public.png", png.clone()),
        ("public.file-url", b"file:///Users/someone/secret.txt".to_vec()),
        ("org.nspasteboard.ConcealedType", Vec::new()),
    ];
    s.viewer.board.write(&items.iter().map(|(k, d)| (k.to_string(), d.clone())).collect::<Vec<_>>());
    s.host.has("bold");
    let kinds = ["public.html", "public.utf8-plain-text", "public.png", "public.file-url", "org.nspasteboard.ConcealedType"];
    let mut got = s.host.board.read(&kinds);
    got.sort();
    let mut want: Vec<_> = items.iter().filter(|(k, _)| *k != "public.file-url").map(|(k, d)| (k.to_string(), d.clone())).collect();
    want.sort();
    assert_eq!(got, want, "everything but the file, the password manager's marker too");
}
