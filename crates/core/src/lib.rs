//! LanKVM core: networking, capture/encode, decode/render, pairing. Everything except the UI.
//!
//! The SwiftUI app drives it through the C ABI in [`ffi`] and receives [`Event`]s as JSON.

mod address_book;
mod client;
mod clipboard;
pub mod control;
mod dht;
mod displays;
pub mod ffi;
mod host;
mod internet;
mod microphone;
pub mod rate;
mod render;
mod rendezvous;
mod stats;
mod view;

use std::collections::HashMap;
use std::ffi::c_void;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use platform_mac::{permissions, system};
use protocol::{CursorState, DEFAULT_PORT, DisplayChoice, InputMsg};
use serde::Serialize;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;
use transport::endpoint::Network;
use transport::identity::{DeviceIdentity, Fingerprint, short_hex};
use transport::pairing::TrustStore;
use transport::rendezvous::{DEFAULT_SERVER, RendezvousIdentity};

pub use crate::address_book::Via;
pub use crate::client::{FrameProbe, ProbeFrame};
pub use crate::clipboard::ClipboardBackend;
pub use crate::microphone::{AudioBackend, MicSink, MicSource, Recording};
pub use crate::stats::FrameTiming;
use crate::address_book::AddressBook;
use crate::client::{Session, SessionEvent, SessionInfo};
use crate::clipboard::Clipboard;
use crate::dht::DhtRendezvous;
use crate::internet::{HostInternet, InternetHosts};
use crate::microphone::{Microphone, Speaker};
use crate::rendezvous::Rendezvous;

/// Notifications for the UI. Delivered on arbitrary threads.
#[derive(Serialize, Debug)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    /// Viewers or pairing requests of this Mac changed: re-read the host status.
    HostChanged,
    /// The list of paired devices changed.
    TrustChanged,
    /// First connection to a host: ask the user for the PIN it shows.
    PinNeeded { session: u64 },
    Connected { session: u64, info: SessionInfo },
    Ended { session: u64, error: Option<String> },
    /// Whether this session controls its host now (answering `request`, or 0 when the host acted
    /// on its own); `reason` (a `ControlReason` code) and `message` say why not.
    #[serde(rename_all = "camelCase")]
    Control { session: u64, request: u32, active: bool, reason: u16, message: String, injected_tag: i64, host_pid: u32 },
    /// A host cursor image (base64 PNG at 2x; sizes and hot spot in points, top-left origin).
    #[serde(rename_all = "camelCase")]
    CursorShape {
        session: u64,
        id: u32,
        #[serde(serialize_with = "base64")]
        png: Vec<u8>,
        width: f32,
        height: f32,
        hot_x: f32,
        hot_y: f32,
    },
    /// How to show the host's cursor: `shape` with `id`, `hidden`, or `inVideo`.
    Cursor { session: u64, state: &'static str, id: Option<u32> },
    /// What the session shows now (`info`), answering `request` (0: the host changed it on its
    /// own). `reason` (a `DisplayReason` code) and `message` say why it isn't what was asked for, or
    /// what happened. Sent once the new picture is on screen.
    Display { session: u64, request: u32, info: SessionInfo, reason: u16, message: String },
    /// The session's video of `width` × `height` pixels can't be shown (e.g. this Mac can't
    /// decode that size).
    StreamError { session: u64, message: String, width: u32, height: u32 },
    /// A clipboard of `bytes` was too big to share with the session's host, so the other Mac's
    /// clipboard was emptied: this Mac's (`sent`), or the host's (this Mac's was emptied).
    ClipboardTooLarge { session: u64, bytes: u64, sent: bool },
    /// Whether the session's host plays this Mac's microphone now. When it doesn't, `reason` (a
    /// `MicrophoneReason` code) and `message` say why it doesn't, or wouldn't if asked (0: it
    /// could). Sent as the session starts, on every answer to `lk_set_microphone`, and when it
    /// changes; a refusal also turns the session's microphone off.
    Microphone { session: u64, active: bool, reason: u16, message: String },
}

