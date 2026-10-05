//! Internet access: paired Macs connecting from outside the local network.
//!
//! Host side ([`HostInternet`]): a secret only this Mac knows gives each paired viewer its own
//! access key (`transport::knock::access_key`), handed over on the local network after every
//! connect. While internet access is on, the gate in front of the endpoint answers knocks made
//! with those keys and nothing else, the router is asked to forward the port, and this Mac
//! registers with its LanKVM server, which introduces paired viewers with no router setup (see
//! [`crate::rendezvous`]). Forgetting a viewer drops its key, so its knocks and requests go
//! unanswered from then on.
//!
//! Viewer side ([`InternetHosts`]): the keys hosts gave this Mac, with the addresses they
//! announced, the ones that reached them, and the LanKVM server each registers with, to pick
//! which keys to knock with and how to reach a paired host.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use platform_mac::portmap::{MapProblem, MapState, PortMapping};
use protocol::DEFAULT_PORT;
use serde::{Deserialize, Serialize};
use transport::gate::Gate;
use transport::identity::{Fingerprint, write_private};
use transport::knock::{AccessKey, access_key, random_bytes};
use transport::pairing::TrustStore;
use transport::rendezvous::RendezvousId;

use crate::dht::{DhtRendezvous, DhtView};
use crate::rendezvous::{Rendezvous, ServerView};
use crate::{Event, EventSink, Trust, from_hex, hex};

/// The secret access keys are derived from, in the data directory.
const SECRET_FILE: &str = "internet-secret.key";
/// Addresses a viewer remembers having reached a host at, newest first.
const MAX_USED: usize = 4;
/// Addresses a host may announce (it sends two at most), and their length: a host can't make
/// this Mac store much.
const MAX_ANNOUNCED: usize = 8;
const MAX_ADDRESS_LEN: usize = 255;

