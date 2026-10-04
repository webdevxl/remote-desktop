//! Rendezvous through a LanKVM server, the core's part (the protocol is in
//! `transport::rendezvous`, the socket's side of it in the gate).
//!
//! As a host, while internet access is on and a server is set: stays registered with the server
//! from the QUIC socket, which also keeps the router's mapping for that socket open, and answers
//! the requests of paired viewers the server passes on, after checking their tokens: punches
//! toward the viewer, or starts a relay session for it. A stranger's request goes unanswered.
//!
//! As a viewer: asks the server to introduce this Mac to a paired host, punches toward the
//! address it names and connects there, and asks for a relay when that doesn't work in time.
//!
//! One task reads the control messages the gate takes off the socket and passes each on: what
//! the server says to the host (challenges, answers to keepalives, viewers' requests) to the
//! registration task, and the answers to a viewer's requests to the attempt that sent them, by
//! nonce. The gate's servers are the host's and every one a viewer's attempt or relay session
//! uses, each for as long as it does.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use quinn::Connection;
use serde::Serialize;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout, timeout_at};
use transport::endpoint::Network;
use transport::identity::short_hex;
use transport::knock::{AccessKey, now_unix, random_bytes};
use transport::rendezvous::{
    Cookie, DEFAULT_SERVER, HOST_PUNCHES, KEEPALIVE_INTERVAL, MAX_BACKOFF, Message, Nonce, PUNCH_TIMEOUT, RESEND_AFTER, RendezvousId,
    RendezvousIdentity, SessionId, Token, VIEWER_PUNCHES, punch, token,
};

use crate::{Event, EventSink};

/// Where a server listens when its address names no port.
const DEFAULT_SERVER_PORT: u16 = 3478;
/// Messages for the registration task not handled yet. More are dropped, as a datagram can be.
const HOST_QUEUE: usize = 64;
/// Requests in a row the server left unanswered before it counts as unreachable.
const UNREACHABLE_AFTER: u32 = 3;
/// How long looking up a server's name may take.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Copies of a viewer's request sent at most: the first, then one after each of the first three
/// waits of [`RESEND_AFTER`] (at 0, 0.5, 1.5 and 3.5 s).
const REQUEST_SENDS: usize = 4;
/// How long a viewer's request waits for an answer in all; the last copy gets a second.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(4500);
/// How long a handshake through a relay may take: a few round trips by way of the server.
const RELAY_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);
/// A viewer's relay session stays this long after the session it carried is over, so the packets
/// that close the connection still reach the host.
const RELAY_LINGER: Duration = Duration::from_secs(1);
/// The longest a stopped registration waits to tell the server, when LanKVM quits.
const UNREGISTER_TIMEOUT: Duration = Duration::from_millis(200);

/// This Mac's dealings with LanKVM servers, as host and as viewer. One per core.
pub(crate) struct Rendezvous {
    network: Network,
    identity: RendezvousIdentity,
    rt: Handle,
    events: EventSink,
    /// Tests: viewers go straight to the relay (see `CoreOptions::force_relay`).
    force_relay: bool,
    /// Testing internet access on one Mac (`LANKVM_TEST_LOOPBACK_IS_INTERNET` is set, to
    /// anything): the default server is never contacted then, as while loopback counts as the
    /// internet (see [`Self::shielded`]).
    testing: bool,
    /// The default server was named explicitly (`LANKVM_RENDEZVOUS`), so even a test may reach it.
    default_allowed: bool,
    host: Mutex<Host>,
    /// Counts changes to the host's registration, so a registration that was stopped can't
    /// touch what came after it (its task may still be running when it is stopped).
    generation: AtomicU64,
    servers: Mutex<Servers>,
    /// Viewers' requests waiting for the server's answer, by nonce.
    waiters: Mutex<HashMap<Nonce, Waiter>>,
}

/// The host's side: the settings it follows, and its registration while they call for one.
struct Host {
    /// Internet access is on.
    enabled: bool,
    /// As set; "" for none.
    server: String,
    registration: Option<Registration>,
    state: State,
    /// Where the server sees this Mac, as it said last.
    observed: Option<SocketAddr>,
    /// LanKVM is quitting: no new registrations.
    closed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Off,
    Connecting,
    Registered,
    Unreachable,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Connecting => "connecting",
            Self::Registered => "registered",
            Self::Unreachable => "unreachable",
        }
    }
}

/// A running registration with one server.
struct Registration {
    /// As set.
    server: String,
    /// What the server says to the host, for its task.
    messages: mpsc::Sender<(SocketAddr, Message)>,
    task: JoinHandle<()>,
    /// The server's address, once found: where to unregister.
    resolved: Arc<Mutex<Option<SocketAddr>>>,
}

