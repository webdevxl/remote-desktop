//! Meeting paired Macs through the BitTorrent DHT, with no LanKVM server: the core's part (the
//! DHT client and the mailbox format are in `transport::dht`).
//!
//! As a host, while internet access and the BitTorrent DHT option are on: for each paired viewer,
//! keeps a note in their mailbox saying where this Mac is and which DHT nodes it watches, asks
//! those nodes every couple of seconds for a request from that viewer, and when one comes,
//! punches toward the viewer (which opens this Mac's router for it) and answers in its note.
//!
//! As a viewer: finds the host's note (one DHT lookup), leaves a request at the nodes the host
//! watches, punches toward the host (which opens this Mac's router for its packets), and connects
//! as soon as the host punches back, or its note answers, or right away where the host can be
//! reached without punching (its router forwards the port, or it is on this Mac's network).
//!
//! The DHT client runs on the QUIC socket, starts the first time it is needed, and remembers the
//! nodes that answered for the next start (`dht-nodes.txt`), so the public bootstrap nodes are
//! needed only the first time.
//!
//! Timers that must hold across sleep (when to look for nodes again, when to rewrite a note) go
//! by the wall clock: the monotonic one stops while the Mac sleeps, and a note left that long
//! tells viewers the host is gone.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use quinn::Connection;
use serde::Serialize;
use tokio::runtime::Handle;
use tokio::task::JoinSet;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until, timeout};
use transport::dht::mailbox::{self, RequestNonce, epoch_at, epochs_around, next_seq, now_ms, request_nonce};
use transport::dht::{Config, DEFAULT_BOOTSTRAP, Dht, HostNote, Mailbox, MutableItem, Side, Slot, ViewerNote};
use transport::endpoint::{Network, peer_fingerprint};
use transport::identity::{Fingerprint, short_hex};
use transport::knock::{AccessKey, now_unix};
use transport::rendezvous::{HOST_PUNCHES, punch};

use crate::{Event, EventSink};

/// How often the host asks the nodes it watches for a viewer's request: the most a viewer waits
/// for the host to notice it, on top of the DHT's own delays.
const POLL_EVERY: Duration = Duration::from_secs(2);
/// Nodes asked each time, taking turns among those the host's note names: the viewer stores its
/// request on all of them (and on those its own lookup finds, which differ a little), so a few
/// do, and the host sends a couple of small packets a second per paired Mac.
const POLL_NODES: usize = 4;
/// How often the host looks for the nodes closest to its mailbox again: nodes come and go.
const SEARCH_EVERY_SECS: u64 = 10 * 60;
/// After a search found nothing (the DHT out of reach, the network not up yet), the host tries
/// again this soon, twice as long each time, up to the most.
const SEARCH_RETRY_SECS: u64 = 10;
const SEARCH_RETRY_MAX_SECS: u64 = 120;
/// How often the host rewrites its note when nothing changed. Nodes keep items for hours, and
/// viewers take a note older than [`NOTE_FRESH_SECS`] for a host that is gone (allowing for the
/// two Macs' clocks to differ by [`mailbox::SKEW_SECS`]).
const PUBLISH_EVERY_SECS: u64 = 15 * 60;
const NOTE_FRESH_SECS: u64 = PUBLISH_EVERY_SECS + mailbox::SKEW_SECS + 5 * 60;
/// After a note couldn't be stored anywhere, the host tries again this soon.
const PUBLISH_RETRY_SECS: u64 = 10;
/// Requests the host remembers having answered, so a request read again isn't answered again.
const ANSWERED_TTL: Duration = Duration::from_secs(30 * 60);
/// How long since a DHT node last answered before this Mac counts as off the DHT.
const OFF_THE_DHT: Duration = Duration::from_secs(60);
/// How long a viewer waits for the host once its request is out: the host's poll, its answer,
/// and a handshake. An attempt the host's punch or answer started gets its own time on top.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the viewer punches toward the host while it waits (each opens its router for the
/// host's packets a little longer), and asks the host's nodes for its answer.
const VIEWER_PUNCH_EVERY: Duration = Duration::from_millis(250);
const ANSWER_POLL_EVERY: Duration = Duration::from_millis(500);
/// How long one connection attempt may take.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(6);
/// Addresses the host's punches and answers may start attempts at, at most.
const ANSWERED_ATTEMPTS: usize = 6;
/// Nodes remembered for the next start.
const SAVED_NODES: usize = 200;
const NODES_FILE: &str = "dht-nodes.txt";