/// This Mac's secret for internet access keys: 32 random bytes, made on first start and readable
/// only by this user. A new secret would revoke every key handed out.
pub(crate) fn load_or_create_secret(dir: &Path) -> Result<[u8; 32]> {
    let path = dir.join(SECRET_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => match <[u8; 32]>::try_from(bytes.as_slice()) {
            Ok(secret) => {
                keep_private(&path);
                return Ok(secret);
            }
            Err(_) => tracing::warn!(
                "{} is damaged: making a new one (paired Macs get their keys again on the local network)",
                path.display()
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    let secret = random_bytes();
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    write_private(&path, &secret).with_context(|| format!("write {}", path.display()))?;
    Ok(secret)
}

/// Makes `path` readable only by this user again if something (a restore from a backup, say)
/// left it readable by others.
fn keep_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.permissions().mode() & 0o077 != 0 {
        tracing::warn!("{} was readable by other users: making it private", path.display());
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!("make {} private: {e}", path.display());
        }
    }
}

/// The host's side of internet access: the setting in effect, the gate's keys, and the port
/// mapping on the router. Persisting the settings and ending sessions is up to `HostCtx`.
pub(crate) struct HostInternet {
    secret: [u8; 32],
    gate: Arc<Gate>,
    trust: Arc<Trust>,
    /// The endpoint's UDP port: what the router is asked to forward (to the same port outside,
    /// if it can).
    port: u16,
    events: EventSink,
    enabled: AtomicBool,
    public_address: Mutex<String>,
    /// Held while the gate's keys are worked out and set, so a change made at the same time
    /// (pairing, Forget, the setting) can't put back keys that were just dropped.
    keys: Mutex<()>,
    router: Router,
    /// The registration with the LanKVM server.
    rendezvous: Arc<Rendezvous>,
    /// The notes for paired viewers on the BitTorrent DHT.
    dht: Arc<DhtRendezvous>,
}

/// Asking the router to forward the port.
#[derive(Default)]
struct Router {
    /// Held while a mapping starts or stops, so a new one never starts before the old one is
    /// deleted (UPnP deletes by port). The mapping's callback never takes it.
    slot: Mutex<Slot>,
    /// What the router said last. None while not asking.
    state: Arc<Mutex<Option<MapState>>>,
}

#[derive(Default)]
struct Slot {
    mapping: Option<PortMapping>,
    /// LanKVM is quitting: no new mappings.
    closed: bool,
}

impl HostInternet {
    pub(crate) fn new(
        secret: [u8; 32],
        gate: Arc<Gate>,
        trust: Arc<Trust>,
        port: u16,
        events: EventSink,
        enabled: bool,
        public_address: String,
        rendezvous: Arc<Rendezvous>,
        dht: Arc<DhtRendezvous>,
    ) -> Arc<Self> {
        Arc::new(Self {
            secret,
            gate,
            trust,
            port,
            events,
            enabled: AtomicBool::new(enabled),
            public_address: Mutex::new(public_address),
            keys: Mutex::new(()),
            router: Router::default(),
            rendezvous,
            dht,
        })
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// The key `viewer` knocks with.
    pub(crate) fn access_key(&self, viewer: &Fingerprint) -> AccessKey {
        access_key(&self.secret, viewer)
    }

    /// Whether `addr` is on the internet, not the local network.
    pub(crate) fn is_internet(&self, addr: SocketAddr) -> bool {
        self.gate.is_internet(addr)
    }

    /// Whether a connection with `addr` goes through a LanKVM server's relay.
    pub(crate) fn is_relayed(&self, addr: SocketAddr) -> bool {
        self.gate.is_relayed(addr)
    }

    /// Turns internet access on or off: the gate's keys at once, the router and the LanKVM
    /// server in the background.
    pub(crate) fn set_enabled(self: &Arc<Self>, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
        self.sync_keys();
        self.update_mapping();
        self.rendezvous.set_host_enabled(on);
    }

    /// Whether this Mac meets paired Macs through the BitTorrent DHT too.
    pub(crate) fn set_dht(&self, on: bool) {
        self.dht.set_enabled(on);
    }

    /// The LanKVM server to register with while internet access is on ("host:port"; "" for
    /// none). True if the registration changed.
    pub(crate) fn set_rendezvous_server(&self, server: &str) -> bool {
        self.rendezvous.set_host_server(server)
    }

    /// The LanKVM server viewers are told to ask for this Mac at, and the ID to ask for; empty
    /// while it doesn't register there.
    pub(crate) fn rendezvous_announced(&self) -> (String, Vec<u8>) {
        self.rendezvous.announced()
    }

    pub(crate) fn set_public_address(&self, address: String) {
        *self.public_address.lock().unwrap() = address;
    }

    /// Lets in the knocks of every trusted viewer while internet access is on, and none while it
    /// is off, and keeps notes on the DHT for the same viewers. Call whenever either changes.
    pub(crate) fn sync_keys(&self) {
        let _one_at_a_time = self.keys.lock().unwrap();
        let keys: Option<Vec<(Fingerprint, AccessKey)>> = self.enabled().then(|| {
            let viewers = self.trust.viewers.lock().unwrap();
            viewers.entries().iter().map(|(fp, _)| (*fp, self.access_key(fp))).collect()
        });
        self.dht.set_viewers(keys.clone().unwrap_or_default());
        self.gate.set_keys(keys);
    }

    /// Asks the router to forward the port while internet access is on, and stops asking once
    /// it is off. Stopping waits for the mapping's thread (a moment, or about 10 s while
    /// mDNSResponder hangs), so this happens on a thread of its own and returns at once.
    pub(crate) fn update_mapping(self: &Arc<Self>) {
        let this = self.clone();
        if let Err(e) = std::thread::Builder::new().name("lankvm-portmap-update".into()).spawn(move || this.apply_mapping()) {
            tracing::warn!("couldn't update the port mapping: {e}");
        }
    }

    /// Brings the mapping in line with the setting. One at a time, and each looks at the setting
    /// once it runs, so the last one leaves it right.
    pub(crate) fn apply_mapping(&self) {
        let mut slot = self.router.slot.lock().unwrap();
        if slot.closed {
            return;
        }
        let enabled = self.enabled();
        // Testing on one Mac: loopback is the internet, and its address this Mac's public one.
        // Nothing on the router needs forwarding then.
        let loopback = self.gate.loopback_is_internet();
        let wanted = enabled && !loopback;
        if !wanted && let Some(mapping) = slot.mapping.take() {
            mapping.stop();
        }
        let before = *self.router.state.lock().unwrap();
        if wanted && slot.mapping.is_none() {
            let (state, events) = (self.router.state.clone(), self.events.clone());
            let mapping = PortMapping::start(self.port, self.port, move |new| {
                *state.lock().unwrap() = Some(new);
                events(Event::HostChanged);
            });
            // Requesting, or Failed if the mapping's thread couldn't start (no callback comes
            // then). Read under the lock the callback takes, so an answer that already came in
            // stays.
            let mut state = self.router.state.lock().unwrap();
            *state = Some(mapping.state());
            drop(state);
            slot.mapping = Some(mapping);
        } else if !wanted {
            let external = SocketAddrV4::new(Ipv4Addr::LOCALHOST, self.port);
            *self.router.state.lock().unwrap() = (enabled && loopback).then_some(MapState::PublicAddress { external });
        }
        let after = *self.router.state.lock().unwrap();
        drop(slot);
        if after != before {
            (self.events)(Event::HostChanged);
        }
    }

    /// When LanKVM quits: deletes the mapping on the router, waiting for that at most `wait`.
    /// mDNSResponder deletes it anyway once LanKVM has quit, so a slow daemon (or a mapping
    /// being stopped right then) doesn't hold up quitting.
    pub(crate) fn stop_mapping(self: &Arc<Self>, wait: Duration) {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let this = self.clone();
        let stopping = std::thread::Builder::new().name("lankvm-portmap-stop".into()).spawn(move || {
            let mut slot = this.router.slot.lock().unwrap();
            slot.closed = true;
            if let Some(mapping) = slot.mapping.take() {
                mapping.stop();
            }
            drop(done_tx);
        });
        if let Err(e) = stopping {
            tracing::warn!("couldn't stop the port mapping: {e}");
        } else if done_rx.recv_timeout(wait) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            tracing::warn!("the port mapping didn't stop in time: mDNSResponder removes it once LanKVM quits");
        }
    }

    fn map_state(&self) -> Option<MapState> {
        *self.router.state.lock().unwrap()
    }

    /// Where paired Macs are told to reach this one over the internet; empty while internet
    /// access is off. Includes the address the LanKVM server sees: the router doesn't forward it
    /// by itself, but a viewer that knows it takes the public IP typed for it as this Mac's and
    /// connects through the server.
    pub(crate) fn announced_addresses(&self) -> Vec<String> {
        let mut addresses = self.internet_addresses();
        // And where it is on its own network, which viewers keep apart: one at home with it that
        // was given the public IP connects there directly. (Not while loopback stands in for the
        // internet: the viewer is on this Mac, and its local network would always win.)
        if self.enabled() && !self.gate.loopback_is_internet() {
            addresses.extend(crate::local_addresses().into_iter().map(|ip| format!("{ip}:{}", self.port)));
        }
        addresses
    }

    /// [`Self::announced_addresses`] on the internet only. (Not where DHT nodes see this Mac:
    /// strangers say that, and a viewer knocks at every address it is told. That one goes only
    /// into this Mac's sealed notes on the DHT, where a viewer looking there finds it.)
    fn internet_addresses(&self) -> Vec<String> {
        if !self.enabled() {
            return Vec::new();
        }
        let mut addresses = announced(&self.public_address.lock().unwrap(), self.map_state(), self.port);
        if let Some(observed) = self.rendezvous.host_observed().map(|o| o.to_string())
            && !addresses.iter().any(|a| same_address(a, &observed))
        {
            addresses.push(observed);
        }
        addresses
    }

    /// Where this Mac's DHT notes say it is, besides where DHT nodes see it: its addresses on
    /// the internet that are IPs (the router's forwarded port, the public address set), then
    /// those on its own network. Empty while internet access is off.
    pub(crate) fn dht_addresses(&self) -> Vec<SocketAddr> {
        self.announced_addresses().iter().filter_map(|a| with_port(a, self.port).parse().ok()).collect()
    }

    pub(crate) fn view(&self) -> InternetView {
        let public_address = self.public_address.lock().unwrap().clone();
        let mut view = build_view(self.enabled(), self.map_state(), self.port, public_address, lan_address(), self.gate.stats().ignored);
        view.server = self.rendezvous.host_view();
        view.dht = self.dht.view(self.enabled());
        view.announced = self.internet_addresses();
        view
    }
}

/// Internet access as the UI sees it (`InternetStatus` in the app).
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InternetView {
    /// The setting.
    pub enabled: bool,
    /// "off", "requesting" (asking the router), "mapped" (it forwards the port), "public" (this
    /// Mac's own address is public) or "problem".
    pub state: &'static str,
    /// In state "problem": "noResponse", "unsupported", "disabled", "doubleNat", "cgnat",
    /// "noRouter", "serviceDown", "firewall" or "other".
    pub problem: Option<&'static str>,
    /// Where the internet reaches this Mac ("203.0.113.7:47800"), in states "mapped" and "public".
    pub external_address: Option<String>,
    /// The router's own outside address when that isn't public ("doubleNat", "cgnat").
    pub router_address: Option<String>,
    /// This Mac's address on the local network, to forward the port to by hand.
    pub local_address: Option<String>,
    /// The UDP port LanKVM listens on.
    pub port: u16,
    /// The setting: the address the user gave for paired Macs to use, or "".
    pub public_address: String,
    /// The addresses paired Macs are told to use.
    pub announced: Vec<String>,
    /// Packets and connection attempts from the internet ignored since LanKVM started.
    pub ignored: u64,
    /// The LanKVM server that introduces paired Macs to this one.
    pub server: ServerView,
    /// The BitTorrent DHT, through which paired Macs find this one with no server.
    pub dht: DhtView,
}

fn build_view(
    enabled: bool,
    map: Option<MapState>,
    port: u16,
    public_address: String,
    local_address: Option<String>,
    ignored: u64,
) -> InternetView {
    let mut view = InternetView {
        enabled,
        state: "off",
        problem: None,
        external_address: None,
        router_address: None,
        local_address,
        port,
        announced: if enabled { announced(&public_address, map, port) } else { Vec::new() },
        public_address,
        ignored,
        server: ServerView::default(),
        dht: DhtView::default(),
    };
    if !enabled {
        return view;
    }
    view.state = match map {
        None | Some(MapState::Requesting) => "requesting",
        Some(MapState::Mapped { external }) => {
            view.external_address = Some(external.to_string());
            "mapped"
        }
        Some(MapState::PublicAddress { external }) => {
            view.external_address = Some(external.to_string());
            "public"
        }
        Some(MapState::Failed(problem)) => {
            view.problem = Some(problem_name(problem));
            view.router_address = match problem {
                MapProblem::DoubleNat { router_external } => router_external.map(|ip| ip.to_string()),
                MapProblem::Cgnat { router_external } => Some(router_external.to_string()),
                _ => None,
            };
            "problem"
        }
    };
    view
}

fn problem_name(problem: MapProblem) -> &'static str {
    match problem {
        MapProblem::NoResponse => "noResponse",
        MapProblem::Unsupported => "unsupported",
        MapProblem::Disabled => "disabled",
        MapProblem::DoubleNat { .. } => "doubleNat",
        MapProblem::Cgnat { .. } => "cgnat",
        MapProblem::NoRouter => "noRouter",
        MapProblem::ServiceDown => "serviceDown",
        MapProblem::Firewall => "firewall",
        MapProblem::Other(_) => "other",
    }
}

