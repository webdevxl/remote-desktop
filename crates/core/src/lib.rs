//! LanKVM core: networking, capture/encode, decode/render, pairing. Everything except the UI.
//!
//! The SwiftUI app drives it through the C ABI in [`ffi`] and receives [`Event`]s as JSON.

mod client;
pub mod ffi;
mod host;
mod render;
mod stats;
mod view;

use std::collections::HashMap;
use std::ffi::c_void;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use platform_mac::{permissions, system};
use protocol::DEFAULT_PORT;
use quinn::Endpoint;
use serde::Serialize;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriterExt;
use transport::identity::{DeviceIdentity, Fingerprint, short_hex};
use transport::pairing::TrustStore;

use crate::client::{Session, SessionEvent, SessionInfo};

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
}

pub type EventSink = Arc<dyn Fn(Event) + Send + Sync>;

/// This device's identity and the devices it has paired with.
pub struct Trust {
    pub fingerprint: Fingerprint,
    /// Devices allowed to view and control this Mac.
    pub viewers: Mutex<TrustStore>,
    /// Macs this one has paired with as a viewer.
    pub hosts: Mutex<TrustStore>,
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
}

pub struct Core {
    rt: tokio::runtime::Runtime,
    endpoint: Endpoint,
    trust: Arc<Trust>,
    host: Arc<host::HostCtx>,
    events: EventSink,
    this_mac: ThisMac,
    recents: Mutex<Recents>,
    sessions: Mutex<HashMap<u64, Arc<Session>>>,
    next_session: AtomicU64,
}

impl Core {
    pub fn start(events: EventSink) -> Result<Arc<Self>> {
        init_logging();
        let dir = data_dir();
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let identity = DeviceIdentity::load_or_create(&dir)?;
        let port = std::env::var("LANKVM_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
        let endpoint = {
            let _guard = rt.enter();
            transport::endpoint::make_endpoint(SocketAddr::from(([0, 0, 0, 0], port)), &identity)
                .with_context(|| format!("Can't listen on UDP port {port}. Is LanKVM already running?"))?
        };
        let device_id = short_hex(&identity.fingerprint);
        tracing::info!(addr = %endpoint.local_addr()?, %device_id, "listening");

        let trust = Arc::new(Trust {
            fingerprint: identity.fingerprint,
            viewers: Mutex::new(TrustStore::load(&dir.join("trusted-viewers.txt"))),
            hosts: Mutex::new(TrustStore::load(&dir.join("trusted-hosts.txt"))),
        });
        let host = Arc::new(host::HostCtx {
            status: Mutex::new(host::HostStatus::default()),
            events: events.clone(),
            trust: trust.clone(),
        });
        rt.spawn(host::run(endpoint.clone(), host.clone()));

        let this_mac = ThisMac { name: system::device_name(), addresses: local_addresses(), port, device_id };
        Ok(Arc::new(Self {
            rt,
            endpoint,
            trust,
            host,
            events,
            this_mac,
            recents: Mutex::new(Recents::load(&dir.join("recent-hosts.txt"))),
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
        }))
    }

    pub fn this_mac(&self) -> &ThisMac {
        &self.this_mac
    }

    pub fn host_status(&self) -> host::HostStatusView {
        self.host.view()
    }

    pub fn paired_devices(&self) -> PairedDevices {
        let list = |store: &Mutex<TrustStore>| {
            store
                .lock()
                .unwrap()
                .entries()
                .iter()
                .map(|(fp, name)| PairedDevice { fingerprint: hex(fp), device_id: short_hex(fp), name: name.clone() })
                .collect()
        };
        PairedDevices { viewers: list(&self.trust.viewers), hosts: list(&self.trust.hosts) }
    }

    pub fn recent_hosts(&self) -> Vec<RecentHost> {
        self.recents.lock().unwrap().entries.clone()
    }

    /// Opens a viewer session; progress arrives as events tagged with the returned id.
    pub fn connect(self: &Arc<Self>, target: &str, max_size: (u32, u32)) -> u64 {
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let target = target.trim().to_string();
        let weak = Arc::downgrade(self);
        let typed = target.clone();
        let on_event = Arc::new(move |event: SessionEvent| {
            let Some(core) = weak.upgrade() else { return };
            let event = match event {
                SessionEvent::PinNeeded => Event::PinNeeded { session: id },
                SessionEvent::Connected(info) => {
                    core.recents.lock().unwrap().add(&typed, &info.host_name);
                    Event::Connected { session: id, info }
                }
                SessionEvent::Ended { error } => {
                    // Drop the session off this thread: it may be the session's own task.
                    let ended = core.sessions.lock().unwrap().remove(&id);
                    std::thread::spawn(move || drop(ended));
                    Event::Ended { session: id, error }
                }
            };
            (core.events)(event);
        });
        let session = Session::start(
            self.rt.handle(),
            self.endpoint.clone(),
            self.trust.clone(),
            target,
            max_size,
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
        if kind != "host" {
            self.host.close_viewers_with(&fp);
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
    pub name: String,
}

#[derive(Serialize)]
pub struct PairedDevices {
    pub viewers: Vec<PairedDevice>,
    pub hosts: Vec<PairedDevice>,
}

/// Recently connected hosts, newest first, as `<address>\t<name>` lines.
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
                let (address, name) = l.split_once('\t')?;
                Some(RecentHost { address: address.to_string(), name: name.to_string() })
            })
            .collect();
        Self { path: path.to_path_buf(), entries }
    }

    fn add(&mut self, address: &str, name: &str) {
        self.entries.retain(|r| r.address != address);
        self.entries.insert(0, RecentHost { address: address.to_string(), name: name.replace(['\t', '\n'], " ") });
        self.entries.truncate(Self::MAX);
        let text: String = self.entries.iter().map(|r| format!("{}\t{}\n", r.address, r.name)).collect();
        if let Err(e) = std::fs::write(&self.path, text) {
            tracing::warn!("save recent hosts: {e}");
        }
    }
}

fn hex(fp: &Fingerprint) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Fingerprint> {
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
        .collect::<Option<_>>()?;
    bytes.try_into().ok()
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
        .unwrap_or_else(|_| EnvFilter::new("info,wgpu_core=warn,wgpu_hal=warn,naga=warn"));
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

/// Private IPv4 addresses of this Mac, i.e. what to type on the other machine.
fn local_addresses() -> Vec<String> {
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
    fn hex_round_trip() {
        let fp: Fingerprint = core::array::from_fn(|i| i as u8 * 7);
        assert_eq!(from_hex(&hex(&fp)), Some(fp));
        assert_eq!(from_hex("zz"), None);
    }

    #[test]
    fn recents_dedupe_and_order() {
        let path = std::env::temp_dir().join(format!("lankvm-recents-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut r = Recents::load(&path);
        r.add("192.168.1.2", "Studio");
        r.add("192.168.1.3", "Mini");
        r.add("192.168.1.2", "Studio");
        let r = Recents::load(&path);
        let addrs: Vec<_> = r.entries.iter().map(|e| e.address.as_str()).collect();
        assert_eq!(addrs, ["192.168.1.2", "192.168.1.3"]);
        let _ = std::fs::remove_file(&path);
    }
}