/// This Mac's dealings with the BitTorrent DHT, as host and as viewer. One per core.
pub(crate) struct DhtRendezvous {
    network: Network,
    rt: Handle,
    events: EventSink,
    nodes_path: PathBuf,
    /// Where the client joins the DHT; None when the DHT is off for this run (tests that don't
    /// bring a test DHT of their own: none may reach the real one).
    bootstrap: Option<Vec<String>>,
    /// The public DHT, through the usual bootstrap nodes: nodes from an earlier run are kept and
    /// used.
    public: bool,
    /// Every bootstrap node is at a local address (a test DHT): so may the nodes they name be.
    local: bool,
    /// The user's setting.
    enabled: AtomicBool,
    client: Mutex<Option<Dht>>,
    host: Mutex<HostSide>,
    /// Counts changes to what the host does, so the tasks of an earlier one stop.
    generation: AtomicU64,
    /// This Mac's addresses for its notes, from the host's internet access.
    addresses: Mutex<Option<Addresses>>,
    /// Viewers' mailboxes, by epoch, that have this Mac's current note, and when it was stored
    /// (Unix seconds).
    listed: Mutex<HashMap<(Fingerprint, u64), u64>>,
    /// When the host side last (re)started (wall clock): the DHT counts as out of reach only once
    /// it had time to answer since.
    restarted: Mutex<std::time::SystemTime>,
    /// The view the UI saw last, to tell it when that changes.
    shown: Mutex<Option<(&'static str, Option<String>, bool)>>,
    /// Tests: as a viewer, connects only once the host has punched or answered, never straight
    /// to the addresses in its note (on loopback those always work, with nothing to punch).
    wait_for_answer: AtomicBool,
    /// Tests: as a viewer, reaches paired hosts through the DHT and no other way.
    only: AtomicBool,
}

type Addresses = Box<dyn Fn() -> Vec<SocketAddr> + Send + Sync>;

#[derive(Default)]
struct HostSide {
    /// The viewers to keep notes for, with their keys: empty while internet access is off.
    viewers: Vec<(Fingerprint, AccessKey)>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    /// LanKVM is quitting.
    closed: bool,
}

/// The BitTorrent DHT as the UI sees it (part of `InternetStatus`).
#[derive(Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DhtView {
    /// The setting.
    pub enabled: bool,
    /// "off", "idle" (no paired Mac to keep a note for), "joining" (finding DHT nodes and
    /// placing its notes), "listed" (paired Macs can find it) or "unreachable" (no DHT node
    /// answers).
    pub state: &'static str,
    /// Where DHT nodes see this Mac.
    pub observed: Option<String>,
    /// This Mac's router gives each destination a port of its own: punching through it fails.
    pub symmetric: bool,
    /// DHT nodes that answered lately.
    pub nodes: usize,
}

impl Default for DhtView {
    fn default() -> Self {
        Self { enabled: false, state: "off", observed: None, symmetric: false, nodes: 0 }
    }
}

/// The host isn't on the DHT (no note there, or an old one): news only when it should be.
#[derive(Debug)]
pub(crate) struct NotListed(String);

impl std::fmt::Display for NotListed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotListed {}

impl DhtRendezvous {
    pub(crate) fn start(network: Network, rt: Handle, events: EventSink, data_dir: &Path, bootstrap: Option<Vec<String>>, enabled: bool) -> Arc<Self> {
        let public = bootstrap.as_ref().is_some_and(|b| b.iter().map(String::as_str).eq(DEFAULT_BOOTSTRAP.iter().copied()));
        let local = bootstrap.as_ref().is_some_and(|b| {
            !b.is_empty() && b.iter().all(|n| n.parse::<SocketAddr>().is_ok_and(|a| !transport::dht::client::routable(a)))
        });
        if bootstrap.is_some() && !public {
            tracing::warn!(?bootstrap, "the BitTorrent DHT is joined through other nodes for this run (LANKVM_DHT_BOOTSTRAP)");
        }
        Arc::new(Self {
            network,
            rt,
            events,
            nodes_path: data_dir.join(NODES_FILE),
            bootstrap,
            public,
            local,
            enabled: AtomicBool::new(enabled),
            client: Mutex::new(None),
            host: Mutex::default(),
            generation: AtomicU64::new(0),
            addresses: Mutex::new(None),
            listed: Mutex::default(),
            restarted: Mutex::new(std::time::SystemTime::now()),
            shown: Mutex::new(None),
            wait_for_answer: AtomicBool::new(false),
            only: AtomicBool::new(false),
        })
    }

    /// Whether this Mac uses the DHT at all (the setting, and not off for this run).
    pub(crate) fn available(&self) -> bool {
        self.bootstrap.is_some() && self.enabled.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    pub(crate) fn set_wait_for_answer(&self, on: bool) {
        self.wait_for_answer.store(on, Ordering::SeqCst);
    }

    #[doc(hidden)]
    pub(crate) fn set_only(&self, on: bool) {
        self.only.store(on, Ordering::SeqCst);
    }

    /// Paired hosts are reached through the DHT and no other way (tests, and `probe --only-dht`).
    pub(crate) fn only(&self) -> bool {
        self.only.load(Ordering::SeqCst)
    }

    /// Where this Mac's addresses come from, for its notes.
    pub(crate) fn set_addresses(&self, addresses: impl Fn() -> Vec<SocketAddr> + Send + Sync + 'static) {
        *self.addresses.lock().unwrap() = Some(Box::new(addresses));
    }

    /// The user's setting: whether this Mac uses the BitTorrent DHT, as host and as viewer.
    pub(crate) fn set_enabled(self: &Arc<Self>, on: bool) {
        if self.enabled.swap(on, Ordering::SeqCst) != on {
            tracing::info!(on, "BitTorrent DHT");
            let mut host = self.host.lock().unwrap();
            self.restart_host(&mut host);
        }
    }

    /// The viewers this Mac keeps notes for, with their keys: every paired viewer while internet
    /// access is on, none while it is off.
    pub(crate) fn set_viewers(self: &Arc<Self>, viewers: Vec<(Fingerprint, AccessKey)>) {
        let mut host = self.host.lock().unwrap();
        if host.viewers == viewers {
            return;
        }
        host.viewers = viewers;
        self.restart_host(&mut host);
    }

    fn restart_host(self: &Arc<Self>, host: &mut HostSide) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        host.tasks.drain(..).for_each(|task| task.abort());
        self.listed.lock().unwrap().clear();
        *self.restarted.lock().unwrap() = std::time::SystemTime::now();
        if !host.closed && !host.viewers.is_empty() && self.available()
            && let Some(dht) = self.client()
        {
            for &(viewer, key) in &host.viewers {
                host.tasks.push(self.rt.spawn(watch_viewer(Arc::downgrade(self), generation, dht.clone(), viewer, key)));
            }
        }
        (self.events)(Event::HostChanged);
    }