/// The public address the user gave (with the port added when it names none: the one the
/// router forwards, else this Mac's), then the one the router reported, without repeats.
fn announced(public_address: &str, map: Option<MapState>, port: u16) -> Vec<String> {
    let external = match map {
        Some(MapState::Mapped { external } | MapState::PublicAddress { external }) => Some(external),
        _ => None,
    };
    let mut addresses: Vec<String> = Vec::new();
    if !public_address.trim().is_empty() {
        addresses.push(with_port(public_address, external.map_or(port, |e| e.port())));
    }
    if let Some(external) = external {
        let external = external.to_string();
        if !addresses.iter().any(|a| same_address(a, &external)) {
            addresses.push(external);
        }
    }
    addresses
}

/// `address` with `port` added when it names none: "home.example.com" becomes
/// "home.example.com:47800" and "2001:db8::7" becomes "[2001:db8::7]:47800". IP addresses come
/// out in their usual form.
pub(crate) fn with_port(address: &str, port: u16) -> String {
    let address = address.trim();
    if let Ok(addr) = address.parse::<SocketAddr>() {
        return addr.to_string();
    }
    if let Ok(ip) = address.parse::<IpAddr>() {
        return SocketAddr::new(ip, port).to_string();
    }
    let address = address.strip_suffix(':').unwrap_or(address);
    if let Some(ip) = address.strip_prefix('[').and_then(|a| a.strip_suffix(']')).and_then(|a| a.parse::<IpAddr>().ok()) {
        return SocketAddr::new(ip, port).to_string();
    }
    match address.rsplit_once(':') {
        Some((host, p)) if !host.is_empty() && !host.contains(':') && p.parse::<u16>().is_ok() => address.to_string(),
        _ => format!("{address}:{port}"),
    }
}

/// An address in one form for comparing: lowercase, with the default port when it names none.
fn normalized(address: &str) -> String {
    with_port(address, DEFAULT_PORT).to_ascii_lowercase()
}

pub(crate) fn same_address(a: &str, b: &str) -> bool {
    normalized(a) == normalized(b)
}

