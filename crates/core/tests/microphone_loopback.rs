//! The shared microphone end to end over real QUIC on loopback: a viewer core sends a tone in
//! place of its microphone, and a host core records what it would play into LanKVM Microphone.
//! Never touches the user's microphone or audio devices; needs no permissions.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lankvm_core::control::Backend;
use lankvm_core::{AudioBackend, ClipboardBackend, Core, CoreOptions, Event, MicSink, MicSource, Recording};
use transport::identity::DeviceIdentity;

const WAIT: Duration = Duration::from_secs(5);
const RATE: f64 = 48_000.0;
const TONE_HZ: f32 = 1000.0;
// `protocol::MicrophoneReason` codes.
const NONE: u16 = 0;
const NOT_INSTALLED: u16 = 1;
const TURNED_OFF: u16 = 2;

struct Peer {
    core: Arc<Core>,
    events: Receiver<Event>,
}

/// What a `microphone` event said.
#[derive(Debug, PartialEq)]
struct Mic {
    active: bool,
    reason: u16,
    message: String,
}

impl Peer {
    fn start(dir: &Path, audio: AudioBackend) -> Self {
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
            clipboard: ClipboardBackend::Off,
            audio,
        };
        let core = Core::start_with(Arc::new(move |e| drop(tx.lock().unwrap().send(e))), options).unwrap();
        Self { core, events }
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

    /// The next `microphone` event of session `id` matching `want`.
    fn mic(&self, id: u64, what: &str, want: impl Fn(&Mic) -> bool) -> Mic {
        self.wait(what, |e| match e {
            Event::Microphone { session, active, reason, message } if session == id => {
                let mic = Mic { active, reason, message };
                want(&mic).then_some(mic)
            }
            _ => None,
        })
    }

    fn viewer_has_mic(&self) -> bool {
        self.core.host_status().viewers.iter().any(|v| v.microphone)
    }
}