    /// When LanKVM quits: stops, and remembers the nodes that answered for the next start.
    pub(crate) fn shutdown(&self) {
        {
            let mut host = self.host.lock().unwrap();
            host.closed = true;
            self.generation.fetch_add(1, Ordering::SeqCst);
            host.tasks.drain(..).for_each(|task| task.abort());
        }
        self.save_nodes();
        self.network.gate.stop_dht();
    }

    pub(crate) fn view(&self, internet_access: bool) -> DhtView {
        let enabled = self.enabled.load(Ordering::SeqCst);
        let mut view = DhtView { enabled, ..DhtView::default() };
        if !enabled || !internet_access || self.bootstrap.is_none() {
            return view;
        }
        let viewers: Vec<Fingerprint> = self.host.lock().unwrap().viewers.iter().map(|(fp, _)| *fp).collect();
        if viewers.is_empty() {
            view.state = "idle";
            return view;
        }
        let client = self.client.lock().unwrap().clone();
        let Some(client) = client else {
            view.state = "joining";
            return view;
        };
        let observed = client.observed();
        let stats = client.stats();
        view.observed = observed.addr.map(|a| a.to_string());
        view.symmetric = observed.varies;
        view.nodes = stats.alive;
        let answering = client.since_answer().is_some_and(|ago| ago < OFF_THE_DHT);
        let settled = self.restarted.lock().unwrap().elapsed().is_ok_and(|ago| ago >= OFF_THE_DHT);
        let now = now_unix();
        // Listed in the epoch viewers are in now (a note for the next one may still be on its way).
        let listed = {
            let listed = self.listed.lock().unwrap();
            viewers.iter().all(|fp| listed.get(&(*fp, epoch_at(now))).is_some_and(|at| now.saturating_sub(*at) < NOTE_FRESH_SECS))
        };
        view.state = if !answering && settled && stats.queries >= 20 {
            "unreachable"
        } else if listed && answering {
            "listed"
        } else {
            "joining"
        };
        view
    }

    /// Tells the UI when what it shows about the DHT has changed.
    fn notify(&self) {
        let view = self.view(true);
        let now = (view.state, view.observed, view.symmetric);
        let mut shown = self.shown.lock().unwrap();
        if shown.as_ref() != Some(&now) {
            *shown = Some(now);
            drop(shown);
            (self.events)(Event::HostChanged);
        }
    }

    /// The DHT client, started the first time it is needed. None when the DHT is off.
    fn client(&self) -> Option<Dht> {
        let bootstrap = self.bootstrap.as_ref()?;
        let mut client = self.client.lock().unwrap();
        if client.is_none() {
            let inbound = self.network.gate.take_dht_receiver();
            // Nodes from a run on the public DHT are no use on another (a test's), nor the other
            // way round.
            let seeds = if self.public { load_nodes(&self.nodes_path) } else { Vec::new() };
            tracing::info!(seeds = seeds.len(), "joining the BitTorrent DHT");
            let config = Config { bootstrap: bootstrap.clone(), seeds, allow_local: self.local };
            *client = Some(Dht::start(self.rt.clone(), Arc::new(self.network.clone()), inbound, config));
        }
        client.clone()
    }

    fn save_nodes(&self) {
        if !self.public {
            return;
        }
        let Some(client) = self.client.lock().unwrap().clone() else { return };
        let nodes = client.alive_nodes(SAVED_NODES);
        if nodes.is_empty() {
            return;
        }
        let text: String = nodes.iter().map(|n| format!("{n}\n")).collect();
        if let Err(e) = std::fs::write(&self.nodes_path, text) {
            tracing::debug!("save {}: {e}", self.nodes_path.display());
        }
    }

    /// This Mac's addresses for its note: where DHT nodes see it (the mapping its router keeps
    /// for the QUIC socket), its IPv6 address, then those of its internet access (the router's
    /// forwarded port, the public address set) and its own network.
    fn host_addresses(&self, dht: &Dht) -> Vec<SocketAddr> {
        let mut addrs: Vec<SocketAddr> = dht.observed().addr.into_iter().collect();
        addrs.extend(self.v6_address());
        if let Some(addresses) = &*self.addresses.lock().unwrap() {
            addrs.extend(addresses());
        }
        dedup(addrs)
    }