/// Serializes bytes as standard base64 (what Swift's `JSONDecoder` expects for `Data`).
fn base64<S: serde::Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() { ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s.serialize_str(&out)
}

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// This device's identity and the devices it has paired with.
pub struct Trust {
    pub fingerprint: Fingerprint,
    /// Devices allowed to view and control this Mac.
    pub viewers: Mutex<TrustStore>,
    /// Macs this one has paired with as a viewer.
    pub hosts: Mutex<TrustStore>,
    /// How to reach those Macs over the internet: the keys they gave this one.
    pub(crate) internet_hosts: Mutex<InternetHosts>,
    /// The names the user calls those Macs by, where they are on the local network, and how to
    /// connect to them.
    pub(crate) address_book: Mutex<AddressBook>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ThisMac {
    pub name: String,
    pub addresses: Vec<String>,
    pub port: u16,
    pub device_id: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RecentHost {
    pub address: String,
    pub name: String,
    /// The Mac it reached (hex), when known: Connect lists paired Macs apart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

pub struct Core {
    rt: tokio::runtime::Runtime,
    network: Network,
    trust: Arc<Trust>,
    host: Arc<host::HostCtx>,
    /// Introductions through LanKVM servers, as host and as viewer.
    rendezvous: Arc<Rendezvous>,
    /// Introductions through the BitTorrent DHT, as host and as viewer: no server at all.
    dht: Arc<DhtRendezvous>,
    events: EventSink,
    this_mac: ThisMac,
    recents: Mutex<Recents>,
    sessions: Mutex<HashMap<u64, Arc<Session>>>,
    next_session: AtomicU64,
    /// This Mac's clipboard, shared with the Macs it controls and the one controlling it (None if
    /// the core shares none).
    clipboard: Option<Arc<Clipboard>>,
    /// Whether this Mac shares its clipboard with the Macs it controls (the user's choice).
    share_clipboard: tokio::sync::watch::Sender<bool>,
    /// This Mac's microphone, for the hosts its sessions share it with (None if the core shares
    /// none).
    microphone: Option<Arc<Microphone>>,
    _activity: Activity,
}

/// Keeps App Nap from throttling the process: a host usually runs in the background, and a
/// throttled timer or thread is added latency for whoever controls it.
struct Activity(#[allow(dead_code)] system::ActivityToken);
// SAFETY: the token is an opaque object that is only kept alive, never used.
unsafe impl Send for Activity {}
unsafe impl Sync for Activity {}

/// How a core runs. [`CoreOptions::from_env`] is what the app uses; tests run several cores in
/// one process with their own directories and ports.
#[derive(Clone, Debug)]
pub struct CoreOptions {
    /// Identity, trust lists and settings.
    pub data_dir: PathBuf,
    /// UDP port to listen on (0: any free port).
    pub port: u16,
    /// Stream the screen to viewers. Without it a host still accepts viewers and remote control,
    /// for tests that run without Screen Recording permission.
    pub video: bool,
    /// Where injected input goes.
    pub backend: control::Backend,
    /// Let a viewer on this same Mac control it (only for testing: the pointer and keyboard are
    /// shared, so it would otherwise loop).
    pub allow_same_mac_control: bool,
    /// Tests: inject only into this process's windows (`LANKVM_INJECT_GUARD_PID`).
    pub guard_pid: Option<i32>,
    /// Tests: end control this long after granting it (`LANKVM_TEST_CONTROL_TTL`, seconds).
    pub control_ttl: Option<std::time::Duration>,
    /// Tests: treat connections over loopback as internet ones, so internet access can be tried
    /// on one Mac (`LANKVM_TEST_LOOPBACK_IS_INTERNET=1`). The router is then left alone.
    pub loopback_is_internet: bool,
    /// The LanKVM server this Mac registers with as a host, instead of the setting
    /// (`LANKVM_RENDEZVOUS`); "" turns it off. None: the setting. Tests set it, always: none may
    /// reach the real server.
    pub rendezvous: Option<String>,
    /// Tests: connecting to a paired Mac by fingerprint goes straight to its LanKVM server's
    /// relay, without trying its addresses or punching (`LANKVM_TEST_FORCE_RELAY=1`).
    pub force_relay: bool,
    /// The nodes this Mac joins the BitTorrent DHT through, instead of the public ones
    /// (`LANKVM_DHT_BOOTSTRAP`, comma-separated); empty turns the DHT off for the run. None: the
    /// public ones. Tests set it, always: none may reach the real DHT.
    pub dht_bootstrap: Option<Vec<String>>,
    /// The clipboard shared with the Macs this one controls or is controlled by
    /// (`LANKVM_CLIPBOARD`). Tests use pasteboards of their own, never the user's.
    pub clipboard: ClipboardBackend,
    /// The microphone this Mac shares with the Macs it views, and where theirs play
    /// (`LANKVM_MICROPHONE`). Tests use a tone and a recording, never the user's microphone.
    pub audio: AudioBackend,
}

impl CoreOptions {
    /// `LANKVM_DATA_DIR`, `LANKVM_PORT` and `LANKVM_INJECT`, or the defaults.
    pub fn from_env() -> Self {
        Self {
            data_dir: data_dir(),
            port: std::env::var("LANKVM_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT),
            video: true,
            backend: control::Backend::from_env(),
            allow_same_mac_control: std::env::var("LANKVM_ALLOW_SAME_MAC_CONTROL").is_ok_and(|v| v == "1"),
            guard_pid: std::env::var("LANKVM_INJECT_GUARD_PID").ok().and_then(|v| v.parse().ok()),
            control_ttl: std::env::var("LANKVM_TEST_CONTROL_TTL")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .and_then(|s| std::time::Duration::try_from_secs_f64(s).ok())
                .filter(|d| !d.is_zero()),
            loopback_is_internet: std::env::var("LANKVM_TEST_LOOPBACK_IS_INTERNET").is_ok_and(|v| v == "1"),
            // An instance testing internet access on one Mac doesn't register with the real
            // LanKVM server unless told to.
            rendezvous: std::env::var("LANKVM_RENDEZVOUS").ok().or_else(|| testing_internet().then(String::new)),
            force_relay: std::env::var("LANKVM_TEST_FORCE_RELAY").is_ok_and(|v| v == "1"),
            // Nor does it join the real BitTorrent DHT.
            dht_bootstrap: std::env::var("LANKVM_DHT_BOOTSTRAP")
                .ok()
                .map(|list| list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
                .or_else(|| testing_internet().then(Vec::new)),
            clipboard: ClipboardBackend::from_env(),
            audio: AudioBackend::from_env(),
        }
    }
}

impl Core {
    pub fn start(events: EventSink) -> Result<Arc<Self>> {
        Self::start_with(events, CoreOptions::from_env())
    }

    pub fn start_with(events: EventSink, options: CoreOptions) -> Result<Arc<Self>> {
        init_logging();
        let activity = Activity(system::begin_latency_critical_activity());
        let CoreOptions {
            data_dir: dir,
            port,
            video,
            backend,
            allow_same_mac_control,
            guard_pid,
            control_ttl,
            loopback_is_internet,
            rendezvous: rendezvous_override,
            force_relay,
            dht_bootstrap,
            clipboard,
            audio,
        } = options;
        // Video decode runs synchronously on these (3-6 ms a frame); enough workers keep the
        // connection drivers and input writer from waiting behind it.
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build()?;
        let identity = DeviceIdentity::load_or_create(&dir)?;
        let network = {
            let _guard = rt.enter();
            bind_network(port, &identity).with_context(|| format!("Can't listen on UDP port {port}. Is LanKVM already running?"))?
        };
        if loopback_is_internet {
            tracing::warn!("loopback counts as the internet (LANKVM_TEST_LOOPBACK_IS_INTERNET)");
            network.gate.set_loopback_is_internet(true);
        }
        let port = network.endpoint.local_addr()?.port();
        let device_id = short_hex(&identity.fingerprint);
        tracing::info!(addr = %network.endpoint.local_addr()?, %device_id, "listening");
        tracing::info!(
            screen_capture = permissions::screen_capture_allowed(),
            input_control = permissions::input_control_allowed(),
            "permissions"
        );

        let trust = Arc::new(Trust {
            fingerprint: identity.fingerprint,
            viewers: Mutex::new(TrustStore::load(&dir.join("trusted-viewers.txt"))),
            hosts: Mutex::new(TrustStore::load(&dir.join("trusted-hosts.txt"))),
            internet_hosts: Mutex::new(InternetHosts::load(&dir.join("internet-hosts.json"))),
            address_book: Mutex::new(AddressBook::load(&dir.join("address-book.json"))),
        });
        let recents = Recents::load(&dir.join("recent-hosts.txt"));
        adopt_local_addresses(&recents, &trust);
        // The first keyboard event a process creates must be made on the main thread (it loads
        // the keyboard layout); lk_start runs there. Elsewhere (tests) it would do no good.
        if platform_mac::system::is_main_thread() {
            platform_mac::inject::warm_up();
        }
        let settings_path = dir.join("host-settings.json");
        let settings = control::HostSettings::load(&settings_path);
        if let Some(server) = &rendezvous_override {
            tracing::warn!(server, "the LanKVM server is set for this run, not by the setting (LANKVM_RENDEZVOUS; \"\": none)");
        }
        if force_relay {
            tracing::warn!("connections to paired Macs go through the LanKVM server's relay (LANKVM_TEST_FORCE_RELAY)");
        }
        let rendezvous_server = rendezvous_override.clone().unwrap_or_else(|| settings.rendezvous_server.clone());
        let rendezvous = Rendezvous::start(
            network.clone(),
            load_or_create_rendezvous_identity(&dir)?,
            rt.handle().clone(),
            events.clone(),
            force_relay,
            loopback_is_internet || testing_internet(),
            rendezvous_override.as_deref().map(str::trim) == Some(DEFAULT_SERVER),
        );
        // The public DHT unless told otherwise; never while loopback stands in for the internet
        // (a test), unless a test names its own nodes.
        let dht_bootstrap = match dht_bootstrap {
            Some(nodes) if nodes.is_empty() => None,
            Some(nodes) => Some(nodes),
            None if loopback_is_internet || testing_internet() => None,
            None => Some(transport::dht::DEFAULT_BOOTSTRAP.iter().map(|s| s.to_string()).collect()),
        };
        let dht = DhtRendezvous::start(network.clone(), rt.handle().clone(), events.clone(), &dir, dht_bootstrap, settings.dht);
        let internet = HostInternet::new(
            internet::load_or_create_secret(&dir)?,
            network.gate.clone(),
            trust.clone(),
            port,
            events.clone(),
            settings.internet_access,
            settings.public_address.clone(),
            rendezvous.clone(),
            dht.clone(),
        );
        if backend != control::Backend::Hid {
            tracing::warn!(?backend, "injected input is redirected (LANKVM_INJECT)");
        }
        if clipboard != ClipboardBackend::System {
            tracing::warn!(?clipboard, "the shared clipboard isn't this Mac's (LANKVM_CLIPBOARD)");
        }
        let clipboard = Clipboard::start(&clipboard);
        if !audio.is_system() {
            tracing::warn!(?audio, "the shared microphones aren't this Mac's (LANKVM_MICROPHONE)");
        }
        let host = Arc::new_cyclic(|weak| host::HostCtx {
            displays: displays::Displays::new(weak.clone(), identity.fingerprint, Box::new(displays::RealScreens::default())),
            console_active: std::sync::atomic::AtomicBool::new(true),
            status: Mutex::new(host::HostStatus::default()),
            events: events.clone(),
            trust: trust.clone(),
            settings: Mutex::new(settings),
            settings_path,
            internet: internet.clone(),
            backend,
            injected_tag: platform_mac::inject::new_injected_tag(),
            rt: rt.handle().clone(),
            video,
            inputs: Mutex::new(Vec::new()),
            allow_same_mac_control,
            controller: Mutex::new(None),
            guard_pid,
            control_ttl,
            pairing_throttle: Default::default(),
            pending: Default::default(),
            handshakes: Default::default(),
            clipboard: clipboard.clone(),
            speaker: Speaker::new(audio.sink),
        });
        if guard_pid.is_some() || control_ttl.is_some() {
            tracing::warn!(?guard_pid, ?control_ttl, "test limits on remote control");
        }
        if video {
            // Lid, cables, Displays settings: the virtual displays' arrangement may need repair, and
            // sessions may need to follow. (Tests without video never touch displays.)
            let weak = Arc::downgrade(&host);
            platform_mac::virtual_display::on_reconfiguration(move || {
                if let Some(host) = weak.upgrade() {
                    host.displays.reconfigured();
                }
            });
        }
        // Before the first connection: the gate starts with internet access off.
        {
            let internet = Arc::downgrade(&internet);
            dht.set_addresses(move || internet.upgrade().map(|i| i.dht_addresses()).unwrap_or_default());
        }
        internet.sync_keys();
        internet.apply_mapping();
        rendezvous.set_host_server(&rendezvous_server);
        rendezvous.set_host_enabled(internet.enabled());
        rt.spawn(host::run(network.clone(), host.clone()));

        let this_mac = ThisMac { name: system::device_name(), addresses: local_addresses(), port, device_id };
        Ok(Arc::new(Self {
            rt,
            network,
            trust,
            host,
            rendezvous,
            dht,
            events,
            this_mac,
            recents: Mutex::new(recents),
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
            clipboard,
            share_clipboard: tokio::sync::watch::channel(true).0,
            microphone: Microphone::new(audio.source),
            _activity: activity,
        }))
    }

    /// Before the app quits: lets go of everything held on this Mac for a remote viewer, and of
    /// everything this Mac holds on remote Macs, so no key stays down anywhere. Blocks briefly
    /// (deleting the port mapping on the router gets a second at most).
    pub fn shutdown(&self) {
        // First, so sessions ending below don't start removing displays: quitting removes them.
        self.host.displays.shutdown();
        self.host.release_all_input(std::time::Duration::from_millis(300));
        let sessions: Vec<_> = self.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
        for s in &sessions {
            s.close();
        }
        // While the socket is still open: the LanKVM server forgets this Mac now rather than in
        // a minute or so.
        let rendezvous = self.rendezvous.clone();
        self.rt.block_on(async move { rendezvous.shutdown().await });
        // And remember the DHT nodes that answered, for the next start.
        self.dht.shutdown();
        // Tell every peer (viewers of this Mac included) the connection is over, rather than
        // leaving them to time out, and give that a moment to go out.
        self.network.endpoint.close(0u32.into(), b"LanKVM quit");
        let endpoint = self.network.endpoint.clone();
        let _ = self.rt.block_on(async { tokio::time::timeout(std::time::Duration::from_millis(300), endpoint.wait_idle()).await });
        // Then delete the port mapping on the router (mDNSResponder does that without the
        // socket). A daemon that hangs gets a second, then deletes it itself once LanKVM quits.
        self.host.internet.stop_mapping(std::time::Duration::from_secs(1));
    }

    pub fn this_mac(&self) -> &ThisMac {
        &self.this_mac
    }

    pub fn host_status(&self) -> host::HostStatusView {
        self.host.view()
    }

    pub fn paired_devices(&self) -> PairedDevices {
        let device = |fp: &Fingerprint, name: &String| PairedDevice {
            fingerprint: hex(fp),
            device_id: short_hex(fp),
            name: name.clone(),
            alias: None,
            local_address: None,
            internet_address: None,
            reachable: false,
            connection: None,
        };
        let viewers = self.trust.viewers.lock().unwrap().entries().iter().map(|(fp, name)| device(fp, name)).collect();
        let hosts = self.trust.hosts.lock().unwrap().entries().to_vec();
        let internet = self.trust.internet_hosts.lock().unwrap();
        let book = self.trust.address_book.lock().unwrap();
        let hosts = hosts
            .iter()
            .map(|(fp, name)| {
                // Where it was reached on the local network, or else where it says it is there
                // (on this Mac's network first).
                let announced = internet.way_to(fp).map(|way| way.lan).unwrap_or_default();
                let nearby = || announced.iter().find(|a| a.parse::<SocketAddr>().is_ok_and(|a| on_this_network(a.ip())));
                PairedDevice {
                    alias: book.alias(fp),
                    local_address: book.local(fp).or_else(|| nearby().or(announced.first()).cloned()),
                    internet_address: internet.address(fp),
                    reachable: internet.reachable(fp, self.dht.available()),
                    connection: Some(book.via(fp)),
                    ..device(fp, name)
                }
            })
            .collect();
        PairedDevices { viewers, hosts }
    }

    /// Newest first. Each says which Mac it reached, when that is known.
    pub fn recent_hosts(&self) -> Vec<RecentHost> {
        let hosts = self.trust.hosts.lock().unwrap();
        let mut entries = self.recents.lock().unwrap().entries.clone();
        for recent in &mut entries {
            if recent.fingerprint.is_none() {
                recent.fingerprint = recent_host(recent, &hosts).map(|fp| hex(&fp));
            }
        }
        entries
    }

    /// Calls paired host `fingerprint_hex` `alias` everywhere on this Mac ("" for the name it gave
    /// itself).
    pub fn set_host_alias(&self, fingerprint_hex: &str, alias: &str) {
        let Some(fp) = self.paired_host(fingerprint_hex) else { return };
        if self.trust.address_book.lock().unwrap().set_alias(&fp, alias) {
            (self.events)(Event::TrustChanged);
        }
    }

    /// How to connect to paired host `fingerprint_hex` by name (`lankvm:<fingerprint>`): see
    /// [`Via`]. Applies from the next connection.
    pub fn set_host_connection(&self, fingerprint_hex: &str, via: Via) {
        let Some(fp) = self.paired_host(fingerprint_hex) else { return };
        if self.trust.address_book.lock().unwrap().set_via(&fp, via) {
            tracing::info!(host = %short_hex(&fp), ?via, "connection type");
            (self.events)(Event::TrustChanged);
        }
    }

    /// The paired host `fingerprint_hex` names, if this Mac is paired with it.
    fn paired_host(&self, fingerprint_hex: &str) -> Option<Fingerprint> {
        from_hex(fingerprint_hex.trim()).filter(|fp| self.trust.hosts.lock().unwrap().contains(fp))
    }

    /// Opens a viewer session; progress arrives as events tagged with the returned id. `target` is
    /// an address (IP, `ip:port` or a name), or `lankvm:<fingerprint>` for a paired host reached
    /// over the internet: at its addresses and through its LanKVM server at once. `max_size` and
    /// `max_fps` describe this Mac's screen (pixels, refresh rate).
    pub fn connect(self: &Arc<Self>, target: &str, max_size: (u32, u32), max_fps: u32) -> u64 {
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let target = target.trim().to_string();
        let weak = Arc::downgrade(self);
        let typed = target.clone();
        let on_event = Arc::new(move |event: SessionEvent| {
            let Some(core) = weak.upgrade() else { return };
            let event = match event {
                SessionEvent::PinNeeded => Event::PinNeeded { session: id },
                SessionEvent::Connected(info) => {
                    core.recents.lock().unwrap().add(&typed, &info.host_name, &info.host_fingerprint);
                    Event::Connected { session: id, info }
                }
                SessionEvent::Ended { error } => {
                    // Drop the session off this thread: it may be the session's own task.
                    let ended = core.sessions.lock().unwrap().remove(&id);
                    std::thread::spawn(move || drop(ended));
                    Event::Ended { session: id, error }
                }
                SessionEvent::Control(state) => Event::Control {
                    session: id,
                    request: state.request,
                    active: state.active,
                    reason: state.reason.0,
                    message: state.message,
                    injected_tag: state.injected_tag,
                    host_pid: state.host_pid,
                },
                SessionEvent::CursorShape { id: shape, png, width, height, hot_x, hot_y } => {
                    Event::CursorShape { session: id, id: shape, png, width, height, hot_x, hot_y }
                }
                SessionEvent::Cursor(state) => {
                    let (state, shape) = match state {
                        CursorState::Shape(shape) => ("shape", Some(shape)),
                        CursorState::Hidden => ("hidden", None),
                        CursorState::InVideo => ("inVideo", None),
                    };
                    Event::Cursor { session: id, state, id: shape }
                }
                SessionEvent::Display { request, info, reason, message } => Event::Display { session: id, request, info, reason, message },
                SessionEvent::StreamError { message, width, height } => Event::StreamError { session: id, message, width, height },
                SessionEvent::TrustChanged => Event::TrustChanged,
                SessionEvent::ClipboardTooLarge { bytes, sent } => Event::ClipboardTooLarge { session: id, bytes, sent },
                SessionEvent::Microphone(state) => {
                    Event::Microphone { session: id, active: state.active, reason: state.reason.0, message: state.message }
                }
            };
            (core.events)(event);
        });
        let session = Session::start(
            self.rt.handle(),
            self.network.clone(),
            self.trust.clone(),
            self.rendezvous.clone(),
            self.dht.clone(),
            target,
            max_size,
            max_fps,
            self.clipboard.clone().map(|clipboard| (clipboard, self.share_clipboard.subscribe())),
            self.microphone.clone(),
            on_event,
        );
        self.sessions.lock().unwrap().insert(id, Arc::new(session));
        id
    }

    fn session(&self, id: u64) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }

    pub fn submit_pin(&self, id: u64, pin: &str) {
        if let Some(s) = self.session(id) {
            s.submit_pin(pin.to_string());
        }
    }

    /// Asks the session's host for control (true) or to only view it (false). Returns the
    /// request id the answer will carry (0 if there's no such session).
    pub fn set_control(&self, id: u64, on: bool, take_over: bool) -> u32 {
        self.session(id).map_or(0, |s| s.set_control(on, take_over))
    }

    /// Asks the session's host to show `display`. Returns the request id the answering
    /// [`Event::Display`] carries (0 if there's no such session).
    pub fn set_display(&self, id: u64, display: DisplayChoice) -> u32 {
        self.session(id).map_or(0, |s| s.set_display(display))
    }

    /// Whether this Mac shares its clipboard with the Macs it controls: what is copied on either
    /// can be pasted on the other. Applies to every session at once (on by default).
    pub fn set_share_clipboard(&self, on: bool) {
        if self.share_clipboard.send_replace(on) != on {
            tracing::info!(on, "share the clipboard with the Macs this one controls");
        }
    }

    /// Whether to share this Mac's microphone with the session's host, whose apps then hear it as
    /// "LanKVM Microphone". [`Event::Microphone`] says whether the host plays it. Off on connecting.
    pub fn set_microphone(&self, id: u64, on: bool) {
        if let Some(s) = self.session(id) {
            s.set_microphone(on);
        }
    }

    /// The LanKVM Microphone driver was installed or removed here (the app does that): viewers
    /// learn whether they can share their microphones now.
    pub fn microphone_driver_changed(&self) {
        self.host.microphone_changed();
    }

    /// While controlling: whether the session's window has the focus.
    pub fn set_focus(&self, id: u64, forwarding: bool) {
        if let Some(s) = self.session(id) {
            s.set_focus(forwarding);
        }
    }

    pub fn send_input(&self, id: u64, msg: InputMsg) {
        if let Some(s) = self.session(id) {
            s.send_input(msg);
        }
    }

    /// Test hook: calls `probe` with every decoded tile of the session.
    pub fn set_frame_probe(&self, id: u64, probe: Option<FrameProbe>) {
        if let Some(s) = self.session(id) {
            s.set_frame_probe(probe);
        }
    }

    pub fn disconnect(&self, id: u64) {
        let session = self.sessions.lock().unwrap().remove(&id);
        if let Some(session) = session {
            session.close();
        }
    }

    /// # Safety
    /// `layer` must be a valid `CAMetalLayer`.
    pub unsafe fn attach_view(&self, id: u64, layer: *mut c_void, width: u32, height: u32) {
        if let Some(s) = self.session(id)
            && let Err(e) = unsafe { s.attach_view(layer, width, height) }
        {
            tracing::warn!("attach view: {e:#}");
        }
    }

    pub fn resize_view(&self, id: u64, width: u32, height: u32) {
        if let Some(s) = self.session(id) {
            s.resize_view(width, height);
        }
    }

    pub fn detach_view(&self, id: u64) {
        if let Some(s) = self.session(id) {
            s.detach_view();
        }
    }

    pub fn session_stats(&self, id: u64) -> Option<stats::StatsView> {
        self.session(id).map(|s| s.stats())
    }

    pub fn kick_viewer(&self, id: u64) {
        self.host.close_viewer(id);
    }

    /// Takes control back from a viewer, which keeps viewing.
    pub fn stop_control(&self, id: u64) {
        self.host.stop_control(id);
    }

    /// Takes this Mac back from remote use: control from whoever has it, and the virtual displays
    /// made for viewers (menu, stop hotkey).
    pub fn stop_all_control(&self) {
        self.host.stop_all_control();
        self.host.remove_virtual_display(None);
    }

    /// Removes a virtual display made for a viewer (`None`: all of them). Never waits.
    pub fn remove_virtual_display(&self, display_id: Option<u32>) {
        self.host.remove_virtual_display(display_id);
    }

    /// Whether this user's session has the screen (fast user switching). Never waits.
    pub fn set_console_active(&self, active: bool) {
        self.host.set_console_active(active);
    }

    /// Whether paired Macs may control this one.
    pub fn set_allow_control(&self, allow: bool) {
        self.host.set_allow_control(allow);
    }

    /// Whether paired Macs may connect over the internet. On asks the router to forward the
    /// port; off stops that and ends the sessions that came over the internet. Returns at once.
    pub fn set_internet_access(&self, on: bool) {
        self.host.set_internet_access(on);
    }

    /// The address paired Macs are told to use over the internet (a dynamic DNS name or an IP,
    /// with or without a port); "" for none.
    pub fn set_public_address(&self, address: &str) {
        self.host.set_public_address(address);
    }

    /// The LanKVM server ("host:port") this Mac registers with while internet access is on, so
    /// paired Macs reach it with no router setup; "" for none. Saved; takes effect at once.
    /// Whether this Mac uses the BitTorrent DHT to meet paired Macs with no server, as host and
    /// as viewer.
    pub fn set_dht(&self, on: bool) {
        self.host.set_dht(on);
        // Whether paired Macs count as reachable over the internet depends on it.
        (self.events)(Event::TrustChanged);
    }

    /// Tests: as a viewer, connects through the DHT only once the host punched or answered,
    /// never straight to the addresses in its note (on loopback those always work).
    #[doc(hidden)]
    pub fn set_dht_wait_for_answer(&self, on: bool) {
        self.dht.set_wait_for_answer(on);
    }

    /// Tests: as a viewer, reaches paired hosts through the BitTorrent DHT and no other way (not
    /// on the local network, at addresses they announced, or through a LanKVM server).
    #[doc(hidden)]
    pub fn set_dht_only(&self, on: bool) {
        self.dht.set_only(on);
    }

    pub fn set_rendezvous_server(&self, address: &str) {
        self.host.set_rendezvous_server(address);
    }

    /// Tests: treat connections over loopback as internet ones (see
    /// [`CoreOptions::loopback_is_internet`]).
    #[doc(hidden)]
    pub fn set_loopback_is_internet(&self, on: bool) {
        self.network.gate.set_loopback_is_internet(on);
        self.host.internet.update_mapping();
    }

    /// Fresh check of the Accessibility permission (the host status includes it too).
    pub fn control_permission(&self) -> bool {
        self.host.backend.permitted()
    }

    pub fn deny_pairing(&self, id: u64) {
        self.host.deny_pairing(id);
    }

    /// Forgets a paired device. `kind` is "viewer" (may control this Mac) or "host".
    pub fn forget_device(&self, kind: &str, fingerprint_hex: &str) {
        let Some(fp) = from_hex(fingerprint_hex) else { return };
        let store = if kind == "host" { &self.trust.hosts } else { &self.trust.viewers };
        if let Err(e) = store.lock().unwrap().remove(&fp) {
            tracing::warn!("forget device: {e:#}");
        }
        if kind == "host" {
            self.trust.internet_hosts.lock().unwrap().remove(&fp);
            self.trust.address_book.lock().unwrap().remove(&fp);
        } else {
            // Its knocks go unanswered from now on.
            self.host.internet.sync_keys();
            self.host.close_viewers_with(&fp);
            self.host.displays.forget(fp, format!("{} forgot this Mac.", system::device_name()));
        }
        (self.events)(Event::TrustChanged);
    }

    /// Fast check (may be stale until the app restarts).
    pub fn screen_capture_allowed(&self) -> bool {
        permissions::screen_capture_allowed()
    }

    /// Slower, authoritative check: actually asks ScreenCaptureKit for the displays.
    pub fn verify_screen_capture(&self) -> bool {
        host::can_capture()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    pub fingerprint: String,
    pub device_id: String,
    /// The name it gave itself.
    pub name: String,
    /// A host: the name the user calls it by instead, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// A host's address on the local network: where this Mac last reached it there, or else where
    /// it said it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_address: Option<String>,
    /// A host's address over the internet: where this Mac last reached it, or else where it
    /// said to reach it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internet_address: Option<String>,
    /// A host this Mac knows a way to try over the internet (its LanKVM server, or an address).
    pub reachable: bool,
    /// A host: how `lankvm:<fingerprint>` connects to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<Via>,
}

#[derive(Serialize)]
pub struct PairedDevices {
    pub viewers: Vec<PairedDevice>,
    pub hosts: Vec<PairedDevice>,
}

/// Recently connected hosts, newest first, as `<address>\t<name>\t<fingerprint>` lines (lines from
/// before LanKVM kept the fingerprint have none).
struct Recents {
    path: PathBuf,
    entries: Vec<RecentHost>,
}

impl Recents {
    const MAX: usize = 12;

    fn load(path: &Path) -> Self {
        let entries = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut fields = l.split('\t');
                let (address, name) = (fields.next()?, fields.next()?);
                let fingerprint = fields.next().filter(|fp| from_hex::<32>(fp).is_some()).map(str::to_string);
                Some(RecentHost { address: address.to_string(), name: name.to_string(), fingerprint })
            })
            .collect();
        Self { path: path.to_path_buf(), entries }
    }

    fn add(&mut self, address: &str, name: &str, host: &Fingerprint) {
        self.entries.retain(|r| r.address != address);
        let name = name.replace(['\t', '\n'], " ");
        self.entries.insert(0, RecentHost { address: address.to_string(), name, fingerprint: Some(hex(host)) });
        self.entries.truncate(Self::MAX);
        let text: String = self
            .entries
            .iter()
            .map(|r| match &r.fingerprint {
                Some(fp) => format!("{}\t{}\t{fp}\n", r.address, r.name),
                None => format!("{}\t{}\n", r.address, r.name),
            })
            .collect();
        if let Err(e) = std::fs::write(&self.path, text) {
            tracing::warn!("save recent hosts: {e}");
        }
    }
}

/// The paired host `recent` reached: the one it says, or that its `lankvm:` target names, or (a
/// line from before LanKVM kept that) the only paired host with its name.
fn recent_host(recent: &RecentHost, hosts: &TrustStore) -> Option<Fingerprint> {
    let named = recent.fingerprint.as_deref().or(recent.address.strip_prefix("lankvm:"));
    if let Some(fp) = named {
        return from_hex(fp.trim()).filter(|fp| hosts.contains(fp));
    }
    let mut same_name = hosts.entries().iter().filter(|(_, name)| *name == recent.name);
    match (same_name.next(), same_name.next()) {
        (Some((fp, _)), None) => Some(*fp),
        _ => None,
    }
}

/// Macs paired before LanKVM kept where they are on the local network: the address last typed for
/// each, when it is one on a local network (and recent hosts can tell which Mac it reached).
fn adopt_local_addresses(recents: &Recents, trust: &Trust) {
    let hosts = trust.hosts.lock().unwrap();
    let mut book = trust.address_book.lock().unwrap();
    for recent in recents.entries.iter().filter(|r| r.fingerprint.is_none()) {
        let Some(fp) = recent_host(recent, &hosts) else { continue };
        if book.local(&fp).is_none() && internet::is_local_address(&recent.address) {
            let address = internet::with_port(&recent.address, DEFAULT_PORT);
            tracing::info!(host = %short_hex(&fp), %address, "kept from recent hosts: its address on the local network");
            book.set_local(&fp, &address);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `N` bytes in hex (a fingerprint, a key, an ID).
fn from_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
        .collect::<Option<_>>()?;
    bytes.try_into().ok()
}

/// This Mac's key for LanKVM servers (`rendezvous-key.p8`). A damaged one is replaced: paired
/// viewers then learn the new ID on their next connection on the local network.
fn load_or_create_rendezvous_identity(dir: &Path) -> Result<RendezvousIdentity> {
    RendezvousIdentity::load_or_create(dir).or_else(|e| {
        tracing::warn!("{e:#}: making a new rendezvous key");
        let _ = std::fs::remove_file(dir.join(transport::rendezvous::KEY_FILE));
        RendezvousIdentity::load_or_create(dir)
    })
}

/// `LANKVM_TEST_LOOPBACK_IS_INTERNET` is set, to anything (not only "1", which makes loopback
/// count as the internet): this instance tests internet access, and never contacts the real
/// LanKVM server unless `LANKVM_RENDEZVOUS` names it.
fn testing_internet() -> bool {
    std::env::var_os("LANKVM_TEST_LOOPBACK_IS_INTERNET").is_some()
}

/// `$LANKVM_DATA_DIR` gives an instance its own identity, trust lists and log, so a second copy
/// can run on the same Mac as a separate device (see `scripts/second-instance.sh`).
fn data_dir_override() -> Option<PathBuf> {
    std::env::var_os("LANKVM_DATA_DIR").filter(|d| !d.is_empty()).map(PathBuf::from)
}

fn data_dir() -> PathBuf {
    if let Some(dir) = data_dir_override() {
        return dir;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/Application Support/lankvm")
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    // Log to a file too: a Finder-launched app has no terminal.
    let log_path = match data_dir_override() {
        Some(dir) => std::fs::create_dir_all(&dir).ok().map(|()| dir.join("lankvm.log")),
        None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Logs/lankvm.log")),
    };
    let file = log_path.and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok());
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false);
    let _ = match file {
        Some(file) => builder.with_writer(std::io::stderr.and(Mutex::new(file))).try_init(),
        None => builder.try_init(),
    };
}

/// The endpoint on `port`, for IPv4 and IPv6 peers alike (IPv4 ones appear to quinn IPv4-mapped):
/// over IPv6 there is no router address to get around, only a firewall that punching opens.
/// IPv4 alone where IPv6 is off, or when told (`LANKVM_IPV4_ONLY=1`).
fn bind_network(port: u16, identity: &DeviceIdentity) -> Result<Network> {
    let v4 = SocketAddr::from(([0, 0, 0, 0], port));
    if std::env::var("LANKVM_IPV4_ONLY").as_deref() == Ok("1") {
        return Network::bind(v4, identity);
    }
    // macOS lets an IPv6 socket take a port another socket holds for IPv4, which then gets every
    // IPv4 packet. So the port is taken for IPv4 first, which fails as it always did when it's
    // in use (LanKVM already running), and gives port 0 one that's free for both; then let go
    // once the dual-stack socket holds it too.
    let held = std::net::UdpSocket::bind(v4)?;
    let port = held.local_addr()?.port();
    let network = Network::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)), identity);
    drop(held);
    match network {
        Ok(network) => Ok(network),
        Err(e) => {
            tracing::info!("IPv4 only: couldn't listen on IPv6: {e:#}");
            Network::bind(SocketAddr::from(([0, 0, 0, 0], port)), identity)
        }
    }
}

/// `addr` with an IPv4-mapped IPv6 address turned into the IPv4 one: how a peer's address is
/// shown and stored (the endpoint's socket is dual-stack, so quinn knows IPv4 peers mapped).
/// Other IPv6 addresses stay as they are, scope included.
pub(crate) fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => v6.ip().to_ipv4_mapped().map_or(addr, |v4| SocketAddr::new(v4.into(), v6.port())),
        SocketAddr::V4(_) => addr,
    }
}