/// Whether `address` is an IP on a local network (private, link-local or unique-local), not one
/// to reach over the internet. Loopback isn't: tests stand it in for the internet.
fn is_lan_address(address: &str) -> bool {
    match with_port(address, DEFAULT_PORT).parse::<SocketAddr>().map(|a| a.ip().to_canonical()) {
        Ok(IpAddr::V4(v4)) => v4.is_private() || v4.is_link_local(),
        Ok(IpAddr::V6(v6)) => (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80,
        Err(_) => false,
    }
}

/// Whether `address` leads to a local network, not the internet: an IP there ([`is_lan_address`])
/// or on this Mac (loopback), or a Bonjour name ("studio.local").
pub(crate) fn is_local_address(address: &str) -> bool {
    let address = address.trim();
    let host = with_port(address, DEFAULT_PORT).rsplit_once(':').map(|(host, _)| host.trim_matches(['[', ']']).to_ascii_lowercase());
    is_lan_address(address)
        || host.as_deref().and_then(|h| h.parse::<IpAddr>().ok()).is_some_and(|ip| ip.to_canonical().is_loopback())
        || host.is_some_and(|h| h.trim_end_matches('.').ends_with(".local"))
}

/// The addresses a host announced, fit to keep: trimmed, without empty, overlong or repeated
/// ones, and only the first few. Only so many are looked at, so a host sending a great many
/// costs this Mac next to nothing.
pub(crate) fn clean_announced(addresses: Vec<String>) -> Vec<String> {
    let mut announced: Vec<String> = Vec::new();
    for address in addresses.iter().take(MAX_ANNOUNCED * 4) {
        let address = address.trim();
        if !address.is_empty() && address.len() <= MAX_ADDRESS_LEN && !announced.iter().any(|a| same_address(a, address)) {
            announced.push(address.to_string());
            if announced.len() == MAX_ANNOUNCED {
                break;
            }
        }
    }
    announced
}

/// The LanKVM server and ID a host said to ask for it by, fit to keep: None when it named none,
/// or something no server could be (an ID of the wrong size, an overlong name).
pub(crate) fn clean_rendezvous(server: String, id: Vec<u8>) -> Option<(String, RendezvousId)> {
    let server = server.trim();
    let id = RendezvousId::try_from(id.as_slice()).ok()?;
    (!server.is_empty() && server.len() <= MAX_ADDRESS_LEN).then(|| (server.to_string(), id))
}

/// This Mac's private IPv4 address on the local network, preferring Ethernet and Wi-Fi (en*)
/// over VPNs and bridges.
fn lan_address() -> Option<String> {
    let private: Vec<_> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback() && matches!(i.ip(), IpAddr::V4(v4) if v4.is_private()))
        .collect();
    private.iter().find(|i| i.name.starts_with("en")).or(private.first()).map(|i| i.ip().to_string())
}

/// What this Mac knows about reaching hosts over the internet: the access key each gave it, the
/// addresses it announced, the ones that reached it, and the LanKVM server it registers with.
/// Stored in `internet-hosts.json`, readable only by this user (the keys let this Mac in).
pub(crate) struct InternetHosts {
    path: PathBuf,
    hosts: BTreeMap<Fingerprint, InternetHost>,
}

#[derive(Clone, Debug, PartialEq)]
struct InternetHost {
    key: AccessKey,
    /// What the host said to use, as of the last connection.
    announced: Vec<String>,
    /// What reached it, as typed, newest first.
    used: Vec<String>,
    /// The LanKVM server it registers with and its ID there, as of the last connection.
    rendezvous: Option<(String, RendezvousId)>,
    /// Its addresses on its own local network, as of the last connection: when this Mac is on
    /// that network too (both at home, the public IP typed), connecting there is direct.
    lan: Vec<String>,
}

/// An entry of `internet-hosts.json`, keyed by the host's fingerprint (hex).
#[derive(Serialize, Deserialize)]
struct Stored {
    key: String,
    #[serde(default)]
    announced: Vec<String>,
    #[serde(default)]
    used: Vec<String>,
    /// The LanKVM server ("host:port") and the host's ID there (hex); absent when none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    server: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    lan: Vec<String>,
}

/// How to reach a paired host over the internet (see [`InternetHosts::way_to`]).
pub(crate) struct Way {
    pub(crate) key: AccessKey,
    /// Where it was reached and where it said to reach it, in that order.
    pub(crate) addresses: Vec<String>,
    /// Its LanKVM server and its ID there.
    pub(crate) rendezvous: Option<(String, RendezvousId)>,
    /// Its addresses on its local network.
    pub(crate) lan: Vec<String>,
}

impl InternetHosts {
    pub(crate) fn load(path: &Path) -> Self {
        let stored: BTreeMap<String, Stored> =
            std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let hosts = stored
            .into_iter()
            .filter_map(|(fp, s)| {
                let rendezvous = from_hex(&s.id).filter(|_| !s.server.is_empty()).map(|id| (s.server, id));
                Some((from_hex(&fp)?, InternetHost { key: from_hex(&s.key)?, announced: s.announced, used: s.used, rendezvous, lan: s.lan }))
            })
            .collect();
        Self { path: path.to_path_buf(), hosts }
    }

    fn save(&self) {
        let stored: BTreeMap<String, Stored> = self
            .hosts
            .iter()
            .map(|(fp, h)| {
                let (server, id) = h.rendezvous.as_ref().map_or_else(Default::default, |(server, id)| (server.clone(), hex(id)));
                (hex(fp), Stored { key: hex(&h.key), announced: h.announced.clone(), used: h.used.clone(), server, id, lan: h.lan.clone() })
            })
            .collect();
        let result = serde_json::to_vec_pretty(&stored).map_err(anyhow::Error::from).and_then(|bytes| write_private(&self.path, &bytes));
        if let Err(e) = result {
            tracing::warn!("save {}: {e:#}", self.path.display());
        }
    }

    /// Records what `host` said on connecting: this Mac's key for it, where it can be reached
    /// (its addresses on its own local network apart), and its LanKVM server and ID there (see
    /// [`clean_rendezvous`]). True if that changed anything.
    pub(crate) fn set_announced(
        &mut self,
        host: Fingerprint,
        key: AccessKey,
        addresses: Vec<String>,
        rendezvous: Option<(String, RendezvousId)>,
    ) -> bool {
        let (lan, announced): (Vec<String>, Vec<String>) = clean_announced(addresses).into_iter().partition(|a| is_lan_address(a));
        if self.hosts.get(&host).is_some_and(|h| h.key == key && h.announced == announced && h.rendezvous == rendezvous && h.lan == lan) {
            return false;
        }
        let entry = self.hosts.entry(host).or_insert_with(|| InternetHost {
            key,
            announced: Vec::new(),
            used: Vec::new(),
            rendezvous: None,
            lan: Vec::new(),
        });
        entry.key = key;
        entry.announced = announced;
        entry.rendezvous = rendezvous;
        entry.lan = lan;
        self.save();
        true
    }