    /// Where the host can punch toward this Mac: where DHT nodes see it, its IPv6 address, and
    /// (`lan_ok`) its addresses on its own network, for a host there too.
    fn viewer_addresses(&self, dht: &Dht, lan_ok: bool) -> Vec<SocketAddr> {
        let mut addrs: Vec<SocketAddr> = dht.observed().addr.into_iter().collect();
        addrs.extend(self.v6_address());
        let port = self.network.endpoint.local_addr().map_or(0, |a| a.port());
        // Not while loopback stands in for the internet: the host is on this Mac.
        if lan_ok && !self.network.gate.loopback_is_internet() {
            addrs.extend(crate::local_addresses().iter().filter_map(|ip| format!("{ip}:{port}").parse::<SocketAddr>().ok()));
        }
        dedup(addrs)
    }

    /// This Mac's IPv6 address on the internet and the endpoint's port, when the endpoint takes
    /// IPv6 and the Mac has one. Over IPv6 there is no router address to get around: punching
    /// only opens the firewall in front, which works where IPv4 needs a relay (carrier-grade
    /// NAT, routers that change ports). Not while loopback stands in for the internet (tests).
    fn v6_address(&self) -> Option<SocketAddr> {
        let local = self.network.endpoint.local_addr().ok().filter(SocketAddr::is_ipv6)?;
        if self.network.gate.loopback_is_internet() {
            return None;
        }
        crate::internet_v6().map(|ip| SocketAddr::new(ip.into(), local.port()))
    }

    /// Punches toward a viewer at each of `addrs` on the [`HOST_PUNCHES`] schedule: this Mac's
    /// router then takes the viewer's packets for answers and lets them in. Each punch carries
    /// `nonce`, which only this pair can make, so the viewer knows it's this Mac's and connects
    /// where it came from.
    fn punch_toward(&self, addrs: &[SocketAddr], nonce: RequestNonce) {
        let addrs: Vec<SocketAddr> = addrs.iter().copied().filter(|a| self.worth_punching(*a)).take(mailbox::MAX_ADDRS).collect();
        let network = self.network.clone();
        self.rt.spawn(async move {
            let start = Instant::now();
            for at in HOST_PUNCHES {
                sleep_until(start + at).await;
                for &addr in &addrs {
                    let _ = network.send_raw(addr, &punch(&nonce)).await;
                }
            }
        });
    }

    /// An address worth sending to: one on the internet, or on this Mac's network (elsewhere a
    /// private address is someone else's, or nobody's).
    fn worth_punching(&self, addr: SocketAddr) -> bool {
        addr.port() != 0 && !addr.ip().is_unspecified() && (self.network.gate.is_internet(addr) || crate::on_this_network(addr.ip()))
    }

    // MARK: Viewer

    /// Connects to the paired host `host`, named `name` within a sentence, that gave this Mac
    /// `key`, through the DHT. `lan_ok`: at an address on the local network too (the way that
    /// note or punches may give, when both Macs are at home); otherwise only over the internet.
    pub(crate) async fn connect(self: &Arc<Self>, name: &str, key: AccessKey, host: Fingerprint, lan_ok: bool) -> Result<Connection> {
        let Some(dht) = self.available().then(|| self.client()).flatten() else {
            bail!("This Mac doesn't use the BitTorrent DHT.");
        };
        let mailbox = Mailbox::new(&key);
        let now = now_unix();
        let epoch = epoch_at(now);
        let (host_slot, viewer_slot) = (mailbox.slot(Side::Host, epoch), mailbox.slot(Side::Viewer, epoch));
        let started = Instant::now();
        // Where requests are stored besides the nodes the host's note names: those closest to
        // the request's place, found meanwhile (the note may be an older one's, from before the
        // host restarted, naming nodes it no longer watches).
        let mut own_nodes = JoinSet::new();
        {
            let (dht, key) = (dht.clone(), viewer_slot.key);
            own_nodes.spawn(async move { dht.get_mutable(&key, &[]).await.1.into_iter().map(|f| f.node.addr).collect::<Vec<_>>() });
        }
        let fresh = |item: &MutableItem| mailbox.read_host(&host_slot, item).is_some_and(|n| n.at + NOTE_FRESH_SECS >= now);
        let (item, storing) = dht.find_mutable(&host_slot.key, &[], &fresh).await;
        self.save_nodes();
        let (Some(item), Some(note)) = (item.clone(), item.as_ref().and_then(|i| mailbox.read_host(&host_slot, i))) else {
            if dht.stats().alive == 0 {
                return Err(NotListed("No BitTorrent DHT node answered: UDP may be blocked on this network.".into()).into());
            }
            return Err(NotListed(format!(
                "{name} isn't on the BitTorrent DHT: LanKVM isn't running there, or its internet access (or the DHT option) is off."
            ))
            .into());
        };
        if note.at + NOTE_FRESH_SECS < now {
            return Err(NotListed(format!(
                "{name} hasn't been on the BitTorrent DHT for {}: LanKVM isn't running there, or its internet access is off.",
                ago(now.saturating_sub(note.at))
            ))
            .into());
        }
        tracing::info!(ms = started.elapsed().as_millis() as u64, addrs = ?note.addrs, "found {name}'s note on the BitTorrent DHT");
        // The first note found will do to start with, but may be an older one's (the host
        // restarted, kept elsewhere): the whole lookup goes on meanwhile, for the newest, and
        // the nodes that have it.
        let mut newest = JoinSet::new();
        {
            let (dht, key) = (dht.clone(), host_slot.key);
            newest.spawn(async move {
                let (item, storing) = dht.get_mutable(&key, &[]).await;
                (item, storing.into_iter().map(|f| f.node.addr).collect::<Vec<_>>())
            });
        }

        let nonce = request_nonce();
        let mut punches = self.network.gate.watch_punches(mailbox.punch_nonce(&nonce));
        let request = ViewerNote { at: now_unix(), nonce, addrs: self.viewer_addresses(&dht, lan_ok) };
        let request = mailbox.viewer_item(&viewer_slot, next_seq(None, now_ms()), &request);
        let mut wait = Waiting {
            dht,
            name,
            key,
            host,
            lan_ok,
            request,
            nonce,
            stored_at: HashSet::new(),
            stores: JoinSet::new(),
            stored: 0,
            host_addrs: Vec::new(),
            answer_nodes: storing.iter().map(|f| f.node.addr).collect(),
            seq: item.seq,
            attempts: JoinSet::new(),
            tries: Tries::default(),
            deadline: Instant::now() + ANSWER_TIMEOUT,
        };
        wait.note(self, &note, !self.wait_for_answer.load(Ordering::SeqCst));
        let result = self.wait_for_host(&mut wait, &mailbox, &host_slot, &mut punches, &mut own_nodes, &mut newest).await;
        if result.is_ok() {
            tracing::info!(ms = started.elapsed().as_millis() as u64, "reached {name} through the BitTorrent DHT");
        }
        result
    }