/// The IPv6 address this Mac sends from to the internet, if it has a global one: the address
/// macOS picks (a temporary one, usually), which is where a peer reaches it over IPv6, as the
/// firewall in front lets in answers to what went out from there.
pub(crate) fn internet_v6() -> Option<Ipv6Addr> {
    let socket = std::net::UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).ok()?;
    // Sends nothing: connecting only picks the route, and the source address with it.
    socket.connect(SocketAddr::from(([0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888], 53))).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V6(ip) if is_global_v6(ip) => Some(ip),
        _ => None,
    }
}

/// Global unicast (2000::/3), not documentation (2001:db8::/32) or Teredo (2001::/32).
fn is_global_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] & 0xe000 == 0x2000 && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] == 0))
}

/// Whether `ip` belongs to this Mac (loopback or one of its interfaces).
pub(crate) fn is_this_mac(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    ip.is_loopback() || if_addrs::get_if_addrs().unwrap_or_default().iter().any(|i| i.ip() == ip)
}

/// Whether `ip` is another device on one of this Mac's local networks: inside the subnet of one
/// of its interfaces (loopback and point-to-point links aside), and not this Mac itself. A host's
/// address on its own network leads to it only then: elsewhere the same private address is
/// someone else's, or nobody's. Link-local addresses never count: every link has the same
/// subnet, so it says nothing about being on the same one.
pub(crate) fn on_this_network(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    let link_local = match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => v6.segments()[0] & 0xffc0 == 0xfe80,
    };
    let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
    if link_local || ip.is_loopback() || ip.is_unspecified() || interfaces.iter().any(|i| i.ip() == ip) {
        return false;
    }
    interfaces.iter().filter(|i| !i.is_loopback() && !i.is_p2p).any(|i| match (&i.addr, ip) {
        (if_addrs::IfAddr::V4(a), IpAddr::V4(ip)) => {
            let mask = u32::from(a.netmask);
            a.prefixlen > 0 && u32::from(a.ip) & mask == u32::from(ip) & mask
        }
        (if_addrs::IfAddr::V6(a), IpAddr::V6(ip)) => {
            let mask = u128::from(a.netmask);
            a.prefixlen > 0 && u128::from(a.ip) & mask == u128::from(ip) & mask
        }
        _ => false,
    })
}