/// Which request the registration task sends next.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    /// REGISTER_BEGIN: asks for a challenge.
    Begin,
    /// REGISTER, answering the challenge with this cookie.
    Register(Cookie),
    /// KEEPALIVE.
    Registered,
}

/// The servers whose datagrams the gate keeps from quinn.
#[derive(Default)]
struct Servers {
    /// The host's.
    host: Option<SocketAddr>,
    /// Those of viewers' attempts and relay sessions, with how many use each.
    viewers: HashMap<SocketAddr, usize>,
}

struct Waiter {
    server: SocketAddr,
    answers: mpsc::UnboundedSender<Message>,
}

/// The host's LanKVM server as the UI sees it (`server` in `InternetView`).
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ServerView {
    /// As set ("host:port"), or "" for none.
    pub address: String,
    /// "off" (no server, or internet access off), "connecting", "registered" (paired Macs can
    /// reach this one through it) or "unreachable" (it doesn't answer; LanKVM keeps trying).
    pub state: &'static str,
    /// Where the server sees this Mac on the internet ("203.0.113.7:51234"), while registered.
    pub observed: Option<String>,
}

impl Default for ServerView {
    fn default() -> Self {
        Self { address: String::new(), state: State::Off.name(), observed: None }
    }
}

/// A connection a LanKVM server brought about, and the relay session it goes through if it does
/// (keep it for as long as the connection).
pub(crate) struct Introduced {
    pub(crate) conn: Connection,
    pub(crate) relay: Option<RelaySession>,
}

/// What came back for a viewer's request.
enum Answer {
    Message(Message),
    /// Nothing. `alive`: the server answered another request sent with it, so it is running and
    /// the host kept quiet.
    Silence { alive: bool },
}

impl Rendezvous {
    /// Starts passing on the control messages from servers. The host registers once
    /// [`Self::set_host_enabled`] and [`Self::set_host_server`] say so.
    pub(crate) fn start(
        network: Network,
        identity: RendezvousIdentity,
        rt: Handle,
        events: EventSink,
        force_relay: bool,
        testing: bool,
        default_allowed: bool,
    ) -> Arc<Self> {
        let control = network.gate.take_control_receiver();
        let this = Arc::new(Self {
            network,
            identity,
            rt: rt.clone(),
            events,
            force_relay,
            testing,
            default_allowed,
            host: Mutex::new(Host {
                enabled: false,
                server: String::new(),
                registration: None,
                state: State::Off,
                observed: None,
                closed: false,
            }),
            generation: AtomicU64::new(0),
            servers: Mutex::new(Servers::default()),
            waiters: Mutex::new(HashMap::new()),
        });
        rt.spawn(dispatch(Arc::downgrade(&this), control));
        this
    }

    pub(crate) fn force_relay(&self) -> bool {
        self.force_relay
    }

    /// The host registers while internet access is on (`enabled`) and a server is set, and not
    /// otherwise. Takes effect at once; the registration itself runs in the background. True if
    /// the registration changed.
    pub(crate) fn set_host_enabled(self: &Arc<Self>, enabled: bool) -> bool {
        self.change_host(|host| host.enabled = enabled)
    }

    /// The server the host registers with ("host:port", or "" for none). See
    /// [`Self::set_host_enabled`].
    pub(crate) fn set_host_server(self: &Arc<Self>, server: &str) -> bool {
        let server = server.trim().to_string();
        self.change_host(|host| host.server = server)
    }

    /// Changes one of the host's settings, under the lock that keeps the other as it is.
    fn change_host(self: &Arc<Self>, change: impl FnOnce(&mut Host)) -> bool {
        let changed = {
            let mut host = self.host.lock().unwrap();
            change(&mut host);
            self.update_host(&mut host)
        };
        if changed {
            (self.events)(Event::HostChanged);
        }
        changed
    }