    /// Waits for the host to answer the request, punching toward it meanwhile, and connects.
    async fn wait_for_host(
        &self,
        wait: &mut Waiting<'_>,
        mailbox: &Mailbox,
        host: &Slot,
        punches: &mut transport::gate::PunchWatch,
        own_nodes: &mut JoinSet<Vec<SocketAddr>>,
        newest: &mut JoinSet<(Option<MutableItem>, Vec<SocketAddr>)>,
    ) -> Result<Connection> {
        let name = wait.name;
        let mut polls = JoinSet::new();
        let mut answered = false;
        let mut punch_tick = interval(VIEWER_PUNCH_EVERY);
        let mut poll_tick = interval(ANSWER_POLL_EVERY);
        poll_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_error = None;
        loop {
            tokio::select! {
                _ = sleep_until(wait.deadline) => break,
                Some(from) = punches.next() => {
                    // Only the host can make these punches: it's there, and its router is open
                    // for this Mac now. Attempts made before were likely dropped on the way. From
                    // an IP its notes name only (its router may pick another port): someone who
                    // saw a punch on the way can send it again, from anywhere.
                    if wait.usable(self, from) && wait.host_addrs.iter().any(|a| a.ip() == from.ip()) {
                        tracing::info!(%from, "{name} punched toward this Mac: connecting there");
                        wait.answered_attempt(self, from);
                    }
                }
                _ = punch_tick.tick() => {
                    for &addr in wait.host_addrs.iter().filter(|a| self.network.gate.is_internet(**a)) {
                        let _ = self.network.send_raw(addr, &punch(&wait.nonce)).await;
                    }
                }
                _ = poll_tick.tick(), if polls.is_empty() && !answered => {
                    let (dht, nodes, key, seq) = (wait.dht.clone(), wait.answer_nodes.clone(), host.key, wait.seq);
                    polls.spawn(async move { dht.poll_mutable(&nodes, &key, &[], Some(seq)).await });
                }
                Some(polled) = polls.join_next() => {
                    if let Ok(Some(item)) = polled {
                        answered |= self.newer_note(wait, mailbox, host, &item);
                    }
                }
                Some(found) = newest.join_next() => {
                    let Ok((item, nodes)) = found else { continue };
                    for node in nodes {
                        if !wait.answer_nodes.contains(&node) {
                            wait.answer_nodes.push(node);
                        }
                    }
                    if let Some(item) = item.filter(|i| i.seq > wait.seq) {
                        answered |= self.newer_note(wait, mailbox, host, &item);
                    }
                }
                Some(nodes) = own_nodes.join_next() => {
                    if let Ok(nodes) = nodes {
                        wait.store(&nodes);
                    }
                }
                Some(done) = wait.stores.join_next() => {
                    wait.stored += done.unwrap_or(0);
                }
                Some(result) = wait.attempts.join_next() => match result {
                    Ok((addr, Ok(conn))) => {
                        if peer_fingerprint(&conn) == Some(wait.host) {
                            return Ok(conn);
                        }
                        // Another Mac at that address (a private one, on another network than
                        // the host's): not the way, but the others may still be.
                        conn.close(0u32.into(), b"");
                        last_error = Some(anyhow!("{addr}: the Mac there isn't {name}"));
                    }
                    Ok((_, Err(e))) => last_error = Some(e),
                    Err(_) => {}
                },
            }
        }
        if answered {
            let why = last_error.map(|e| format!(" ({e:#})")).unwrap_or_default();
            bail!("{name} answered through the BitTorrent DHT, but no connection got through{why}.");
        }
        if wait.stored == 0 && wait.stores.is_empty() {
            bail!("Couldn't leave {name} a request on the BitTorrent DHT.");
        }
        Err(anyhow!("{name} didn't answer through the BitTorrent DHT. LanKVM may not be running there, or its internet access is off."))
    }
}