struct Setup {
    root: PathBuf,
    host: Peer,
    viewer: Peer,
    recording: Recording,
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

/// A paired host and viewer; `installed`: the host has the LanKVM Microphone driver.
fn setup(name: &str, installed: bool) -> Setup {
    let root = std::env::temp_dir().join(format!("lankvm-microphone-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (host_dir, host_fp) = device(&root, "host");
    let (viewer_dir, viewer_fp) = device(&root, "viewer");
    std::fs::write(host_dir.join("trusted-viewers.txt"), format!("{viewer_fp} viewer\n")).unwrap();
    std::fs::write(viewer_dir.join("trusted-hosts.txt"), format!("{host_fp} host\n")).unwrap();
    let recording = Recording::default();
    let sink = if installed { MicSink::Record(recording.clone()) } else { MicSink::Off };
    let host = Peer::start(&host_dir, AudioBackend { source: MicSource::Off, sink });
    let viewer = Peer::start(&viewer_dir, AudioBackend { source: MicSource::Tone(TONE_HZ), sink: MicSink::Off });
    Setup { root, host, viewer, recording }
}

/// Waits until the host has played `seconds` of the tone, and checks it is the tone.
fn hears_the_tone(recording: &Recording, seconds: f64) {
    let want = (seconds * RATE) as usize;
    let deadline = Instant::now() + WAIT;
    let mut heard = Vec::new();
    loop {
        heard.extend(recording.take().into_iter().skip_while(|&s| s == 0.0));
        if heard.len() >= want {
            break;
        }
        assert!(Instant::now() < deadline, "heard {} samples of audio, wanted {want}", heard.len());
        std::thread::sleep(Duration::from_millis(20));
    }
    let heard = &heard[..want];
    let crossings = heard.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    let hz = crossings as f64 * RATE / heard.len() as f64;
    assert!((hz - f64::from(TONE_HZ)).abs() < 15.0, "played {hz} Hz, sent {TONE_HZ} Hz");
    let peak = heard.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!((0.45..=0.55).contains(&peak), "peak {peak}, sent 0.5");
    // Nothing lost on loopback: the tone has no gaps.
    let gaps = heard.windows(48).filter(|w| w.iter().all(|&s| s == 0.0)).count();
    assert_eq!(gaps, 0, "silent stretches in the tone");
}

/// Waits until the host plays only silence.
fn goes_silent(recording: &Recording) {
    let deadline = Instant::now() + WAIT;
    loop {
        recording.take();
        std::thread::sleep(Duration::from_millis(300));
        if recording.take().iter().all(|&s| s == 0.0) {
            return;
        }
        assert!(Instant::now() < deadline, "still playing");
    }
}

#[test]
fn the_host_plays_the_viewers_microphone_while_asked() {
    let s = setup("plays", true);
    let id = s.viewer.connect(&s.host);
    // As the session starts: it could share its microphone, and doesn't.
    let start = s.viewer.mic(id, "the host's word", |m| !m.active);
    assert_eq!(start.reason, NONE, "{start:?}");
    assert!(s.recording.take().iter().all(|&x| x == 0.0), "nothing plays before it's asked");

    s.viewer.core.set_microphone(id, true);
    s.viewer.mic(id, "microphone on", |m| m.active);
    hears_the_tone(&s.recording, 0.5);
    assert!(s.host.viewer_has_mic(), "This Mac lists the viewer's microphone");

    s.viewer.core.set_microphone(id, false);
    let off = s.viewer.mic(id, "microphone off", |m| !m.active);
    assert_eq!(off.reason, NONE);
    goes_silent(&s.recording);
    assert!(!s.host.viewer_has_mic());

    // And on again.
    s.viewer.core.set_microphone(id, true);
    s.viewer.mic(id, "microphone on again", |m| m.active);
    hears_the_tone(&s.recording, 0.3);
}

#[test]
fn turning_control_off_on_the_host_stops_it() {
    let s = setup("revoked", true);
    let id = s.viewer.connect(&s.host);
    s.viewer.core.set_microphone(id, true);
    s.viewer.mic(id, "microphone on", |m| m.active);
    hears_the_tone(&s.recording, 0.2);

    s.host.core.set_allow_control(false);
    let stopped = s.viewer.mic(id, "stopped by the host", |m| !m.active);
    assert_eq!(stopped.reason, TURNED_OFF, "{stopped:?}");
    assert!(stopped.message.contains("only view"), "{stopped:?}");
    goes_silent(&s.recording);

    // Asking while it's off is refused, and leaves the microphone off.
    s.viewer.core.set_microphone(id, true);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s.host.viewer_has_mic());
    // Allowed again: the viewer hears it can, and asks again itself.
    s.host.core.set_allow_control(true);
    let again = s.viewer.mic(id, "available again", |m| !m.active && m.reason == NONE);
    assert!(again.message.is_empty());
    s.viewer.core.set_microphone(id, true);
    s.viewer.mic(id, "microphone on again", |m| m.active);
    hears_the_tone(&s.recording, 0.2);
}

#[test]
fn a_host_without_the_driver_says_so() {
    let s = setup("missing", false);
    let id = s.viewer.connect(&s.host);
    let said = s.viewer.mic(id, "the host's word", |m| m.reason == NOT_INSTALLED);
    assert!(!said.active && said.message.contains("This Mac → Microphone"), "{said:?}");
    s.viewer.core.set_microphone(id, true);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s.host.viewer_has_mic());
}

#[test]
fn closing_the_session_stops_it() {
    let s = setup("closed", true);
    let id = s.viewer.connect(&s.host);
    s.viewer.core.set_microphone(id, true);
    s.viewer.mic(id, "microphone on", |m| m.active);
    hears_the_tone(&s.recording, 0.2);
    s.viewer.core.disconnect(id);
    goes_silent(&s.recording);
    let deadline = Instant::now() + WAIT;
    while !s.host.core.host_status().viewers.is_empty() {
        assert!(Instant::now() < deadline, "the viewer is still listed");
        std::thread::sleep(Duration::from_millis(20));
    }
}