    /// Starts or stops the registration as the settings say. True if anything changed.
    fn update_host(self: &Arc<Self>, host: &mut Host) -> bool {
        let wanted = (host.enabled && !host.server.is_empty() && !host.closed).then(|| host.server.clone());
        if host.registration.as_ref().map(|r| &r.server) == wanted.as_ref() {
            return false;
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(old) = host.registration.take() {
            self.stop(old);
        }
        self.set_host_addr(None);
        host.observed = None;
        host.state = match wanted {
            Some(server) => {
                tracing::info!(server, "registering with the LanKVM server");
                host.registration = Some(self.register(server, generation));
                State::Connecting
            }
            None => State::Off,
        };
        true
    }

    /// Ends a registration: stops its task, and tells the server in the background that this
    /// host is gone (best effort: the server forgets it after 75 s anyway).
    fn stop(&self, old: Registration) {
        old.task.abort();
        let resolved = *old.resolved.lock().unwrap();
        if let Some(server) = resolved.filter(|&server| !self.shielded(server)) {
            let (network, id) = (self.network.clone(), self.identity.id());
            self.rt.spawn(async move {
                let _ = network.send_raw(server, &Message::Unregister { id }.encode()).await;
            });
        }
    }

    /// When LanKVM quits, before the endpoint closes: stops registering, and tells the server.
    pub(crate) async fn shutdown(&self) {
        let old = {
            let mut host = self.host.lock().unwrap();
            host.closed = true;
            self.generation.fetch_add(1, Ordering::SeqCst);
            host.registration.take()
        };
        let Some(old) = old else { return };
        old.task.abort();
        let resolved = *old.resolved.lock().unwrap();
        if let Some(server) = resolved.filter(|&server| !self.shielded(server)) {
            let unregister = Message::Unregister { id: self.identity.id() }.encode();
            let _ = timeout(UNREGISTER_TIMEOUT, self.network.send_raw(server, &unregister)).await;
        }
    }

    pub(crate) fn host_view(&self) -> ServerView {
        let host = self.host.lock().unwrap();
        let state = if host.enabled && !host.server.is_empty() { host.state } else { State::Off };
        ServerView {
            address: host.server.clone(),
            state: state.name(),
            observed: host.observed.filter(|_| state == State::Registered).map(|o| o.to_string()),
        }
    }

    /// The server viewers are told to ask for this host at, and the ID to ask for: empty while
    /// the host doesn't register (internet access or the server off).
    pub(crate) fn announced(&self) -> (String, Vec<u8>) {
        let host = self.host.lock().unwrap();
        if host.enabled && !host.server.is_empty() { (host.server.clone(), self.identity.id().to_vec()) } else { Default::default() }
    }

    /// Notes what the registration of `generation` found, unless it has been replaced.
    fn report(&self, generation: u64, state: State, observed: Option<SocketAddr>) {
        let changed = {
            let mut host = self.host.lock().unwrap();
            if self.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let changed = host.state != state || host.observed != observed;
            host.state = state;
            host.observed = observed;
            changed
        };
        if changed {
            (self.events)(Event::HostChanged);
        }
    }

    /// The host's server is at `addr` for the registration of `generation`, unless it has been
    /// replaced.
    fn found_host_server(self: &Arc<Self>, generation: u64, addr: SocketAddr) {
        let _host = self.host.lock().unwrap();
        if self.generation.load(Ordering::SeqCst) == generation {
            self.set_host_addr(Some(addr));
        }
    }

    /// The host's server on the gate. One it replaces stays there [`RELAY_LINGER`] longer, and
    /// so do the host's relay sessions through it: the packets that close the connections they
    /// carry (internet access turned off, say) still reach the viewers.
    fn set_host_addr(self: &Arc<Self>, server: Option<SocketAddr>) {
        let lingering = {
            let mut servers = self.servers.lock().unwrap();
            let old = servers.host.filter(|&old| Some(old) != server);
            if let Some(old) = old {
                *servers.viewers.entry(old).or_default() += 1;
            }
            servers.host = server;
            self.sync_servers(&servers);
            old.map(|old| ServerLease { rendezvous: self.clone(), server: old })
        };
        // Spawned once `servers` is free: a runtime that is shutting down drops the task, and
        // with it the lease, at once.
        if let Some(lease) = lingering {
            self.rt.spawn(async move {
                tokio::time::sleep(RELAY_LINGER).await;
                drop(lease);
            });
        }
    }

    /// Hands the gate the servers in use: their datagrams never reach quinn as they arrived.
    fn sync_servers(&self, servers: &Servers) {
        let mut list: Vec<SocketAddr> = servers.viewers.keys().copied().collect();
        if let Some(host) = servers.host
            && !list.contains(&host)
        {
            list.push(host);
        }
        self.network.gate.set_servers(list);
    }

    /// Keeps `server` among the gate's servers until the lease is dropped.
    fn lease(self: &Arc<Self>, server: SocketAddr) -> ServerLease {
        let mut servers = self.servers.lock().unwrap();
        *servers.viewers.entry(server).or_default() += 1;
        self.sync_servers(&servers);
        ServerLease { rendezvous: self.clone(), server }
    }

    /// While testing (see [`Self::testing`]) or loopback counts as the internet, the default
    /// server is never contacted unless it was named explicitly: a test must not reach the real
    /// one. Its address is never found then (see [`Self::find`]), and nothing goes to it.
    fn shielded(&self, server: SocketAddr) -> bool {
        let testing = self.testing || self.network.gate.loopback_is_internet();
        !self.default_allowed && testing && DEFAULT_SERVER.parse::<SocketAddr>().is_ok_and(|d| d.ip() == server.ip().to_canonical())
    }

    /// The address of `server` ([`resolve`]), unless it is the default server's while that is
    /// shielded.
    async fn find(&self, server: &str) -> Result<SocketAddr> {
        let addr = resolve(server).await?;
        if self.shielded(addr) {
            bail!("{addr} is the default LanKVM server's address, never contacted while testing (LANKVM_RENDEZVOUS can name it)");
        }
        Ok(addr)
    }

    /// Sends `bytes` to `server`, unless it is shielded: that one was found before loopback
    /// began to count as the internet.
    async fn send_to(&self, server: SocketAddr, bytes: &[u8]) -> std::io::Result<()> {
        if self.shielded(server) {
            return Ok(());
        }
        self.network.send_raw(server, bytes).await
    }

    // MARK: Host

    fn register(self: &Arc<Self>, server: String, generation: u64) -> Registration {
        let (messages, received) = mpsc::channel(HOST_QUEUE);
        let resolved = Arc::new(Mutex::new(None));
        let task = self.rt.spawn(self.clone().keep_registered(server.clone(), generation, resolved.clone(), received));
        Registration { server, messages, task, resolved }
    }

    /// Keeps this host registered with `server`: registers, checks in every
    /// [`KEEPALIVE_INTERVAL`], and registers again whenever the server asks (it forgot this Mac,
    /// or sees it at a new address). A request goes out again while unanswered, after
    /// [`RESEND_AFTER`] and then less and less often; while the server doesn't answer, its name
    /// is looked up again. Meanwhile it answers the viewers' requests the server passes on. Runs
    /// until stopped.
    async fn keep_registered(
        self: Arc<Self>,
        server: String,
        generation: u64,
        resolved: Arc<Mutex<Option<SocketAddr>>>,
        mut messages: mpsc::Receiver<(SocketAddr, Message)>,
    ) {
        let id = self.identity.id();
        let mut addr: Option<SocketAddr> = None;
        let mut step = Step::Begin;
        let mut state = State::Connecting;
        // Requests sent since the server last answered.
        let mut unanswered = 0u32;
        let mut next = Instant::now();
        loop {
            tokio::select! {
                _ = sleep_until(next) => {
                    if addr.is_none() || (unanswered >= UNREACHABLE_AFTER && server.parse::<SocketAddr>().is_err()) {
                        match self.find(&server).await {
                            Ok(found) if Some(found) != addr => {
                                tracing::debug!(server, addr = %found, "found the LanKVM server");
                                addr = Some(found);
                                *resolved.lock().unwrap() = Some(found);
                                self.found_host_server(generation, found);
                                (step, unanswered) = (Step::Begin, 0);
                            }
                            Ok(_) => {}
                            Err(e) if addr.is_none() => {
                                if state != State::Unreachable {
                                    tracing::warn!(server, "couldn't find the LanKVM server: {e:#}");
                                }
                                state = State::Unreachable;
                                self.report(generation, state, None);
                                next = Instant::now() + resend_after(unanswered + UNREACHABLE_AFTER);
                                unanswered += 1;
                                continue;
                            }
                            Err(e) => tracing::debug!(server, "couldn't look up the LanKVM server again: {e:#}"),
                        }
                    }
                    let Some(to) = addr else { continue };
                    if unanswered >= UNREACHABLE_AFTER {
                        if state != State::Unreachable {
                            tracing::warn!(server, "the LanKVM server isn't answering: trying again, less and less often");
                            state = State::Unreachable;
                            self.report(generation, state, None);
                        }
                        // Its challenge may have expired meanwhile.
                        if let Step::Register(_) = step {
                            step = Step::Begin;
                        }
                    }
                    let request = match &step {
                        Step::Begin => Message::RegisterBegin { id },
                        Step::Register(cookie) => self.identity.register(cookie),
                        Step::Registered => Message::Keepalive { id },
                    };
                    if let Err(e) = self.send_to(to, &request.encode()).await {
                        tracing::debug!(%to, "couldn't send to the LanKVM server: {e}");
                    }
                    next = Instant::now() + resend_after(unanswered);
                    unanswered += 1;
                }
                received = messages.recv() => {
                    let Some((from, message)) = received else { return };
                    if Some(from) != addr {
                        continue;
                    }
                    match message {
                        // Only in answer to a request for one, or to a keepalive (the server
                        // forgot this Mac, or sees it at a new address) not answered yet. Anyone
                        // who can pass for the server could send one any time, to keep this
                        // Mac registering with a cookie of theirs instead of checking in.
                        Message::Challenge { cookie } if matches!(step, Step::Begin | Step::Registered) && unanswered > 0 => {
                            step = Step::Register(cookie);
                            unanswered = 0;
                            next = Instant::now();
                            if state != State::Connecting {
                                state = State::Connecting;
                                self.report(generation, state, None);
                            }
                        }
                        Message::Registered { observed, .. } => {
                            if state != State::Registered {
                                tracing::info!(server, %observed, "registered with the LanKVM server");
                            }
                            (step, state, unanswered) = (Step::Registered, State::Registered, 0);
                            self.report(generation, state, Some(observed));
                            next = Instant::now() + KEEPALIVE_INTERVAL;
                        }
                        Message::Alive { observed } if step == Step::Registered => {
                            if state != State::Registered {
                                tracing::info!(server, "the LanKVM server answers again");
                            }
                            (state, unanswered) = (State::Registered, 0);
                            self.report(generation, state, Some(observed));
                            next = Instant::now() + KEEPALIVE_INTERVAL;
                        }
                        Message::Incoming { nonce, token, viewer } => self.incoming(from, nonce, token, viewer).await,
                        Message::RelayOffer { nonce, token, sid, viewer } => self.relay_offer(from, nonce, token, sid, viewer).await,
                        _ => {}
                    }
                }
            }
        }
    }

    /// A viewer at `viewer` asks to connect. Answered only if its token comes from a paired
    /// viewer's key (each copy of the request, as the viewer resends it when an answer gets
    /// lost), and punched toward once.
    async fn incoming(&self, server: SocketAddr, nonce: Nonce, token: Token, viewer: SocketAddr) {
        let id = self.identity.id();
        let Some(request) = self.network.gate.check_token(&id, &nonce, &token, viewer, now_unix()) else {
            tracing::debug!(%viewer, "no answer to a connect request through the LanKVM server: not from a paired Mac");
            return;
        };
        if let Err(e) = self.send_to(server, &Message::Accept { id, nonce }.encode()).await {
            tracing::debug!(%server, "couldn't answer the LanKVM server: {e}");
        }
        if request.first {
            tracing::info!(%viewer, viewer_fp = %short_hex(&request.viewer), "a paired Mac connects through the LanKVM server");
            self.punch_toward(viewer, nonce);
        }
    }

    /// Punches toward `viewer` on the [`HOST_PUNCHES`] schedule: this Mac's router then takes the
    /// viewer's packets for answers and lets them in.
    fn punch_toward(&self, viewer: SocketAddr, nonce: Nonce) {
        let network = self.network.clone();
        self.rt.spawn(async move {
            let start = Instant::now();
            for at in HOST_PUNCHES {
                sleep_until(start + at).await;
                let _ = network.send_raw(viewer, &punch(&nonce)).await;
            }
        });
    }

    /// A viewer at `viewer` asks for relay session `sid`, as punching didn't open a path. The
    /// session starts on the gate before the answer goes out: the viewer's first packet follows
    /// right after it.
    async fn relay_offer(&self, server: SocketAddr, nonce: Nonce, token: Token, sid: SessionId, viewer: SocketAddr) {
        let id = self.identity.id();
        let Some(request) = self.network.gate.check_token(&id, &nonce, &token, viewer, now_unix()) else {
            tracing::debug!(%viewer, "no answer to a relay request through the LanKVM server: not from a paired Mac");
            return;
        };
        if request.first {
            let relay = self.network.gate.add_relay(server, sid);
            tracing::info!(%viewer, %relay, viewer_fp = %short_hex(&request.viewer), "a paired Mac connects through the LanKVM server's relay");
        }
        if let Err(e) = self.send_to(server, &Message::RelayAccept { id, sid }.encode()).await {
            tracing::debug!(%server, "couldn't answer the LanKVM server: {e}");
        }
    }

    fn to_host(&self, server: SocketAddr, message: Message) {
        if let Some(registration) = &self.host.lock().unwrap().registration {
            let _ = registration.messages.try_send((server, message));
        }
    }

    // MARK: Viewer

    /// Connects to paired host `name`, registered with `server` as `id`, which gave this Mac
    /// `key`: asks the server to introduce this Mac, punches toward the address it names and
    /// connects there, and goes through the server's relay when that doesn't work in time. The
    /// host answers only a token made with `key`; its certificate is the caller's to check.
    pub(crate) async fn connect(self: &Arc<Self>, name: &str, server: &str, id: RendezvousId, key: AccessKey) -> Result<Introduced> {
        let unreachable = || anyhow!("Couldn't reach the LanKVM server at {server}.");
        let offline = || anyhow!("{name} isn't reachable over the internet right now: LanKVM isn't running there, or its internet access is off.");
        let didnt_answer = || anyhow!("{name} didn't answer through the LanKVM server.");
        let silence = |alive| if alive { didnt_answer() } else { unreachable() };
        let addr = match self.find(server).await {
            Ok(addr) => addr,
            Err(e) => {
                tracing::info!(server, "couldn't find the LanKVM server: {e:#}");
                return Err(unreachable());
            }
        };
        // The host names its server, and while it is in use, nothing from its address reaches
        // quinn: one on the local network (another Mac's address, say) could only hold up
        // sessions with it, as it can't introduce Macs over the internet.
        let gate = &self.network.gate;
        if !gate.is_internet(addr) || gate.is_relayed(addr) || addr.ip().is_unspecified() {
            tracing::info!(server, %addr, "not using a LanKVM server that isn't on the internet");
            return Err(anyhow!("{name} uses a LanKVM server at {server}, which isn't on the internet."));
        }
        let lease = self.lease(addr);

        if !self.force_relay {
            let nonce = random_bytes();
            let connect = Message::Connect { id, nonce, token: token(&key, &id, &nonce, now_unix()) };
            match self.ask(addr, nonce, &connect).await {
                Answer::Message(Message::Peer { host, .. }) => {
                    if let Some(conn) = self.punch_through(host, nonce, key).await {
                        return Ok(Introduced { conn, relay: None });
                    }
                }
                Answer::Message(Message::Unknown { .. }) => return Err(offline()),
                Answer::Message(_) => {}
                Answer::Silence { alive } => return Err(silence(alive)),
            }
        }

        let nonce = random_bytes();
        let request = Message::RelayRequest { id, nonce, token: token(&key, &id, &nonce, now_unix()) };
        let sid = match self.ask(addr, nonce, &request).await {
            Answer::Message(Message::RelayReady { sid, .. }) => sid,
            Answer::Message(Message::Unknown { .. }) => return Err(offline()),
            Answer::Message(_) => return Err(didnt_answer()),
            Answer::Silence { alive } => return Err(silence(alive)),
        };
        let relay = RelaySession::start(lease, sid);
        tracing::info!(server, relay = %relay.addr, "connecting through the LanKVM server's relay");
        let connecting = self.network.endpoint.connect_with(self.network.internet_client_config(key), relay.addr, "lankvm").context("connect")?;
        match timeout(RELAY_HANDSHAKE_TIMEOUT, connecting).await {
            Ok(Ok(conn)) => Ok(Introduced { conn, relay: Some(relay) }),
            Ok(Err(e)) => Err(e).context("connect through the LanKVM server"),
            Err(_) => Err(didnt_answer()),
        }
    }

    /// Punches toward the host at `host` and connects to it there, if that works within
    /// [`PUNCH_TIMEOUT`].
    async fn punch_through(&self, host: SocketAddr, nonce: Nonce, key: AccessKey) -> Option<Connection> {
        let started = Instant::now();
        for _ in 0..VIEWER_PUNCHES {
            let _ = self.network.send_raw(host, &punch(&nonce)).await;
        }
        let connecting = match self.network.endpoint.connect_with(self.network.internet_client_config(key), host, "lankvm") {
            Ok(connecting) => connecting,
            Err(e) => {
                tracing::info!(%host, "can't connect there: {e}; asking the LanKVM server for a relay");
                return None;
            }
        };
        match timeout_at(started + PUNCH_TIMEOUT, connecting).await {
            Ok(Ok(conn)) => Some(conn),
            Ok(Err(e)) => {
                tracing::info!(%host, "no direct path: {e}; asking the LanKVM server for a relay");
                None
            }
            Err(_) => {
                tracing::info!(%host, "no direct path in time: asking the LanKVM server for a relay");
                None
            }
        }
    }

    /// Sends `request` (for request `nonce`) to `server` until it answers, [`REQUEST_SENDS`]
    /// times at most, and returns the answer. With each copy until it is answered goes a connect
    /// request for an ID no host has, which a running server always answers (UNKNOWN): when
    /// nothing else comes back, that tells a server that is down from a host that keeps quiet.
    async fn ask(&self, server: SocketAddr, nonce: Nonce, request: &Message) -> Answer {
        let probe_nonce: Nonce = random_bytes();
        let probe = Message::Connect { id: random_bytes(), nonce: probe_nonce, token: random_bytes() }.encode();
        let (answers_tx, mut answers) = mpsc::unbounded_channel();
        let _waiting = self.wait_for(server, [nonce, probe_nonce], answers_tx);
        let request = request.encode();
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        let (mut sent, mut next, mut alive) = (0, Instant::now(), false);
        loop {
            tokio::select! {
                _ = sleep_until(next), if sent < REQUEST_SENDS => {
                    if let Err(e) = self.send_to(server, &request).await {
                        tracing::debug!(%server, "couldn't send to the LanKVM server: {e}");
                    }
                    if !alive {
                        let _ = self.send_to(server, &probe).await;
                    }
                    next += RESEND_AFTER[sent];
                    sent += 1;
                }
                _ = sleep_until(deadline) => return Answer::Silence { alive },
                Some(answer) = answers.recv() => match answer {
                    Message::Unknown { nonce } if nonce == probe_nonce => alive = true,
                    answer => return Answer::Message(answer),
                },
            }
        }
    }

    /// Passes the server's answers to requests `nonces` to `answers` until dropped.
    fn wait_for(&self, server: SocketAddr, nonces: [Nonce; 2], answers: mpsc::UnboundedSender<Message>) -> Waiting<'_> {
        let mut waiters = self.waiters.lock().unwrap();
        for nonce in nonces {
            waiters.insert(nonce, Waiter { server, answers: answers.clone() });
        }
        Waiting { rendezvous: self, nonces }
    }