impl DhtRendezvous {
    /// Takes in a note of the host's newer than the one the viewer started from: the answer to
    /// its request (true), or a note from after the host started again or moved, which may name
    /// other nodes and addresses (the request goes there too).
    fn newer_note(&self, wait: &mut Waiting<'_>, mailbox: &Mailbox, host: &Slot, item: &MutableItem) -> bool {
        wait.seq = wait.seq.max(item.seq);
        let Some(newer) = mailbox.read_host(host, item) else { return false };
        let name = wait.name;
        if newer.answered == Some(wait.nonce) {
            tracing::info!(addrs = ?newer.addrs, "{name} answered through the BitTorrent DHT");
            wait.note(self, &newer, false);
            let usable: Vec<SocketAddr> = newer.addrs.iter().copied().filter(|a| wait.usable(self, *a)).collect();
            for addr in usable {
                wait.answered_attempt(self, addr);
            }
            return true;
        }
        tracing::info!(addrs = ?newer.addrs, "{name} has a newer note on the BitTorrent DHT");
        wait.note(self, &newer, !self.wait_for_answer.load(Ordering::SeqCst));
        false
    }
}

/// A viewer's request to a host on the DHT, as it waits for the answer.
struct Waiting<'a> {
    dht: Dht,
    name: &'a str,
    key: AccessKey,
    /// The host it's for: a connection to another Mac doesn't count.
    host: Fingerprint,
    lan_ok: bool,
    request: MutableItem,
    nonce: RequestNonce,
    /// The nodes the request went to, and stores still going.
    stored_at: HashSet<SocketAddr>,
    stores: JoinSet<usize>,
    /// The nodes that took the request.
    stored: usize,
    /// Where the host is, as its notes say.
    host_addrs: Vec<SocketAddr>,
    /// Where its answer appears: the nodes its notes are on.
    answer_nodes: Vec<SocketAddr>,
    /// The newest note seen.
    seq: i64,
    attempts: JoinSet<(SocketAddr, Result<Connection>)>,
    tries: Tries,
    deadline: Instant,
}

/// Which addresses a viewer has tried: each once as soon as a note names it, and once more after
/// the host punched or answered (a few at most). An address tried too early was likely dropped
/// by the host's router, and QUIC's next resend may come after the attempt gives up.
#[derive(Default)]
struct Tries {
    early: HashSet<SocketAddr>,
    answered: HashSet<SocketAddr>,
}

impl Tries {
    fn early(&mut self, addr: SocketAddr) -> bool {
        self.early.insert(addr)
    }

    fn answered(&mut self, addr: SocketAddr) -> bool {
        self.answered.len() < ANSWERED_ATTEMPTS && self.answered.insert(addr)
    }
}

impl Waiting<'_> {
    /// Takes in a note of the host's: the request goes to the nodes it watches, its answer is
    /// looked for where the note is kept, and (with `connect`) its addresses are tried.
    fn note(&mut self, dht: &DhtRendezvous, note: &HostNote, connect: bool) {
        self.store(&note.watch);
        for &node in &note.stored {
            if !self.answer_nodes.contains(&node) {
                self.answer_nodes.push(node);
            }
        }
        let usable: Vec<SocketAddr> = note.addrs.iter().copied().filter(|a| self.usable(dht, *a)).collect();
        for addr in usable {
            if !self.host_addrs.contains(&addr) {
                self.host_addrs.push(addr);
            }
            if connect && self.tries.early(addr) {
                self.attempts.spawn(attempt(dht.network.clone(), addr, self.key));
            }
        }
    }

    /// Tries `addr` again now that the host has punched toward this Mac or answered (a few
    /// addresses at most), with time to finish whatever the deadline.
    fn answered_attempt(&mut self, dht: &DhtRendezvous, addr: SocketAddr) {
        if !self.tries.answered(addr) {
            return;
        }
        self.attempts.spawn(attempt(dht.network.clone(), addr, self.key));
        self.deadline = self.deadline.max(Instant::now() + ATTEMPT_TIMEOUT);
    }

    /// An address the host may be reached at: on the internet, or (where that's allowed) on this
    /// Mac's network.
    fn usable(&self, dht: &DhtRendezvous, addr: SocketAddr) -> bool {
        dht.worth_punching(addr) && (self.lan_ok || dht.network.gate.is_internet(addr))
    }

    /// Stores the request on those of `nodes` it isn't on yet.
    fn store(&mut self, nodes: &[SocketAddr]) {
        let new: Vec<SocketAddr> = nodes.iter().copied().filter(|n| self.stored_at.insert(*n)).collect();
        if !new.is_empty() {
            let (dht, request) = (self.dht.clone(), self.request.clone());
            self.stores.spawn(async move { dht.store_at(&request, &new).await });
        }
    }
}