    /// Remembers that `target` (as typed) reached `host` over the internet. True if that
    /// changed anything.
    pub(crate) fn remember_used(&mut self, host: &Fingerprint, target: &str) -> bool {
        let target = target.trim();
        let Some(entry) = self.hosts.get_mut(host) else { return false };
        if target.is_empty() || target.len() > MAX_ADDRESS_LEN || entry.used.first().is_some_and(|u| u == target) {
            return false;
        }
        entry.used.retain(|u| !same_address(u, target));
        entry.used.insert(0, target.to_string());
        entry.used.truncate(MAX_USED);
        self.save();
        true
    }

    /// Forgets `host` (Forget Device): its key goes with it.
    pub(crate) fn remove(&mut self, host: &Fingerprint) {
        if self.hosts.remove(host).is_some() {
            self.save();
        }
    }

    /// Where `host` was last reached over the internet, or else where it said to reach it.
    pub(crate) fn address(&self, host: &Fingerprint) -> Option<String> {
        let entry = self.hosts.get(host)?;
        entry.used.first().or(entry.announced.first()).cloned()
    }

    /// Whether this Mac knows a way to try `host` over the internet: its LanKVM server, or an
    /// address; with `dht` (this Mac looks on the BitTorrent DHT), also its note there, when it
    /// had internet access on last time (it said where it is on its network only then).
    pub(crate) fn reachable(&self, host: &Fingerprint, dht: bool) -> bool {
        self.hosts.get(host).is_some_and(|h| h.rendezvous.is_some() || !h.used.is_empty() || !h.announced.is_empty() || (dht && !h.lan.is_empty()))
    }

    /// How to reach `host` over the internet, if it gave this Mac a key.
    pub(crate) fn way_to(&self, host: &Fingerprint) -> Option<Way> {
        let entry = self.hosts.get(host)?;
        let mut addresses: Vec<String> = Vec::new();
        for address in entry.used.iter().chain(&entry.announced) {
            if !addresses.iter().any(|a| same_address(a, address)) {
                addresses.push(address.clone());
            }
        }
        Some(Way { key: entry.key, addresses, rendezvous: entry.rendezvous.clone(), lan: entry.lan.clone() })
    }

    /// The paired hosts `target` (as typed, resolved to `resolved`) may be, with the keys to
    /// knock with: first those it reached before at that address, then those that announced it,
    /// then those with an address that resolves the same, then those that never said where to
    /// reach them (internet access was off whenever they last connected: the first time over the
    /// internet). Only the right one answers. A host known at other addresses gets no knock sent
    /// here: whoever is at `target` could replay it to that host.
    pub(crate) fn candidates(&self, target: &str, resolved: SocketAddr, trusted: &TrustStore) -> Vec<(Fingerprint, AccessKey)> {
        let typed = normalized(target);
        let resolves_the_same = |a: &String| with_port(a, DEFAULT_PORT).parse::<SocketAddr>().is_ok_and(|addr| addr == resolved);
        let mut matches: Vec<(u8, Fingerprint, AccessKey)> = self
            .hosts
            .iter()
            .filter(|(fp, _)| trusted.contains(fp))
            .filter_map(|(fp, host)| {
                let rank = if host.used.iter().any(|a| normalized(a) == typed) {
                    0
                } else if host.announced.iter().any(|a| normalized(a) == typed) {
                    1
                } else if host.used.iter().chain(&host.announced).any(resolves_the_same) {
                    2
                } else if host.used.is_empty() && host.announced.is_empty() {
                    3
                } else {
                    return None;
                };
                Some((rank, *fp, host.key))
            })
            .collect();
        matches.sort_by_key(|(rank, ..)| *rank);
        matches.into_iter().map(|(_, fp, key)| (fp, key)).collect()
    }