    /// An answer from `server` to a viewer's request `nonce`.
    fn answer(&self, server: SocketAddr, nonce: Nonce, message: Message) {
        if let Some(waiter) = self.waiters.lock().unwrap().get(&nonce)
            && waiter.server == server
        {
            let _ = waiter.answers.send(message);
        }
    }
}

/// Passes each control message from a server to what waits for it; the rest are dropped.
async fn dispatch(rendezvous: Weak<Rendezvous>, mut control: mpsc::Receiver<(SocketAddr, Vec<u8>)>) {
    while let Some((server, bytes)) = control.recv().await {
        let Some(rendezvous) = rendezvous.upgrade() else { return };
        let Some(message) = Message::decode(&bytes) else { continue };
        match message {
            Message::Peer { nonce, .. } | Message::RelayReady { nonce, .. } | Message::Unknown { nonce } => {
                rendezvous.answer(server, nonce, message);
            }
            Message::Challenge { .. }
            | Message::Registered { .. }
            | Message::Alive { .. }
            | Message::Incoming { .. }
            | Message::RelayOffer { .. } => rendezvous.to_host(server, message),
            // The session idled out at the server, or it had to make room. Its packets go nowhere
            // from now on, on either side.
            Message::RelayEnd { sid } => {
                if let Some(relay) = rendezvous.network.gate.remove_relay(server, &sid) {
                    tracing::info!(%server, %relay, "the LanKVM server ended a relay session");
                }
            }
            // Requests, which only servers take.
            _ => {}
        }
    }
}