/// Connects to the host at `addr`: with a knock over the internet, as on any local network here.
async fn attempt(network: Network, addr: SocketAddr, key: AccessKey) -> (SocketAddr, Result<Connection>) {
    let connecting = if network.gate.is_internet(addr) {
        network.endpoint.connect_with(network.internet_client_config(key), addr, "lankvm")
    } else {
        network.endpoint.connect(addr, "lankvm")
    };
    let connecting = match connecting {
        Ok(connecting) => connecting,
        Err(e) => return (addr, Err(anyhow!("{addr}: {e}"))),
    };
    let result = match timeout(ATTEMPT_TIMEOUT, connecting).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => Err(anyhow!("{addr}: {e}")),
        Err(_) => Err(anyhow!("{addr}: no answer")),
    };
    (addr, result)
}

// MARK: Host

/// One epoch's mailbox for one viewer, as the host keeps it.
struct EpochBox {
    host: Slot,
    viewer: Slot,
    /// The nodes closest to the viewer's slot (where requests come), and to the host's (where
    /// its note goes), from the last search.
    watch: Vec<SocketAddr>,
    stored: Vec<SocketAddr>,
    /// When to look for those nodes again (Unix seconds), and searches in a row that found none.
    next_search: u64,
    failed_searches: u32,
    /// What the note said when last stored, and when (Unix seconds).
    published: Option<(u64, Vec<SocketAddr>, Option<RequestNonce>, Vec<SocketAddr>)>,
    /// When a note that couldn't be stored may be tried again.
    next_publish: u64,
    host_seq: Option<i64>,
    /// The newest request read.
    request_seq: Option<i64>,
    /// Where the next poll starts among the nodes watched.
    turn: usize,
}

/// The host's work for one viewer: keeps its note in their mailbox (in each epoch a viewer may
/// be in, given the clocks), watches for its requests, and answers them.
async fn watch_viewer(this: Weak<DhtRendezvous>, generation: u64, dht: Dht, viewer: Fingerprint, key: AccessKey) {
    let mailbox = Arc::new(Mailbox::new(&key));
    let mut boxes: BTreeMap<u64, EpochBox> = BTreeMap::new();
    let mut answered: Option<RequestNonce> = None;
    let mut handled: HashMap<RequestNonce, Instant> = HashMap::new();
    let mut searches: JoinSet<(u64, Vec<SocketAddr>, Vec<SocketAddr>)> = JoinSet::new();
    let mut publishes: JoinSet<(u64, usize)> = JoinSet::new();
    let mut tick = interval(POLL_EVERY);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let who = short_hex(&viewer);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            Some(Ok((epoch, watch, stored))) = searches.join_next() => {
                tracing::debug!(viewer = %who, epoch, watch = watch.len(), stored = stored.len(), "DHT: mailbox nodes found");
                let now = now_unix();
                if let Some(b) = boxes.get_mut(&epoch) {
                    if watch.is_empty() || stored.is_empty() {
                        // Out of reach for now: soon again, rather than in ten minutes.
                        b.failed_searches += 1;
                        b.next_search = now + (SEARCH_RETRY_SECS << (b.failed_searches - 1).min(4)).min(SEARCH_RETRY_MAX_SECS);
                    } else {
                        b.failed_searches = 0;
                    }
                    if !watch.is_empty() {
                        b.watch = watch;
                    }
                    if !stored.is_empty() {
                        b.stored = stored;
                    }
                }
                if let Some(this) = this.upgrade() {
                    this.save_nodes();
                }
            }
            Some(Ok((epoch, stored))) = publishes.join_next() => {
                let Some(this) = this.upgrade() else { return };
                if this.generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                if stored > 0 {
                    tracing::debug!(viewer = %who, epoch, stored, "DHT: note stored");
                    this.listed.lock().unwrap().insert((viewer, epoch), now_unix());
                } else {
                    this.listed.lock().unwrap().remove(&(viewer, epoch));
                    if let Some(b) = boxes.get_mut(&epoch) {
                        b.published = None;
                        b.next_publish = now_unix() + PUBLISH_RETRY_SECS;
                    }
                }
                this.notify();
                continue;
            }
        }
        let Some(this) = this.upgrade() else { return };
        if this.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        let now = now_unix();
        let epochs = epochs_around(now);
        boxes.retain(|epoch, _| epochs.contains(epoch));
        handled.retain(|_, at| at.elapsed() < ANSWERED_TTL);
        let addrs = this.host_addresses(&dht);
        for &epoch in &epochs {
            let b = boxes.entry(epoch).or_insert_with(|| EpochBox {
                host: mailbox.slot(Side::Host, epoch),
                viewer: mailbox.slot(Side::Viewer, epoch),
                watch: Vec::new(),
                stored: Vec::new(),
                next_search: 0,
                failed_searches: 0,
                published: None,
                next_publish: 0,
                host_seq: None,
                request_seq: None,
                turn: 0,
            });
            if now >= b.next_search {
                b.next_search = now + SEARCH_EVERY_SECS;
                let (dht, viewer_key, host_key) = (dht.clone(), b.viewer.key, b.host.key);
                searches.spawn(async move {
                    let ((_, watch), (_, stored)) = tokio::join!(dht.get_mutable(&viewer_key, &[]), dht.get_mutable(&host_key, &[]));
                    let addrs = |found: Vec<transport::dht::Found>| found.into_iter().map(|f| f.node.addr).take(mailbox::MAX_NODES).collect::<Vec<_>>();
                    (epoch, addrs(watch), addrs(stored))
                });
            }
            if !b.watch.is_empty() {
                let nodes: Vec<SocketAddr> = b.watch.iter().copied().cycle().skip(b.turn % b.watch.len()).take(POLL_NODES.min(b.watch.len())).collect();
                b.turn = b.turn.wrapping_add(POLL_NODES);
                if let Some(item) = dht.poll_mutable(&nodes, &b.viewer.key, &[], b.request_seq).await {
                    b.request_seq = Some(item.seq);
                    if let Some(request) = mailbox.read_viewer(&b.viewer, &item)
                        && mailbox::fresh(request.at, now)
                        && !handled.contains_key(&request.nonce)
                    {
                        handled.insert(request.nonce, Instant::now());
                        tracing::info!(viewer = %who, addrs = ?request.addrs, "a paired Mac connects through the BitTorrent DHT");
                        this.punch_toward(&request.addrs, mailbox.punch_nonce(&request.nonce));
                        answered = Some(request.nonce);
                    }
                }
            }
            let due = match &b.published {
                None => now >= b.next_publish,
                Some((at, said, said_answered, said_watch)) => {
                    now.saturating_sub(*at) >= PUBLISH_EVERY_SECS || *said != addrs || *said_answered != answered || *said_watch != b.watch
                }
            };
            if due && !b.stored.is_empty() {
                let note = HostNote { at: now, answered, addrs: addrs.clone(), watch: b.watch.clone(), stored: b.stored.clone() };
                let seq = next_seq(b.host_seq, now_ms());
                b.host_seq = Some(seq);
                b.published = Some((now, addrs.clone(), answered, b.watch.clone()));
                let item = mailbox.host_item(&b.host, seq, &note);
                let (dht, nodes) = (dht.clone(), b.stored.clone());
                publishes.spawn(async move { (epoch, dht.store_at(&item, &nodes).await) });
            }
        }
        this.notify();
    }
}