    /// The paired host known to be at `target` (as typed, resolved to `resolved`): one this Mac
    /// reached there, or that announced it, or with an address that resolves the same. A typed
    /// IP without a port also matches a host at that IP on any port (its router may keep another
    /// port open than the one typed). Such a host is reached as Paired Devices does, through its
    /// LanKVM server as well, so its public IP works without the router forwarding a port.
    pub(crate) fn known_at(&self, target: &str, resolved: SocketAddr, trusted: &TrustStore) -> Option<Fingerprint> {
        let typed = normalized(target);
        let ip_only = target.trim().parse::<IpAddr>().is_ok();
        let resolves = |a: &String| with_port(a, DEFAULT_PORT).parse::<SocketAddr>().ok();
        let rank = |host: &InternetHost| {
            let mut known = host.used.iter().chain(&host.announced);
            if host.used.iter().any(|a| normalized(a) == typed) {
                Some(0)
            } else if host.announced.iter().any(|a| normalized(a) == typed) {
                Some(1)
            } else if known.clone().any(|a| resolves(a) == Some(resolved)) {
                Some(2)
            } else if ip_only && known.any(|a| resolves(a).is_some_and(|addr| addr.ip() == resolved.ip())) {
                Some(3)
            } else {
                None
            }
        };
        self.hosts
            .iter()
            .filter(|(fp, _)| trusted.contains(fp))
            .filter_map(|(fp, host)| Some((rank(host)?, *fp)))
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, fp)| fp)
    }

    /// Whether any paired host told this Mac its LanKVM server.
    pub(crate) fn any_server(&self, trusted: &TrustStore) -> bool {
        self.hosts.iter().any(|(fp, host)| host.rendezvous.is_some() && trusted.contains(fp))
    }

    /// Whether any paired host gave this Mac a key.
    pub(crate) fn any_key(&self, trusted: &TrustStore) -> bool {
        self.hosts.keys().any(|fp| trusted.contains(fp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("lankvm-internet-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn fp(n: u8) -> Fingerprint {
        [n; 32]
    }

    fn key(n: u8) -> AccessKey {
        [n.wrapping_add(100); 32]
    }

    fn trusted(dir: &Path, hosts: &[u8]) -> TrustStore {
        let mut store = TrustStore::load(&dir.join("trusted-hosts.txt"));
        for &n in hosts {
            store.add(fp(n), &format!("host {n}")).unwrap();
        }
        store
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_secret_is_private_and_kept() {
        let dir = TempDir::new("secret");
        let secret = load_or_create_secret(&dir.0).unwrap();
        let path = dir.0.join(SECRET_FILE);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(load_or_create_secret(&dir.0).unwrap(), secret);

        // Made readable by others (restored from a backup, say): private again.
        let loosen = |path: &Path| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        };
        loosen(&path);
        assert_eq!(load_or_create_secret(&dir.0).unwrap(), secret);
        assert_eq!(mode(&path), 0o600);

        std::fs::write(&path, b"short").unwrap();
        loosen(&path);
        let fresh = load_or_create_secret(&dir.0).unwrap();
        assert_ne!(fresh, secret, "a damaged secret is replaced");
        assert_eq!(std::fs::read(&path).unwrap(), fresh);
        assert_eq!(mode(&path), 0o600, "in a new file, not the old one's mode");
    }

    #[test]
    fn hosts_round_trip_privately() {
        let dir = TempDir::new("store");
        let path = dir.0.join("internet-hosts.json");
        let mut hosts = InternetHosts::load(&path);
        assert!(hosts.set_announced(fp(1), key(1), vec!["203.0.113.7:47800".into()], None));
        assert!(!hosts.set_announced(fp(1), key(1), vec!["203.0.113.7:47800".into()], None), "nothing new");
        assert!(hosts.remember_used(&fp(1), "home.example.com"));
        assert!(!hosts.remember_used(&fp(2), "elsewhere"), "no key, nothing to remember");
        assert_eq!(mode(&path), 0o600);

        let loaded = InternetHosts::load(&path);
        assert_eq!(loaded.hosts, hosts.hosts);
        assert_eq!(loaded.address(&fp(1)).as_deref(), Some("home.example.com"));
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let entry = &json[hex(&fp(1))];
        assert_eq!(entry["key"], hex(&key(1)));
        assert_eq!(entry["announced"][0], "203.0.113.7:47800");
        assert_eq!(entry["used"][0], "home.example.com");

        hosts.remove(&fp(1));
        assert!(InternetHosts::load(&path).hosts.is_empty());
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(InternetHosts::load(&path).hosts.is_empty());
    }

    #[test]
    fn the_rendezvous_server_is_kept_with_the_key() {
        let dir = TempDir::new("rendezvous");
        let path = dir.0.join("internet-hosts.json");
        let mut hosts = InternetHosts::load(&path);
        let server = Some(("178.156.129.211:3478".to_string(), [0x5a; 16]));
        assert!(hosts.set_announced(fp(1), key(1), Vec::new(), server.clone()));
        assert!(!hosts.set_announced(fp(1), key(1), Vec::new(), server.clone()), "nothing new");
        assert!(hosts.reachable(&fp(1), false), "through the server, with no address");
        assert_eq!(hosts.address(&fp(1)), None);

        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(json[hex(&fp(1))]["server"], "178.156.129.211:3478");
        assert_eq!(json[hex(&fp(1))]["id"], "5a".repeat(16));
        let loaded = InternetHosts::load(&path);
        assert_eq!(loaded.hosts, hosts.hosts);
        let way = loaded.way_to(&fp(1)).unwrap();
        assert_eq!((way.key, way.addresses.len(), way.rendezvous), (key(1), 0, server));

        // Internet access turned off there: no server any more.
        assert!(hosts.set_announced(fp(1), key(1), Vec::new(), None));
        assert!(!hosts.reachable(&fp(1), false));
        assert!(!hosts.reachable(&fp(1), true), "nor through the DHT: its internet access is off");
        // On with no server and nothing forwarded: it said where it is on its own network, so
        // its internet access is on, and its note is on the DHT.
        assert!(hosts.set_announced(fp(1), key(1), vec!["192.168.1.20:47800".into()], None));
        assert!(!hosts.reachable(&fp(1), false));
        assert!(hosts.reachable(&fp(1), true));
        assert!(hosts.set_announced(fp(1), key(1), Vec::new(), None));
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(json[hex(&fp(1))].get("server").is_none(), "{json}");
        assert!(!hosts.reachable(&fp(2), true), "unknown");

        // A file from before rendezvous loads, and an ID of the wrong size is left out.
        std::fs::write(&path, format!(r#"{{ "{}": {{ "key": "{}", "server": "a.example.com", "id": "5a5a" }} }}"#, hex(&fp(3)), hex(&key(3))))
            .unwrap();
        let loaded = InternetHosts::load(&path);
        assert_eq!(loaded.hosts[&fp(3)].rendezvous, None);
        assert_eq!(loaded.hosts[&fp(3)].key, key(3));
    }

    #[test]
    fn only_usable_rendezvous_details_are_kept() {
        assert_eq!(clean_rendezvous(" s.example.com:3478 ".into(), vec![1; 16]), Some(("s.example.com:3478".into(), [1; 16])));
        assert_eq!(clean_rendezvous(String::new(), Vec::new()), None, "internet access or the server off");
        assert_eq!(clean_rendezvous("s.example.com".into(), vec![1; 15]), None);
        assert_eq!(clean_rendezvous("  ".into(), vec![1; 16]), None);
        assert_eq!(clean_rendezvous("a".repeat(MAX_ADDRESS_LEN + 1), vec![1; 16]), None);
    }

    #[test]
    fn a_way_to_a_host_tries_where_it_was_reached_first() {
        let dir = TempDir::new("way");
        let mut hosts = InternetHosts::load(&dir.0.join("internet-hosts.json"));
        hosts.set_announced(fp(1), key(1), vec!["203.0.113.7:47800".into(), "home.example.com".into()], None);
        hosts.remember_used(&fp(1), "Home.example.com:47800");
        hosts.remember_used(&fp(1), "198.51.100.1");
        let way = hosts.way_to(&fp(1)).unwrap();
        assert_eq!(way.addresses, ["198.51.100.1", "Home.example.com:47800", "203.0.113.7:47800"]);
        assert!(hosts.reachable(&fp(1), false));
        assert!(hosts.way_to(&fp(2)).is_none());
    }

    #[test]
    fn a_new_host_counts_as_a_change_even_with_nothing_announced() {
        let dir = TempDir::new("new");
        let mut hosts = InternetHosts::load(&dir.0.join("internet-hosts.json"));
        assert!(hosts.set_announced(fp(1), key(1), Vec::new(), None));
        assert!(!hosts.set_announced(fp(1), key(1), Vec::new(), None));
        assert!(hosts.set_announced(fp(1), key(9), Vec::new(), None), "a new key");
        assert_eq!(hosts.address(&fp(1)), None);
    }

    #[test]
    fn announced_addresses_are_cleaned_up() {
        let dir = TempDir::new("clean");
        let mut hosts = InternetHosts::load(&dir.0.join("internet-hosts.json"));
        let long = "a".repeat(MAX_ADDRESS_LEN + 1);
        let mut many: Vec<String> = vec![" 203.0.113.7:47800 ".into(), "203.0.113.7".into(), String::new(), long];
        many.extend((0..20).map(|i| format!("host{i}.example.com")));
        hosts.set_announced(fp(1), key(1), many, None);
        let announced = &hosts.hosts[&fp(1)].announced;
        assert_eq!(announced.len(), MAX_ANNOUNCED);
        assert_eq!(announced[..2], ["203.0.113.7:47800".to_string(), "host0.example.com".to_string()]);

        // Only the first few are looked at.
        let mut repeated = vec!["203.0.113.7".to_string(); 100_000];
        repeated.push("late.example.com".into());
        assert_eq!(clean_announced(repeated), ["203.0.113.7"]);
    }

    #[test]
    fn used_addresses_are_newest_first_without_repeats() {
        let dir = TempDir::new("used");
        let mut hosts = InternetHosts::load(&dir.0.join("internet-hosts.json"));
        hosts.set_announced(fp(1), key(1), Vec::new(), None);
        for target in ["a.example.com", "b.example.com", "A.example.com:47800", "c.example.com", "d.example.com", "e.example.com"] {
            hosts.remember_used(&fp(1), target);
        }
        assert!(!hosts.remember_used(&fp(1), "e.example.com"), "already the newest");
        assert_eq!(hosts.hosts[&fp(1)].used, ["e.example.com", "d.example.com", "c.example.com", "A.example.com:47800"]);
    }

    #[test]
    fn candidates_prefer_where_a_host_was_reached() {
        let dir = TempDir::new("candidates");
        let mut hosts = InternetHosts::load(&dir.0.join("internet-hosts.json"));
        let resolved = addr("203.0.113.7:47800");
        hosts.set_announced(fp(1), key(1), vec!["198.51.100.1:47800".into()], None);
        hosts.set_announced(fp(2), key(2), vec!["203.0.113.7:47800".into()], None);
        hosts.set_announced(fp(3), key(3), vec!["home.example.com:47800".into()], None);
        hosts.set_announced(fp(4), key(4), Vec::new(), None);
        hosts.remember_used(&fp(4), "Home.Example.com");
        hosts.set_announced(fp(5), key(5), vec!["other.example.com".into()], None);
        // Paired while its internet access was off: it never said where to reach it.
        hosts.set_announced(fp(6), key(6), Vec::new(), None);
        let all = trusted(&dir.0, &[1, 2, 3, 4, 5, 6]);
        assert!(hosts.any_key(&all));

        // Typed as it reached host 4 (case and default port don't matter), announced by host 3,
        // and resolving to what host 2 announced; then the host that may be anywhere.
        let found = hosts.candidates("home.example.com:47800", resolved, &all);
        assert_eq!(found, [(fp(4), key(4)), (fp(3), key(3)), (fp(2), key(2)), (fp(6), key(6))]);
        assert_eq!(hosts.candidates("203.0.113.7", resolved, &all), [(fp(2), key(2)), (fp(6), key(6))]);

        // Nothing matches: only the host that may be anywhere. The others are known elsewhere,
        // and their knocks could be replayed to them from here.
        assert_eq!(hosts.candidates("new.example.com", addr("192.0.2.1:47800"), &all), [(fp(6), key(6))]);
        let known = trusted(&TempDir::new("candidates-known").0, &[1, 2, 3, 4, 5]);
        assert!(hosts.candidates("new.example.com", addr("192.0.2.1:47800"), &known).is_empty());

        // Hosts no longer paired never count.
        let some = trusted(&TempDir::new("candidates-some").0, &[1, 3]);
        assert!(hosts.candidates("203.0.113.7", resolved, &some).is_empty());
        assert_eq!(hosts.candidates("198.51.100.1", resolved, &some), [(fp(1), key(1))]);
        let none = trusted(&TempDir::new("candidates-none").0, &[]);
        assert!(hosts.candidates("203.0.113.7", resolved, &none).is_empty());
        assert!(!hosts.any_key(&none));
    }

    #[test]
    fn local_addresses_are_told_from_internet_ones() {
        for local in ["192.168.1.31", "192.168.1.31:47801", "10.0.0.4", "[fd00::5]:47800", "fe80::1", "127.0.0.1:47800", "Studio.local", "studio.local:47801"] {
            assert!(is_local_address(local), "{local}");
        }
        for internet in ["203.0.113.7", "203.0.113.7:47800", "home.example.com", "[2001:db8::7]:47800", "lankvm:abcd", ""] {
            assert!(!is_local_address(internet), "{internet}");
        }
    }

    #[test]
    fn addresses_on_the_hosts_own_network_are_kept_apart() {
        let dir = TempDir::new("lan");
        let path = dir.0.join("internet-hosts.json");
        let mut hosts = InternetHosts::load(&path);
        let said = || -> Vec<String> {
            ["203.0.113.7:51000", "192.168.1.98:47800", "[fd00::5]:47800", "169.254.3.4:47800", "127.0.0.1:47800"].map(String::from).to_vec()
        };
        assert!(hosts.set_announced(fp(1), key(1), said(), None));
        let way = hosts.way_to(&fp(1)).unwrap();
        // Loopback stays: tests stand it in for the internet.
        assert_eq!(way.addresses, ["203.0.113.7:51000", "127.0.0.1:47800"]);
        assert_eq!(way.lan, ["192.168.1.98:47800", "[fd00::5]:47800", "169.254.3.4:47800"]);
        assert_eq!(hosts.address(&fp(1)).as_deref(), Some("203.0.113.7:51000"), "Paired Devices shows an internet address");
        assert!(!hosts.set_announced(fp(1), key(1), said(), None), "nothing new");
        assert_eq!(InternetHosts::load(&path).way_to(&fp(1)).unwrap().lan, way.lan, "kept");
        // A typed local address is no public IP of the host.
        let all = trusted(&dir.0, &[1]);
        assert_eq!(hosts.known_at("192.168.1.98", addr("192.168.1.98:47800"), &all), None);
    }

    #[test]
    fn a_typed_public_ip_names_the_host_announced_there() {
        let dir = TempDir::new("known-at");
        let mut hosts = InternetHosts::load(&dir.0.join("internet-hosts.json"));
        // Host 1's router keeps port 51000 open for it (as its LanKVM server saw); host 2 is
        // elsewhere; host 3 never said where it is.
        hosts.set_announced(fp(1), key(1), vec!["203.0.113.7:51000".into()], None);
        hosts.set_announced(fp(2), key(2), vec!["198.51.100.1:47800".into()], None);
        hosts.set_announced(fp(3), key(3), Vec::new(), None);
        let all = trusted(&dir.0, &[1, 2, 3]);

        assert_eq!(hosts.known_at("203.0.113.7:51000", addr("203.0.113.7:51000"), &all), Some(fp(1)));
        // The bare IP gets the default port, which the router doesn't keep open: still host 1.
        assert_eq!(hosts.known_at("203.0.113.7", addr("203.0.113.7:47800"), &all), Some(fp(1)));
        // Another port typed explicitly is another place.
        assert_eq!(hosts.known_at("203.0.113.7:6000", addr("203.0.113.7:6000"), &all), None);
        assert_eq!(hosts.known_at("192.0.2.1", addr("192.0.2.1:47800"), &all), None, "nobody announced it");
        assert_eq!(hosts.known_at("198.51.100.1", addr("198.51.100.1:47800"), &all), Some(fp(2)));
        let not_two = trusted(&TempDir::new("known-at-some").0, &[1, 3]);
        assert_eq!(hosts.known_at("198.51.100.1", addr("198.51.100.1:47800"), &not_two), None, "no longer paired");
    }

    #[test]
    fn ports_are_added_when_missing() {
        assert_eq!(with_port("home.example.com", 47801), "home.example.com:47801");
        assert_eq!(with_port(" home.example.com:5000 ", 47801), "home.example.com:5000");
        assert_eq!(with_port("home.example.com:", 47801), "home.example.com:47801");
        assert_eq!(with_port("203.0.113.7", 47800), "203.0.113.7:47800");
        assert_eq!(with_port("203.0.113.7:9", 47800), "203.0.113.7:9");
        assert_eq!(with_port("2001:db8::7", 47800), "[2001:db8::7]:47800");
        assert_eq!(with_port("[2001:db8::7]", 47800), "[2001:db8::7]:47800");
        assert_eq!(with_port("[2001:db8:0::7]:9", 47800), "[2001:db8::7]:9");
        assert!(same_address("Home.Example.com", "home.example.com:47800"));
        assert!(!same_address("home.example.com", "home.example.com:47801"));
    }

    #[test]
    fn announced_addresses_combine_the_setting_and_the_router() {
        let external = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 47801);
        let mapped = Some(MapState::Mapped { external });
        assert!(announced("", None, 47800).is_empty());
        assert_eq!(announced("", mapped, 47800), ["203.0.113.7:47801"]);
        // A name without a port gets the one the router forwards.
        assert_eq!(announced("home.example.com", mapped, 47800), ["home.example.com:47801", "203.0.113.7:47801"]);
        assert_eq!(announced("home.example.com", Some(MapState::Requesting), 47800), ["home.example.com:47800"]);
        assert_eq!(announced("203.0.113.7", mapped, 47800), ["203.0.113.7:47801"], "no repeats");
        let failed = Some(MapState::Failed(MapProblem::NoResponse));
        assert_eq!(announced("203.0.113.9:6000", failed, 47800), ["203.0.113.9:6000"]);
        let public = Some(MapState::PublicAddress { external: SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 4), 47800) });
        assert_eq!(announced("", public, 47800), ["198.51.100.4:47800"]);
    }

    #[test]
    fn the_view_names_the_state() {
        let view = |enabled, map| build_view(enabled, map, 47800, String::new(), Some("192.168.1.20".into()), 3);
        let external = SocketAddrV4::new(Ipv4Addr::new(203, 0, 113, 7), 47800);

        let off = view(false, Some(MapState::Mapped { external }));
        assert_eq!((off.state, off.problem, off.external_address.as_deref()), ("off", None, None));
        assert!(off.announced.is_empty());
        assert_eq!(view(true, None).state, "requesting");
        assert_eq!(view(true, Some(MapState::Requesting)).state, "requesting");

        let mapped = view(true, Some(MapState::Mapped { external }));
        assert_eq!((mapped.state, mapped.external_address.as_deref()), ("mapped", Some("203.0.113.7:47800")));
        assert_eq!(mapped.announced, ["203.0.113.7:47800"]);
        let public = view(true, Some(MapState::PublicAddress { external }));
        assert_eq!((public.state, public.external_address.as_deref()), ("public", Some("203.0.113.7:47800")));

        let router = Ipv4Addr::new(192, 168, 0, 12);
        let double = view(true, Some(MapState::Failed(MapProblem::DoubleNat { router_external: Some(router) })));
        assert_eq!((double.state, double.problem, double.router_address.as_deref()), ("problem", Some("doubleNat"), Some("192.168.0.12")));
        let cgnat = view(true, Some(MapState::Failed(MapProblem::Cgnat { router_external: Ipv4Addr::new(100, 64, 3, 4) })));
        assert_eq!((cgnat.problem, cgnat.router_address.as_deref()), (Some("cgnat"), Some("100.64.3.4")));
        let other = view(true, Some(MapState::Failed(MapProblem::Other(-65537))));
        assert_eq!((other.problem, other.router_address), (Some("other"), None));

        let json = serde_json::to_value(view(true, Some(MapState::Failed(MapProblem::NoResponse)))).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "enabled": true, "state": "problem", "problem": "noResponse", "externalAddress": null, "routerAddress": null,
                "localAddress": "192.168.1.20", "port": 47800, "publicAddress": "", "announced": [], "ignored": 3,
                "server": { "address": "", "state": "off", "observed": null },
                "dht": { "enabled": false, "state": "off", "observed": null, "symmetric": false, "nodes": 0 }
            })
        );
    }
}