/// Private IPv4 addresses of this Mac, i.e. what to type on the other machine.
pub(crate) fn local_addresses() -> Vec<String> {
    let mut addrs: Vec<String> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.ip() {
            IpAddr::V4(v4) if v4.is_private() || v4.is_link_local() => Some(v4.to_string()),
            _ => None,
        })
        .collect();
    addrs.sort();
    addrs.dedup();
    addrs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard() {
        let enc = |b: &[u8]| {
            let mut out = Vec::new();
            base64(b, &mut serde_json::Serializer::new(&mut out)).unwrap();
            String::from_utf8(out).unwrap()
        };
        assert_eq!(enc(b""), "\"\"");
        assert_eq!(enc(b"f"), "\"Zg==\"");
        assert_eq!(enc(b"fo"), "\"Zm8=\"");
        assert_eq!(enc(b"foo"), "\"Zm9v\"");
        assert_eq!(enc(b"\x89PNG\r\n"), "\"iVBORw0K\"");
    }

    #[test]
    fn this_network_is_the_subnets_of_this_macs_interfaces() {
        assert!(!on_this_network("127.0.0.1".parse().unwrap()));
        assert!(!on_this_network("203.0.113.7".parse().unwrap()));
        assert!(!on_this_network("169.254.3.4".parse().unwrap()), "every link has that subnet");
        // Another address in the subnet of each of this Mac's interfaces that has one with room:
        // on this network. The interface's own address: this Mac, not another device.
        let interfaces = if_addrs::get_if_addrs().unwrap_or_default();
        for i in interfaces.iter().filter(|i| !i.is_loopback() && !i.is_p2p) {
            let if_addrs::IfAddr::V4(a) = &i.addr else { continue };
            if !(1..=30).contains(&a.prefixlen) || a.ip.is_link_local() {
                continue;
            }
            let neighbour = std::net::Ipv4Addr::from(u32::from(a.ip) ^ 1);
            if interfaces.iter().any(|other| other.ip() == IpAddr::V4(neighbour)) {
                continue;
            }
            assert!(on_this_network(neighbour.into()), "{neighbour} next to {} on {}", a.ip, i.name);
            assert!(!on_this_network(a.ip.into()), "{} is this Mac", a.ip);
        }
    }

    #[test]
    fn hex_round_trip() {
        let fp: Fingerprint = core::array::from_fn(|i| i as u8 * 7);
        assert_eq!(from_hex(&hex(&fp)), Some(fp));
        assert_eq!(from_hex::<32>("zz"), None);
        assert_eq!(from_hex::<2>("0aff"), Some([10, 255]));
        assert_eq!(from_hex::<2>("0aff00"), None, "the wrong size");
    }

    #[test]
    fn recents_dedupe_and_order() {
        let path = std::env::temp_dir().join(format!("lankvm-recents-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut r = Recents::load(&path);
        r.add("192.168.1.2", "Studio", &[1; 32]);
        r.add("192.168.1.3", "Mini", &[2; 32]);
        r.add("192.168.1.2", "Studio", &[1; 32]);
        let r = Recents::load(&path);
        let addrs: Vec<_> = r.entries.iter().map(|e| e.address.as_str()).collect();
        assert_eq!(addrs, ["192.168.1.2", "192.168.1.3"]);
        assert_eq!(r.entries[1].fingerprint, Some(hex(&[2; 32])));
        let _ = std::fs::remove_file(&path);
    }

    /// A data directory of its own, gone at the end.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("lankvm-lib-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn trust(&self, hosts: &[(u8, &str)]) -> Trust {
            let mut store = TrustStore::load(&self.0.join("trusted-hosts.txt"));
            for &(n, name) in hosts {
                store.add([n; 32], name).unwrap();
            }
            Trust {
                fingerprint: [0; 32],
                viewers: Mutex::new(TrustStore::load(&self.0.join("trusted-viewers.txt"))),
                hosts: Mutex::new(store),
                internet_hosts: Mutex::new(InternetHosts::load(&self.0.join("internet-hosts.json"))),
                address_book: Mutex::new(AddressBook::load(&self.0.join("address-book.json"))),
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn recent_lines_from_before_fingerprints_still_load() {
        let dir = TempDir::new("old-recents");
        let path = dir.0.join("recent-hosts.txt");
        std::fs::write(&path, format!("192.168.1.2\tStudio\n192.168.1.3\tMini\t{}\n10.0.0.4\tOld\tnot-hex\n", hex(&[2; 32]))).unwrap();
        let r = Recents::load(&path);
        let found: Vec<_> = r.entries.iter().map(|e| (e.address.as_str(), e.name.as_str(), e.fingerprint.clone())).collect();
        assert_eq!(
            found,
            [("192.168.1.2", "Studio", None), ("192.168.1.3", "Mini", Some(hex(&[2; 32]))), ("10.0.0.4", "Old", None)]
        );
    }

    #[test]
    fn recent_hosts_say_which_paired_mac_they_reached() {
        let dir = TempDir::new("recent-host");
        let trust = dir.trust(&[(1, "Studio"), (2, "MacBook Pro"), (3, "MacBook Pro")]);
        let hosts = trust.hosts.lock().unwrap();
        let recent = |address: &str, name: &str, fingerprint: Option<[u8; 32]>| RecentHost {
            address: address.into(),
            name: name.into(),
            fingerprint: fingerprint.map(|fp| hex(&fp)),
        };
        assert_eq!(recent_host(&recent("192.168.1.9", "Anything", Some([2; 32])), &hosts), Some([2; 32]), "as it says");
        assert_eq!(recent_host(&recent(&format!("lankvm:{}", hex(&[3; 32])), "", None), &hosts), Some([3; 32]));
        assert_eq!(recent_host(&recent("192.168.1.9", "Studio", None), &hosts), Some([1; 32]), "the only Studio");
        assert_eq!(recent_host(&recent("192.168.1.9", "MacBook Pro", None), &hosts), None, "two of them");
        assert_eq!(recent_host(&recent("192.168.1.9", "Studio", Some([9; 32])), &hosts), None, "forgotten");
    }

    #[test]
    fn macs_paired_before_keep_their_local_address_from_recent_hosts() {
        let dir = TempDir::new("adopt");
        let trust = dir.trust(&[(1, "Studio"), (2, "Mini"), (3, "Laptop"), (4, "Pro"), (5, "Pro")]);
        trust.address_book.lock().unwrap().set_local(&[2; 32], "192.168.1.77:47800");
        let path = dir.0.join("recent-hosts.txt");
        let lines = [
            format!("lankvm:{}\tStudio", hex(&[1; 32])),
            "192.168.1.31\tStudio".to_string(),
            "studio.local\tStudio".to_string(),
            "192.168.1.40\tMini".to_string(),
            "203.0.113.7\tLaptop".to_string(),
            "192.168.1.50\tPro".to_string(),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        adopt_local_addresses(&Recents::load(&path), &trust);
        let book = trust.address_book.lock().unwrap();
        assert_eq!(book.local(&[1; 32]).as_deref(), Some("192.168.1.31:47800"), "the newest address on a local network");
        assert_eq!(book.local(&[2; 32]).as_deref(), Some("192.168.1.77:47800"), "already known");
        assert_eq!(book.local(&[3; 32]), None, "a public address isn't a local one");
        assert_eq!((book.local(&[4; 32]), book.local(&[5; 32])), (None, None), "which Pro?");
    }
}