/// `addrs` in canonical form, without repeats, in order.
fn dedup(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    for addr in addrs.into_iter().map(crate::canonical) {
        if !out.contains(&addr) {
            out.push(addr);
        }
    }
    out
}

fn load_nodes(path: &Path) -> Vec<SocketAddr> {
    std::fs::read_to_string(path).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).take(SAVED_NODES).collect()
}

/// "3 hours", "2 days": how long ago something was, roughly.
fn ago(secs: u64) -> String {
    let (n, unit) = match secs {
        s if s < 2 * 3600 => (s / 60, "minute"),
        s if s < 2 * 86_400 => (s / 3600, "hour"),
        s => (s / 86_400, "day"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_how_long_ago_roughly() {
        assert_eq!(ago(60), "1 minute");
        assert_eq!(ago(50 * 60), "50 minutes");
        assert_eq!(ago(3 * 3600), "3 hours");
        assert_eq!(ago(3 * 86_400), "3 days");
    }

    #[test]
    fn addresses_dedup_in_canonical_form() {
        let a: SocketAddr = "203.0.113.5:47800".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:203.0.113.5]:47800".parse().unwrap();
        let b: SocketAddr = "192.168.1.20:47800".parse().unwrap();
        assert_eq!(dedup(vec![a, mapped, b, a]), vec![a, b]);
    }

    #[test]
    fn saved_nodes_load_back() {
        let dir = std::env::temp_dir().join(format!("lankvm-dht-nodes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(NODES_FILE);
        std::fs::write(&path, "198.51.100.1:6881\nnot an address\n\n198.51.100.2:51413\n").unwrap();
        assert_eq!(load_nodes(&path), vec!["198.51.100.1:6881".parse::<SocketAddr>().unwrap(), "198.51.100.2:51413".parse().unwrap()]);
        assert!(load_nodes(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_address_tried_early_is_tried_again_once_the_host_answers() {
        let mut tries = Tries::default();
        let addr: SocketAddr = "203.0.113.5:47800".parse().unwrap();
        assert!(tries.early(addr));
        assert!(!tries.early(addr), "once as the note names it");
        assert!(tries.answered(addr), "and again after the host punched");
        assert!(!tries.answered(addr), "but only once more");
        for port in 1..=ANSWERED_ATTEMPTS as u16 {
            tries.answered(SocketAddr::new(addr.ip(), port));
        }
        assert!(!tries.answered("198.51.100.9:1".parse().unwrap()), "a few addresses at most");
    }

    #[test]
    fn a_note_is_fresh_for_one_rewrite_and_the_clocks_difference() {
        assert!(NOTE_FRESH_SECS > PUBLISH_EVERY_SECS + mailbox::SKEW_SECS);
        assert!(NOTE_FRESH_SECS <= 30 * 60, "a host that quit isn't waited for long");
    }
}