/// How long the `k`th copy (from 0) of a request waits for an answer: [`RESEND_AFTER`], then
/// twice as long each time, up to [`MAX_BACKOFF`].
fn resend_after(k: u32) -> Duration {
    match RESEND_AFTER.get(k as usize) {
        Some(&wait) => wait,
        None => (RESEND_AFTER[RESEND_AFTER.len() - 1] * 2u32.pow((k as usize + 1 - RESEND_AFTER.len()).min(8) as u32)).min(MAX_BACKOFF),
    }
}

/// The address of `server`: "host:port", or a name or IP alone for the default port. IPv4 first:
/// LanKVM's socket is an IPv4 one.
async fn resolve(server: &str) -> Result<SocketAddr> {
    let server = server.trim();
    let canonical = |addr: SocketAddr| SocketAddr::new(addr.ip().to_canonical(), addr.port());
    if let Ok(addr) = server.parse::<SocketAddr>() {
        return Ok(canonical(addr));
    }
    if let Ok(ip) = server.parse::<IpAddr>() {
        return Ok(canonical(SocketAddr::new(ip, DEFAULT_SERVER_PORT)));
    }
    let with_port = match server.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => server.to_string(),
        _ => format!("{server}:{DEFAULT_SERVER_PORT}"),
    };
    let found: Vec<SocketAddr> =
        timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host(&with_port)).await.context("timed out")?.context("lookup")?.collect();
    found.iter().find(|a| a.is_ipv4()).or(found.first()).copied().map(canonical).with_context(|| format!("no address for {server}"))
}

/// Takes a viewer's requests off the waiters when dropped.
struct Waiting<'a> {
    rendezvous: &'a Rendezvous,
    nonces: [Nonce; 2],
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut waiters = self.rendezvous.waiters.lock().unwrap();
        for nonce in &self.nonces {
            waiters.remove(nonce);
        }
    }
}

/// Keeps a server among the gate's while a viewer's attempt or relay session uses it.
pub(crate) struct ServerLease {
    rendezvous: Arc<Rendezvous>,
    server: SocketAddr,
}

impl Drop for ServerLease {
    fn drop(&mut self) {
        let mut servers = self.rendezvous.servers.lock().unwrap();
        if let Some(users) = servers.viewers.get_mut(&self.server) {
            *users -= 1;
            if *users == 0 {
                servers.viewers.remove(&self.server);
            }
        }
        self.rendezvous.sync_servers(&servers);
    }
}

/// A viewer's relay session: started on the gate, and ended a moment after this is dropped.
pub(crate) struct RelaySession {
    lease: Option<ServerLease>,
    sid: SessionId,
    /// What stands for the host on this Mac: connect to it.
    addr: SocketAddr,
}

impl RelaySession {
    fn start(lease: ServerLease, sid: SessionId) -> Self {
        let addr = lease.rendezvous.network.gate.add_relay(lease.server, sid);
        Self { lease: Some(lease), sid, addr }
    }
}

impl Drop for RelaySession {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else { return };
        let sid = self.sid;
        let rt = lease.rendezvous.rt.clone();
        rt.spawn(async move {
            tokio::time::sleep(RELAY_LINGER).await;
            lease.rendezvous.network.gate.remove_relay(lease.server, &sid);
            drop(lease);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_wait_longer_and_longer_up_to_the_limit() {
        let waits: Vec<f64> = (0..9).map(|k| resend_after(k).as_secs_f64()).collect();
        assert_eq!(waits, [0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 30.0, 30.0, 30.0]);
        assert_eq!(resend_after(u32::MAX), MAX_BACKOFF);
    }

    /// Only looks: nothing here sends anything.
    #[tokio::test]
    async fn tests_never_find_the_default_server_unless_it_is_named() {
        let dir = std::env::temp_dir().join(format!("lankvm-test-shield-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let identity = transport::identity::DeviceIdentity::load_or_create(&dir).unwrap();
        let network = Network::bind(SocketAddr::from(([127, 0, 0, 1], 0)), &identity).unwrap();
        let start = |testing, default_allowed| {
            let identity = RendezvousIdentity::load_or_create(&dir).unwrap();
            let events: EventSink = Arc::new(|_| {});
            Rendezvous::start(network.clone(), identity, Handle::current(), events, false, testing, default_allowed)
        };
        let default: SocketAddr = DEFAULT_SERVER.parse().unwrap();
        let other: SocketAddr = "203.0.113.7:3478".parse().unwrap();

        let app = start(false, false);
        assert!(!app.shielded(default) && app.find(DEFAULT_SERVER).await.is_ok());
        // LANKVM_TEST_LOOPBACK_IS_INTERNET set, to anything.
        let testing = start(true, false);
        assert!(testing.shielded(default) && testing.shielded(SocketAddr::new(default.ip(), 9)) && !testing.shielded(other));
        assert!(testing.find(DEFAULT_SERVER).await.is_err() && testing.find(&default.ip().to_string()).await.is_err());
        assert_eq!(testing.find("203.0.113.7").await.unwrap(), other);
        // Loopback counting as the internet, from any time on.
        network.gate.set_loopback_is_internet(true);
        assert!(app.shielded(default) && app.find(DEFAULT_SERVER).await.is_err());
        // LANKVM_RENDEZVOUS named it.
        assert!(!start(true, true).shielded(default));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn servers_resolve_with_the_default_port() {
        let addr = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(resolve("203.0.113.7:4000").await.unwrap(), addr("203.0.113.7:4000"));
        assert_eq!(resolve(" 203.0.113.7 ").await.unwrap(), addr("203.0.113.7:3478"));
        assert_eq!(resolve("[::ffff:203.0.113.7]:4000").await.unwrap(), addr("203.0.113.7:4000"));
        assert_eq!(resolve("2001:db8::7").await.unwrap(), addr("[2001:db8::7]:3478"));
        assert_eq!(resolve("localhost").await.unwrap(), addr("127.0.0.1:3478"));
        assert_eq!(resolve("localhost:9").await.unwrap(), addr("127.0.0.1:9"));
        assert_eq!(resolve(DEFAULT_SERVER).await.unwrap(), addr(DEFAULT_SERVER));
    }
}
