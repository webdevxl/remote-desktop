//! The gate in front of the QUIC endpoint: what from the internet reaches quinn at all.
//!
//! Datagrams from the local network pass untouched. From anywhere else, quinn sees only packets
//! that continue a session or handshake this endpoint is part of (short headers, which quinn
//! routes by connection ID and silently drops when the ID isn't ours, see [`crate::cid`]; and
//! long headers addressed to one of our connection IDs, though a client's Initial carrying a
//! token only from an address that knocked), and a viewer's first packet when it
//! carries a valid knock from a viewer allowed in (see [`crate::knock`]). Everything else is
//! dropped before quinn can answer it, so a stranger gets no Version Negotiation, Retry,
//! CONNECTION_CLOSE or stateless reset: the port looks closed. The host checks the knock again
//! with [`Gate::admit`] when quinn hands it the connection attempt.
//!
//! One answer can still get out. quinn sends a stateless reset for a short header addressed to a
//! connection ID this endpoint issued but no longer uses (one from a session that has ended, a
//! retired one, or a Retry's). Only someone who watched an earlier session's packets has such an
//! ID, and the reset tells them nothing more than that the host is still running. Short headers
//! pass from any address so that a session survives the viewer's address changing (its NAT
//! rebinding, or a switch from Wi-Fi to a phone's hotspot): quinn moves the connection to the new
//! address, which a gate passing only addresses it already knows would prevent.
//!
//! The socket also talks to rendezvous servers (see [`crate::rendezvous`]), and nothing from a
//! server the endpoint uses reaches quinn as it arrived. Control messages go to the core through a
//! channel ([`Gate::take_control_receiver`]). Relayed QUIC packets are unwrapped and handed to
//! quinn as if they came from an address standing for their relay session, in 240.0.0.0/4, which
//! no real peer has ([`Gate::add_relay`]). Being outside the local network, they then pass the
//! filter above like any packet from the internet, knock included. quinn's packets to such an
//! address go to the server, wrapped, and never to the address itself: with no session behind it,
//! they are dropped. Punches between Macs are dropped. Without a server, all of this costs one
//! flag check per receive and a look at the destination address per send.
//!
//! A relayed connection can move straight to its peer later, when the peer turns out to be
//! reachable after all (both Macs on one network behind a router that doesn't hairpin): see
//! [`DirectPath`]. quinn still knows the peer by the relay session's address; its packets go from
//! a socket of their own, and only what comes from the peer there reaches quinn, as from that
//! address.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use tokio::sync::mpsc;

use crate::cid::CidKey;
use crate::identity::{Fingerprint, short_hex};
use crate::knock::{AccessKey, Budget, KNOCK_LEN, KnockKeys, ReplayCache, Seen, now_unix};
use crate::net::is_local_network;
use crate::rendezvous::{self, DATA_HEADER_LEN, FromServer, RendezvousId, SessionId, Token};

const LONG_HEADER: u8 = 0x80;
const QUIC_V1: u32 = 1;
const MAX_CID_LEN: usize = 20;
const INITIAL: u8 = 0;
/// Clients pad the datagram carrying their first packet to at least this, and quinn ignores
/// smaller ones.
const MIN_INITIAL_SIZE: usize = 1200;
/// A relay session nothing has gone through for this long is forgotten. The server ends its side
/// after 60 s.
const RELAY_IDLE: Duration = Duration::from_secs(120);
/// Control messages waiting for the core. More are dropped: anyone can send datagrams that claim
/// to come from the server, so this mustn't grow without bound.
const CONTROL_QUEUE: usize = 256;
/// Relay sessions' addresses: 240.0.0.0/4, reserved, so no real peer has one.
const RELAY_NET: u32 = 0xf000_0000;
const RELAY_MASK: u32 = 0xf000_0000;
/// Every relay session's address has this port; the IP alone tells them apart.
const RELAY_PORT: u16 = 1;
/// Tries for [`Network::send_raw`](crate::endpoint::Network::send_raw) while the socket's send
/// buffer is full, and how long each waits for room.
const RAW_SEND_ATTEMPTS: usize = 3;
const RAW_SEND_WAIT: Duration = Duration::from_millis(20);

/// Decides what from the internet reaches the endpoint. One per endpoint, shared with its socket.
pub struct Gate {
    cid_key: Arc<CidKey>,
    /// None while internet access is off.
    keys: RwLock<Option<Arc<KnockKeys>>>,
    replay: Mutex<ReplayCache>,
    budget: Mutex<Budget>,
    loopback_is_internet: AtomicBool,
    admitted: AtomicU64,
    ignored: AtomicU64,
    throttled: AtomicU64,
    replayed: AtomicU64,
    /// The rendezvous servers in use, as host and as viewer, in canonical form.
    servers: RwLock<Vec<SocketAddr>>,
    /// Whether there are any: all a received datagram costs when there aren't.
    any_server: AtomicBool,
    /// Where control messages from the servers go. None until the core asks for them.
    control: Mutex<Option<mpsc::Sender<(SocketAddr, Vec<u8>)>>>,
    relays: RwLock<Relays>,
    /// Nonces of rendezvous requests whose token checked out, and the viewer address each came
    /// from.
    tokens: Mutex<ReplayCache<rendezvous::Nonce>>,
    token_budget: Mutex<Budget>,
    /// What relay sessions' activity times count from.
    epoch: Instant,
    /// Whether the endpoint's socket is IPv6 (dual-stack), so quinn knows IPv4 addresses
    /// IPv4-mapped.
    socket_v6: AtomicBool,
}

/// A rendezvous request whose token checked out (see [`Gate::check_token`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidRequest {
    /// The viewer whose key made the token.
    pub viewer: Fingerprint,
    /// False if the request came before from the same address: the viewer resent it because the
    /// answer got lost, or someone is replaying a copy. Answer it again, to the server, but punch
    /// or start a relay session only for the first: anything more would let a copy of the request
    /// make the host send far more than it costs to send.
    pub first: bool,
}

/// What the gate has done since the endpoint started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GateStats {
    /// Distinct knocks let in.
    pub admitted: u64,
    /// Datagrams from the internet dropped, and connection attempts whose knock failed.
    pub ignored: u64,
    /// Knocks not checked because too many arrived at once (also counted as ignored).
    pub throttled: u64,
    /// Valid knocks that arrived from a second address (also counted as ignored).
    pub replayed: u64,
}

impl Gate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cid_key: Arc::new(CidKey::random()),
            keys: RwLock::new(None),
            replay: Mutex::new(ReplayCache::default()),
            budget: Mutex::new(Budget::new(Instant::now())),
            loopback_is_internet: AtomicBool::new(false),
            admitted: AtomicU64::new(0),
            ignored: AtomicU64::new(0),
            throttled: AtomicU64::new(0),
            replayed: AtomicU64::new(0),
            servers: RwLock::new(Vec::new()),
            any_server: AtomicBool::new(false),
            control: Mutex::new(None),
            relays: RwLock::new(Relays::default()),
            tokens: Mutex::new(ReplayCache::default()),
            token_budget: Mutex::new(Budget::new(Instant::now())),
            epoch: Instant::now(),
            socket_v6: AtomicBool::new(false),
        })
    }

    /// True if `addr` is outside the local network, so a connection with it is an internet one.
    pub fn is_internet(&self, addr: SocketAddr) -> bool {
        !is_local_network(addr.ip())
            || (self.loopback_is_internet.load(Relaxed) && addr.ip().to_canonical().is_loopback())
    }

    /// Treats loopback as the internet, so tests can exercise the gate on one machine.
    #[doc(hidden)]
    pub fn set_loopback_is_internet(&self, on: bool) {
        self.loopback_is_internet.store(on, Relaxed);
    }

    pub fn loopback_is_internet(&self) -> bool {
        self.loopback_is_internet.load(Relaxed)
    }

    /// The viewers allowed in over the internet, with their access keys. None turns internet
    /// access off. Takes effect for the next knock; connections already open stay open.
    pub fn set_keys(&self, keys: Option<Vec<(Fingerprint, AccessKey)>>) {
        *self.keys.write().unwrap() = keys.map(|keys| Arc::new(KnockKeys::new(keys)));
    }

    /// Checks a knock (a client's first destination connection ID) from `remote`: the viewer it
    /// names if it is valid for a current key, fresh, and not a replay from another address.
    /// Used by the socket filter and again when quinn hands over the connection attempt.
    pub fn admit(&self, dcid: &[u8], remote: SocketAddr) -> Option<Fingerprint> {
        self.admit_at(dcid, remote, Instant::now(), now_unix())
    }

    pub fn stats(&self) -> GateStats {
        GateStats {
            admitted: self.admitted.load(Relaxed),
            ignored: self.ignored.load(Relaxed),
            throttled: self.throttled.load(Relaxed),
            replayed: self.replayed.load(Relaxed),
        }
    }

    /// The rendezvous servers this endpoint uses, as host and as viewer: what comes from them is
    /// never handed to quinn as it arrived. Replaces the list; relay sessions through a server no
    /// longer on it end.
    pub fn set_servers(&self, servers: Vec<SocketAddr>) {
        let servers: Vec<SocketAddr> = servers.into_iter().map(canonical).collect();
        let mut current = self.servers.write().unwrap();
        self.relays.write().unwrap().retain_servers(&servers);
        self.any_server.store(!servers.is_empty(), Relaxed);
        *current = servers;
    }

    pub fn servers(&self) -> Vec<SocketAddr> {
        self.servers.read().unwrap().clone()
    }

    /// Control messages from the servers as they arrive, each with the server it came from (in
    /// canonical form), still encoded (see [`rendezvous::Message::decode`]). Each call makes a new
    /// channel and the previous receiver gets nothing more. Until the first call, and while the
    /// receiver lags by [`CONTROL_QUEUE`] messages, they are dropped.
    pub fn take_control_receiver(&self) -> mpsc::Receiver<(SocketAddr, Vec<u8>)> {
        let (sender, receiver) = mpsc::channel(CONTROL_QUEUE);
        *self.control.lock().unwrap() = Some(sender);
        receiver
    }

    /// Starts relay session `sid` through `server` and returns the address that stands for the
    /// peer at its other end: connect to it, or accept from it, as from any address on the
    /// internet. It is in the form quinn uses on the endpoint's socket (IPv4-mapped on a
    /// dual-stack one), so it equals the remote address of a connection through the session. The
    /// same address again for a session already started. Adds `server` to the servers if it
    /// isn't one.
    pub fn add_relay(&self, server: SocketAddr, sid: SessionId) -> SocketAddr {
        let server = canonical(server);
        let mut servers = self.servers.write().unwrap();
        if !servers.contains(&server) {
            servers.push(server);
            self.any_server.store(true, Relaxed);
        }
        let ip = self.relays.write().unwrap().add(server, sid, self.now_ms());
        tracing::debug!(%server, relay = %ip, "relay session started");
        relay_addr(ip, self.socket_v6.load(Relaxed))
    }

    /// Ends relay session `sid` through `server` (the server ended it, or the session it carried
    /// is over): the address that stood for it, as [`Self::add_relay`] gave it, if it was running.
    pub fn remove_relay(&self, server: SocketAddr, sid: &SessionId) -> Option<SocketAddr> {
        let ip = self.relays.write().unwrap().remove(canonical(server), sid)?;
        tracing::debug!(%server, relay = %ip, "relay session ended");
        Some(relay_addr(ip, self.socket_v6.load(Relaxed)))
    }

    /// True if `addr` stands for a relay session (see [`Self::add_relay`]), so a connection with it
    /// goes through a rendezvous server.
    pub fn is_relayed(&self, addr: SocketAddr) -> bool {
        relay_ip(addr).is_some()
    }

    /// Checks the token of a rendezvous request (to connect, or for a relay) to this host, `id`,
    /// from the viewer the server saw at `viewer`: Some, naming the viewer, if the token is valid
    /// for a current key and fresh, and its nonce didn't come before from another address. The
    /// same request again from the same address passes too, marked as not the first: the server
    /// passes it on again when the viewer resends it. None while internet access is off.
    pub fn check_token(
        &self,
        id: &RendezvousId,
        nonce: &rendezvous::Nonce,
        token: &Token,
        viewer: SocketAddr,
        now_unix: u64,
    ) -> Option<ValidRequest> {
        self.check_token_at(id, nonce, token, viewer, Instant::now(), now_unix)
    }

    pub(crate) fn cid_key(&self) -> Arc<CidKey> {
        self.cid_key.clone()
    }

    /// The filter: false if the endpoint should never see this datagram from `remote`.
    pub(crate) fn allow_datagram(&self, data: &[u8], remote: SocketAddr) -> bool {
        !self.is_internet(remote) || self.allow_from_internet(data, remote)
    }

    /// Decides on one receive, after unwrapping it in place if it is relayed. False drops it.
    fn accept_received(&self, buf: &mut [u8], meta: &mut RecvMeta) -> bool {
        // Without a server there is nothing but QUIC to expect.
        if self.any_server.load(Relaxed) && !self.demux(buf, meta) {
            return false;
        }
        self.allow_received(&buf[..meta.len], meta.stride, meta.addr)
    }

    /// Takes what comes from a rendezvous server out of quinn's way, and drops punches. False if
    /// nothing is left for quinn.
    fn demux(&self, buf: &mut [u8], meta: &mut RecvMeta) -> bool {
        let from = canonical(meta.addr);
        if self.servers.read().unwrap().contains(&from) {
            return self.from_server(from, buf, meta);
        }
        // Punches only open routers on the way. (quinn would drop them too, as short headers
        // for a connection ID that isn't ours.)
        !(rendezvous::is_punch(&buf[..segment_len(meta)]) && self.is_internet(meta.addr))
    }

    /// A receive from rendezvous server `server`: control messages go to the core, and relayed
    /// packets are moved to the front of `buf`, without their header, and given the address of
    /// their relay session. With Linux GRO a receive holds several datagrams `stride` bytes apart
    /// (only the last can be shorter); the packets of the first running session among them are
    /// kept, `stride - DATA_HEADER_LEN` bytes apart. False if nothing is left for quinn.
    fn from_server(&self, server: SocketAddr, buf: &mut [u8], meta: &mut RecvMeta) -> bool {
        let (len, stride, now) = (meta.len, segment_len(meta), self.now_ms());
        let mut session = None;
        let (mut read, mut kept) = (0, 0);
        while read < len {
            let end = (read + stride).min(len);
            match rendezvous::classify(&buf[read..end]) {
                FromServer::Control => self.forward_control(server, &buf[read..end]),
                FromServer::Data(sid) => {
                    if let Some(ip) = self.relays.read().unwrap().address(server, &sid, now)
                        && *session.get_or_insert(ip) == ip
                    {
                        buf.copy_within(read + DATA_HEADER_LEN..end, kept);
                        kept += end - read - DATA_HEADER_LEN;
                    }
                }
                FromServer::Other => {}
            }
            read = end;
        }
        let Some(ip) = session else { return false };
        meta.len = kept;
        meta.stride = stride - DATA_HEADER_LEN;
        meta.addr = relay_addr(ip, meta.addr.is_ipv6());
        true
    }

    fn forward_control(&self, server: SocketAddr, message: &[u8]) {
        if let Some(control) = &*self.control.lock().unwrap() {
            // When full, it is lost like any datagram, and the sender resends.
            let _ = control.try_send((server, message.to_vec()));
        }
    }

    /// Where quinn's datagram to `destination` goes. Costs a look at the address unless it is a
    /// relay session's.
    fn route(&self, destination: SocketAddr) -> Route {
        let Some(ip) = relay_ip(destination) else { return Route::Direct };
        match self.relays.read().unwrap().session(ip, self.now_ms()) {
            Some((server, sid)) => Route::Relay(server, sid),
            None => Route::Nowhere,
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn check_token_at(
        &self,
        id: &RendezvousId,
        nonce: &rendezvous::Nonce,
        token: &Token,
        viewer: SocketAddr,
        now: Instant,
        now_unix: u64,
    ) -> Option<ValidRequest> {
        let keys = self.keys.read().unwrap().clone()?;
        if !self.token_budget.lock().unwrap().take(now) {
            return None;
        }
        let viewer_fp = rendezvous::verify_token(&keys, id, nonce, token, now_unix)?;
        let first = match self.tokens.lock().unwrap().note(*nonce, canonical(viewer), now) {
            Seen::First => true,
            Seen::Again => false,
            Seen::Replayed => {
                tracing::debug!(%viewer, viewer_fp = %short_hex(&viewer_fp), "ignored a rendezvous request replayed from another address");
                return None;
            }
        };
        Some(ValidRequest { viewer: viewer_fp, first })
    }

    /// [`Self::allow_datagram`] for one receive: with Linux GRO, `data` holds several datagrams
    /// from the same sender, `stride` bytes apart, and passes only if every one of them does.
    fn allow_received(&self, data: &[u8], stride: usize, remote: SocketAddr) -> bool {
        if stride == 0 || stride >= data.len() {
            return self.allow_datagram(data, remote);
        }
        !self.is_internet(remote) || data.chunks(stride).all(|datagram| self.allow_from_internet(datagram, remote))
    }

    fn allow_from_internet(&self, data: &[u8], remote: SocketAddr) -> bool {
        let allow = match classify(data, &self.cid_key) {
            Class::Pass => true,
            // Counts it as ignored itself when it fails.
            Class::Knock(dcid) => return self.admit(dcid, remote).is_some(),
            Class::FromKnocker => self.replay.lock().unwrap().knocked_from(remote, Instant::now()),
            Class::Drop => false,
        };
        if !allow {
            self.ignored.fetch_add(1, Relaxed);
        }
        allow
    }

    fn admit_at(&self, dcid: &[u8], remote: SocketAddr, now: Instant, now_unix: u64) -> Option<Fingerprint> {
        let viewer = self.check_knock(dcid, remote, now, now_unix);
        if viewer.is_none() {
            self.ignored.fetch_add(1, Relaxed);
        }
        viewer
    }

    fn check_knock(&self, dcid: &[u8], remote: SocketAddr, now: Instant, now_unix: u64) -> Option<Fingerprint> {
        if dcid.len() != KNOCK_LEN {
            return None;
        }
        let keys = self.keys.read().unwrap().clone()?;
        if !self.budget.lock().unwrap().take(now) {
            self.throttled.fetch_add(1, Relaxed);
            return None;
        }
        let (viewer, nonce) = keys.verify(dcid, now_unix)?;
        match self.replay.lock().unwrap().note(nonce, remote, now) {
            Seen::First => {
                self.admitted.fetch_add(1, Relaxed);
                tracing::debug!(%remote, viewer = %short_hex(&viewer), "admitted a knock from the internet");
                Some(viewer)
            }
            Seen::Again => Some(viewer),
            Seen::Replayed => {
                self.replayed.fetch_add(1, Relaxed);
                tracing::debug!(%remote, viewer = %short_hex(&viewer), "ignored a knock replayed from another address");
                None
            }
        }
    }
}

impl fmt::Debug for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gate")
            .field("internet_access", &self.keys.read().unwrap().is_some())
            .field("loopback_is_internet", &self.loopback_is_internet())
            .field("servers", &*self.servers.read().unwrap())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// `addr` with an IPv4-mapped IPv6 address turned into the IPv4 one, as servers are compared.
fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// `addr` in the family of a socket that is IPv6 (`v6`) or IPv4 (as far as it can be).
fn in_family(addr: SocketAddr, v6: bool) -> SocketAddr {
    match (addr.ip(), v6) {
        (IpAddr::V4(ip), true) => SocketAddr::new(ip.to_ipv6_mapped().into(), addr.port()),
        (IpAddr::V6(_), false) => canonical(addr),
        _ => addr,
    }
}

/// The relay address `ip` as quinn sees it on a socket of that family.
fn relay_addr(ip: Ipv4Addr, v6: bool) -> SocketAddr {
    in_family(SocketAddr::from((ip, RELAY_PORT)), v6)
}

/// The IP of `addr` if it is a relay session's address.
fn relay_ip(addr: SocketAddr) -> Option<Ipv4Addr> {
    match addr.ip().to_canonical() {
        IpAddr::V4(ip) if u32::from(ip) & RELAY_MASK == RELAY_NET => Some(ip),
        _ => None,
    }
}

/// The length of a receive's first datagram.
fn segment_len(meta: &RecvMeta) -> usize {
    if meta.stride == 0 { meta.len } else { meta.stride.min(meta.len) }
}

enum Route {
    /// Not a relay session's address.
    Direct,
    Relay(SocketAddr, SessionId),
    /// A relay session that has ended: dropped, as if lost on the way.
    Nowhere,
}

/// The running relay sessions, each known to quinn by an address of its own.
#[derive(Default)]
struct Relays {
    by_address: HashMap<Ipv4Addr, Relay>,
    by_session: HashMap<(SocketAddr, SessionId), Ipv4Addr>,
    /// The last address handed out, counting up through 240.0.0.0/4.
    last: u32,
}

struct Relay {
    server: SocketAddr,
    sid: SessionId,
    /// When a packet last went through, in milliseconds since the gate's epoch.
    active_at: AtomicU64,
}

impl Relay {
    /// Notes activity at `now`; false if the session had already been idle too long.
    fn touch(&self, now: u64) -> bool {
        if now.saturating_sub(self.active_at.load(Relaxed)) >= RELAY_IDLE.as_millis() as u64 {
            return false;
        }
        self.active_at.fetch_max(now, Relaxed);
        true
    }
}

impl Relays {
    fn add(&mut self, server: SocketAddr, sid: SessionId, now: u64) -> Ipv4Addr {
        self.expire(now);
        if let Some(&ip) = self.by_session.get(&(server, sid)) {
            self.by_address[&ip].active_at.fetch_max(now, Relaxed);
            return ip;
        }
        let ip = loop {
            self.last = self.last.wrapping_add(1) & !RELAY_MASK;
            let ip = Ipv4Addr::from(RELAY_NET | self.last);
            if self.last != 0 && !self.by_address.contains_key(&ip) {
                break ip;
            }
        };
        self.by_address.insert(ip, Relay { server, sid, active_at: AtomicU64::new(now) });
        self.by_session.insert((server, sid), ip);
        ip
    }

    fn remove(&mut self, server: SocketAddr, sid: &SessionId) -> Option<Ipv4Addr> {
        let ip = self.by_session.remove(&(server, *sid))?;
        self.by_address.remove(&ip);
        Some(ip)
    }

    /// The address of session `sid` through `server`, if it is running. Counts as activity.
    fn address(&self, server: SocketAddr, sid: &SessionId, now: u64) -> Option<Ipv4Addr> {
        let ip = *self.by_session.get(&(server, *sid))?;
        self.by_address[&ip].touch(now).then_some(ip)
    }

    /// The server and session behind relay address `ip`, if it is running. Counts as activity.
    fn session(&self, ip: Ipv4Addr, now: u64) -> Option<(SocketAddr, SessionId)> {
        let relay = self.by_address.get(&ip)?;
        relay.touch(now).then_some((relay.server, relay.sid))
    }

    fn expire(&mut self, now: u64) {
        let idle = RELAY_IDLE.as_millis() as u64;
        self.retain(|relay| now.saturating_sub(relay.active_at.load(Relaxed)) < idle);
    }

    fn retain_servers(&mut self, servers: &[SocketAddr]) {
        self.retain(|relay| servers.contains(&relay.server));
    }

    fn retain(&mut self, keep: impl Fn(&Relay) -> bool) {
        self.by_address.retain(|_, relay| keep(relay));
        let by_address = &self.by_address;
        self.by_session.retain(|_, ip| by_address.contains_key(ip));
    }
}

enum Class<'a> {
    Pass,
    /// A first packet whose destination connection ID must be a valid knock.
    Knock(&'a [u8]),
    /// Passes only from an address a valid knock recently came from.
    FromKnocker,
    Drop,
}

/// Sorts a datagram from the internet by its first QUIC header.
fn classify<'a>(data: &'a [u8], cid_key: &CidKey) -> Class<'a> {
    let Some(&first) = data.first() else { return Class::Drop };
    if first & LONG_HEADER == 0 {
        // quinn routes it by connection ID, and drops it without answering when the ID is
        // unknown and its tag doesn't check out (see [`crate::cid`]). A peer's stateless reset
        // passes too, so it still ends that connection at once.
        return Class::Pass;
    }
    let Some(header) = data.get(..6) else { return Class::Drop };
    // Both ends speak QUIC v1. Dropping every other version (Version Negotiation is version 0)
    // means quinn never answers a probe with a Version Negotiation packet.
    if u32::from_be_bytes(header[1..5].try_into().expect("4 bytes")) != QUIC_V1 {
        return Class::Drop;
    }
    let dcid_len = header[5] as usize;
    if dcid_len > MAX_CID_LEN {
        return Class::Drop;
    }
    let Some(dcid) = data.get(6..6 + dcid_len) else { return Class::Drop };
    let initial = (first & 0x30) >> 4 == INITIAL;
    if cid_key.is_ours(dcid) {
        // A client's Initials after a Retry carry the Retry's token, and quinn answers a token
        // that is expired or was issued to another address with CONNECTION_CLOSE. So a token
        // copied off the wire would get a stranger an answer. The real client knocked from the
        // same address a moment before. Servers' Initials never carry a token.
        if initial && initial_has_token(&data[6 + dcid_len..]) != Some(false) {
            return Class::FromKnocker;
        }
        // Handshake, Retry, or a server's Initial to our client.
        return Class::Pass;
    }
    if initial && data.len() >= MIN_INITIAL_SIZE {
        return Class::Knock(dcid);
    }
    Class::Drop
}

/// Whether an Initial carries a token, given the bytes after its destination connection ID: the
/// source connection ID's length and the ID, then the token's length as a QUIC varint. None if
/// the datagram ends first.
fn initial_has_token(rest: &[u8]) -> Option<bool> {
    let scid_len = *rest.first()? as usize;
    let token_len = rest.get(1 + scid_len..)?;
    let first = *token_len.first()?;
    // The top two bits give the varint's size: 1, 2, 4 or 8 bytes.
    let varint = token_len.get(..1 << (first >> 6))?;
    Some(first & 0x3f != 0 || varint[1..].iter().any(|&b| b != 0))
}

/// The endpoint's UDP socket, with the gate's filter on everything it receives.
#[derive(Debug)]
pub(crate) struct GatedSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    gate: Arc<Gate>,
    /// Relayed connections that go straight to their peer instead (see [`DirectPath`]).
    directs: RwLock<Vec<Arc<Direct>>>,
    /// The task receiving for quinn, woken when a direct path starts: from then on it must poll
    /// that path's socket too.
    receiving: Mutex<Option<Waker>>,
}

/// A relayed connection going straight to its peer.
#[derive(Debug)]
struct Direct {
    /// The relay session's address: the connection's peer as quinn knows it.
    relay: SocketAddr,
    /// Where the peer is, reached from a socket of this path's own.
    peer: SocketAddr,
    socket: Arc<dyn AsyncUdpSocket>,
    /// Datagrams that came from the peer this way.
    heard: AtomicU64,
}

impl Direct {
    fn send(&self, transmit: &Transmit) -> io::Result<()> {
        let sent = self.socket.try_send(&Transmit {
            destination: self.peer,
            ecn: transmit.ecn,
            contents: transmit.contents,
            segment_size: transmit.segment_size,
            src_ip: None,
        });
        match sent {
            // quinn would wait for room on the endpoint's socket, not this one: lost on the way.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
            result => result,
        }
    }

    /// Hands quinn what came from the peer, as from the relay session; drops anything else.
    fn received(&self, meta: &mut RecvMeta) {
        if canonical(meta.addr) == self.peer {
            meta.addr = self.relay;
            self.heard.fetch_add(1, Relaxed);
        } else {
            meta.len = 0;
        }
    }
}

/// A relayed connection going straight to its peer, from [`Network::go_direct`](crate::endpoint::Network::go_direct),
/// until this is dropped: then it goes through the relay again (if the session still runs).
#[derive(Debug)]
pub struct DirectPath {
    socket: Arc<GatedSocket>,
    direct: Arc<Direct>,
}

impl DirectPath {
    /// Where the peer is reached.
    pub fn peer(&self) -> SocketAddr {
        self.direct.peer
    }

    /// Datagrams that came from the peer this way so far.
    pub fn heard(&self) -> u64 {
        self.direct.heard.load(Relaxed)
    }
}

impl Drop for DirectPath {
    fn drop(&mut self) {
        self.socket.directs.write().unwrap().retain(|d| !Arc::ptr_eq(d, &self.direct));
    }
}

impl GatedSocket {
    pub(crate) fn new(inner: Arc<dyn AsyncUdpSocket>, gate: Arc<Gate>) -> Self {
        gate.socket_v6.store(inner.local_addr().is_ok_and(|addr| addr.is_ipv6()), Relaxed);
        Self { inner, gate, directs: RwLock::default(), receiving: Mutex::default() }
    }

    /// Sends what quinn sends to relay session address `relay` to `peer` from `socket` instead,
    /// and hands quinn what comes from `peer` there as from `relay` (see [`DirectPath`]).
    pub(crate) fn go_direct(self: &Arc<Self>, relay: SocketAddr, peer: SocketAddr, socket: Arc<dyn AsyncUdpSocket>) -> DirectPath {
        let direct = Arc::new(Direct { relay, peer: canonical(peer), socket, heard: AtomicU64::new(0) });
        {
            let mut directs = self.directs.write().unwrap();
            directs.retain(|d| d.relay != relay);
            directs.push(direct.clone());
        }
        if let Some(waker) = self.receiving.lock().unwrap().take() {
            waker.wake();
        }
        DirectPath { socket: self.clone(), direct }
    }

    /// The direct path of the connection with relay session address `relay`, if it has one.
    fn direct(&self, relay: SocketAddr) -> Option<Arc<Direct>> {
        let directs = self.directs.read().unwrap();
        directs.iter().find(|d| d.relay == relay).cloned()
    }

    /// Sends `bytes` to `destination` as one datagram, past quinn: rendezvous control messages
    /// and punches. Waits briefly for room if the socket's send buffer is full. Nothing goes to a
    /// relay session's address (a server can name one as where a viewer is): it is dropped, as if
    /// lost on the way.
    pub(crate) async fn send_raw(&self, destination: SocketAddr, bytes: &[u8]) -> io::Result<()> {
        if relay_ip(destination).is_some() {
            return Ok(());
        }
        let destination = in_family(destination, self.inner.local_addr()?.is_ipv6());
        let transmit = Transmit { destination, ecn: None, contents: bytes, segment_size: None, src_ip: None };
        let mut poller = None;
        for _ in 0..RAW_SEND_ATTEMPTS {
            match self.inner.try_send(&transmit) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let poller = poller.get_or_insert_with(|| self.inner.clone().create_io_poller());
                    let writable = std::future::poll_fn(|cx| poller.as_mut().poll_writable(cx));
                    let _ = tokio::time::timeout(RAW_SEND_WAIT, writable).await;
                }
                result => return result,
            }
        }
        Err(io::ErrorKind::WouldBlock.into())
    }

    /// Sends quinn's datagrams for relay session `sid` to its server, each wrapped, still in one
    /// batch.
    fn send_relayed(&self, server: SocketAddr, sid: &SessionId, transmit: &Transmit) -> io::Result<()> {
        let (contents, segment_size) = rendezvous::wrap_segments(sid, transmit.contents, transmit.segment_size);
        self.inner.try_send(&Transmit {
            destination: in_family(server, transmit.destination.is_ipv6()),
            // Its marks wouldn't reach the peer: the server sends its own datagrams on.
            ecn: None,
            contents: &contents,
            segment_size,
            src_ip: transmit.src_ip,
        })
    }
}

impl AsyncUdpSocket for GatedSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        if relay_ip(transmit.destination).is_some()
            && let Some(direct) = self.direct(transmit.destination)
        {
            return direct.send(transmit);
        }
        // Even without a server: quinn keeps a relayed connection after its session has ended
        // (internet access turned off, say), and a router would send its packets on toward
        // 240.0.0.0/4.
        match self.gate.route(transmit.destination) {
            Route::Direct => self.inner.try_send(transmit),
            Route::Relay(server, sid) => self.send_relayed(server, &sid, transmit),
            Route::Nowhere => Ok(()),
        }
    }

    /// Drops a denied datagram by setting its length to 0, which quinn skips without copying
    /// anything. Returns after one receive even if all of it was dropped, so quinn's limit on
    /// time spent receiving still holds during a flood.
    fn poll_recv(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        if let Poll::Ready(received) = self.inner.poll_recv(cx, bufs, meta) {
            let n = received?;
            for (meta, buf) in meta[..n].iter_mut().zip(bufs.iter_mut()) {
                if !self.gate.accept_received(buf, meta) {
                    meta.len = 0;
                }
            }
            return Poll::Ready(Ok(n));
        }
        {
            let mut receiving = self.receiving.lock().unwrap();
            if !receiving.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                *receiving = Some(cx.waker().clone());
            }
        }
        for direct in self.directs.read().unwrap().iter() {
            match direct.socket.poll_recv(cx, bufs, meta) {
                Poll::Ready(Ok(n)) => {
                    meta[..n].iter_mut().for_each(|meta| direct.received(meta));
                    return Poll::Ready(Ok(n));
                }
                // The endpoint's socket is what quinn must hear about.
                Poll::Ready(Err(e)) => tracing::debug!(peer = %direct.peer, "receive on a direct path: {e}"),
                Poll::Pending => {}
            }
        }
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    // The trait's defaults for these three would turn off batched sends (sendmsg_x) and MTU
    // discovery.
    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use quinn::ConnectionIdGenerator;

    use super::*;
    use crate::cid::MacCidGenerator;
    use crate::knock::{access_key, knock_cid, random_bytes};
    use crate::rendezvous::{Message, PUNCH_LEN, SID_LEN, punch, wrap};

    const VIEWER: Fingerprint = [7; 32];
    const SECRET: [u8; 32] = [1; 32];
    const INITIAL_BYTE: u8 = 0xc3;
    const HANDSHAKE_BYTE: u8 = 0xe3;
    const RETRY_BYTE: u8 = 0xf0;
    const SHORT_BYTE: u8 = 0x43;

    fn internet() -> SocketAddr {
        "203.0.113.7:4000".parse().unwrap()
    }

    fn gate_with_viewer() -> Arc<Gate> {
        let gate = Gate::new();
        gate.set_keys(Some(vec![(VIEWER, access_key(&SECRET, &VIEWER))]));
        gate
    }

    fn knock() -> [u8; KNOCK_LEN] {
        knock_cid(&access_key(&SECRET, &VIEWER), now_unix())
    }

    /// A long-header packet padded with zeros to `len` bytes. For an Initial, the zero after the
    /// source connection ID says it carries no token.
    fn long_header(first: u8, version: u32, dcid: &[u8], len: usize) -> Vec<u8> {
        let mut p = vec![first];
        p.extend(version.to_be_bytes());
        p.push(dcid.len() as u8);
        p.extend(dcid);
        p.push(8);
        p.extend([9; 8]);
        p.resize(len.max(p.len()), 0);
        p
    }

    /// An Initial to `dcid` carrying a token, as a client sends after a Retry.
    fn initial_with_token(dcid: &[u8]) -> Vec<u8> {
        let mut p = long_header(INITIAL_BYTE, QUIC_V1, dcid, 0);
        // A 2-byte varint: 69 bytes of token, about the size of quinn's Retry tokens.
        p.extend([0x40, 69]);
        p.extend([4; 69]);
        p.resize(1200, 0);
        p
    }

    fn our_cid(gate: &Gate) -> quinn::ConnectionId {
        MacCidGenerator::new(gate.cid_key()).generate_cid()
    }

    #[test]
    fn loopback_counts_as_internet_only_when_switched_on() {
        let gate = Gate::new();
        let lan: [SocketAddr; 3] = ["127.0.0.1:1".parse().unwrap(), "[::ffff:127.0.0.1]:1".parse().unwrap(), "192.168.1.20:1".parse().unwrap()];
        assert!(lan.iter().all(|&a| !gate.is_internet(a)));
        assert!(gate.is_internet(internet()));
        gate.set_loopback_is_internet(true);
        assert!(gate.is_internet(lan[0]) && gate.is_internet(lan[1]) && !gate.is_internet(lan[2]));
    }

    #[test]
    fn local_network_always_passes() {
        let gate = Gate::new();
        let lan: SocketAddr = "192.168.1.20:4000".parse().unwrap();
        for packet in [
            vec![],
            vec![0xff],
            long_header(INITIAL_BYTE, 0x1a2a_3a4a, &[1; 8], 1200),
            long_header(INITIAL_BYTE, QUIC_V1, &random_bytes::<20>(), 1200),
            long_header(INITIAL_BYTE, QUIC_V1, &[1; 4], 1200),
        ] {
            assert!(gate.allow_datagram(&packet, lan));
        }
        assert_eq!(gate.stats(), GateStats::default());
    }

    #[test]
    fn internet_short_headers_pass() {
        let gate = Gate::new();
        let mut packet = vec![SHORT_BYTE];
        packet.extend(random_bytes::<40>());
        assert!(gate.allow_datagram(&packet, internet()));
        packet[1..9].copy_from_slice(&our_cid(&gate));
        assert!(gate.allow_datagram(&packet, internet()));
    }

    #[test]
    fn other_versions_are_dropped() {
        let gate = gate_with_viewer();
        // A scanner's Version Negotiation probe, a Version Negotiation packet, and a QUIC draft.
        for version in [0x1a2a_3a4a, 0, 0xff00_001d] {
            assert!(!gate.allow_datagram(&long_header(INITIAL_BYTE, version, &knock(), 1200), internet()));
        }
        assert_eq!(gate.stats().ignored, 3);
    }

    #[test]
    fn initial_without_a_knock_is_dropped() {
        let gate = gate_with_viewer();
        assert!(!gate.allow_datagram(&long_header(INITIAL_BYTE, QUIC_V1, &random_bytes::<20>(), 1200), internet()));
        assert!(!gate.allow_datagram(&long_header(INITIAL_BYTE, QUIC_V1, &random_bytes::<8>(), 1200), internet()));
        assert!(!gate.allow_datagram(&long_header(INITIAL_BYTE, QUIC_V1, &[], 1200), internet()));
        let wrong_key = knock_cid(&access_key(&[2; 32], &VIEWER), now_unix());
        assert!(!gate.allow_datagram(&long_header(INITIAL_BYTE, QUIC_V1, &wrong_key, 1200), internet()));
        assert_eq!(gate.stats(), GateStats { ignored: 4, ..Default::default() });
    }

    #[test]
    fn knocked_initial_passes_while_the_key_is_current() {
        let gate = gate_with_viewer();
        let knocked = long_header(INITIAL_BYTE, QUIC_V1, &knock(), 1200);
        assert!(gate.allow_datagram(&knocked, internet()));
        // The client resends it until the host answers.
        assert!(gate.allow_datagram(&knocked, internet()));
        assert_eq!(gate.stats(), GateStats { admitted: 1, ..Default::default() });

        // Too short to be a client's first datagram, or not an Initial.
        let mut short = knocked.clone();
        short.truncate(1199);
        assert!(!gate.allow_datagram(&short, internet()));
        assert!(!gate.allow_datagram(&long_header(HANDSHAKE_BYTE, QUIC_V1, &knock(), 1200), internet()));

        // Forgetting the viewer, or turning internet access off, shuts it out.
        gate.set_keys(Some(vec![([8; 32], access_key(&SECRET, &[8; 32]))]));
        assert!(!gate.allow_datagram(&long_header(INITIAL_BYTE, QUIC_V1, &knock(), 1200), internet()));
        gate.set_keys(None);
        assert!(!gate.allow_datagram(&long_header(INITIAL_BYTE, QUIC_V1, &knock(), 1200), internet()));
        assert_eq!(gate.stats(), GateStats { admitted: 1, ignored: 4, ..Default::default() });
    }

    #[test]
    fn packets_to_our_connection_ids_pass() {
        let gate = Gate::new();
        let ours = our_cid(&gate);
        for first in [HANDSHAKE_BYTE, INITIAL_BYTE, RETRY_BYTE] {
            assert!(gate.allow_datagram(&long_header(first, QUIC_V1, &ours, 100), internet()));
        }
        assert!(!gate.allow_datagram(&long_header(HANDSHAKE_BYTE, QUIC_V1, &random_bytes::<8>(), 100), internet()));
        let other_endpoint = Gate::new();
        assert!(!gate.allow_datagram(&long_header(HANDSHAKE_BYTE, QUIC_V1, &our_cid(&other_endpoint), 100), internet()));
    }

    #[test]
    fn initials_to_our_ids_with_a_token_pass_only_from_an_address_that_knocked() {
        let gate = gate_with_viewer();
        let ours = our_cid(&gate);
        let knocker = internet();
        let stranger: SocketAddr = "198.51.100.9:4000".parse().unwrap();
        let after_retry = initial_with_token(&ours);
        assert!(!gate.allow_datagram(&after_retry, knocker));
        assert!(!gate.allow_datagram(&after_retry, stranger));
        assert_eq!(gate.stats().ignored, 2);

        assert_eq!(gate.admit(&knock(), knocker), Some(VIEWER));
        assert!(gate.allow_datagram(&after_retry, knocker));
        assert!(!gate.allow_datagram(&after_retry, stranger));
        // A token length cut off by the end of the datagram counts as a token.
        assert!(!gate.allow_datagram(&after_retry[..6 + ours.len() + 1 + 8 + 1], stranger));
        assert!(!gate.allow_datagram(&after_retry[..6 + ours.len()], stranger));
        assert!(gate.allow_datagram(&after_retry[..6 + ours.len() + 1 + 8 + 1], knocker));
        // A server's Initial to our client, which never carries a token, passes from anywhere.
        assert!(gate.allow_datagram(&long_header(INITIAL_BYTE, QUIC_V1, &ours, 1200), stranger));
        assert_eq!(gate.stats(), GateStats { admitted: 1, ignored: 5, ..Default::default() });
    }

    #[test]
    fn token_length_is_read_as_a_varint() {
        // Source connection ID of length 2, then the token length.
        for (token_len, has_token) in [
            (&[0x00][..], Some(false)),
            (&[0x05], Some(true)),
            (&[0x40, 0x00], Some(false)),
            (&[0x40, 0x45], Some(true)),
            (&[0x80, 0, 0, 0], Some(false)),
            (&[0x80, 0, 1, 0], Some(true)),
            (&[0xc0, 0, 0, 0, 0, 0, 0, 0], Some(false)),
            (&[0xc0, 0, 0, 0, 0, 0, 0, 1], Some(true)),
            (&[0x40], None),
            (&[], None),
        ] {
            assert_eq!(initial_has_token(&[&[2, 9, 9][..], token_len].concat()), has_token, "{token_len:02x?}");
        }
        assert_eq!(initial_has_token(&[2, 9]), None);
        assert_eq!(initial_has_token(&[]), None);
    }

    #[test]
    fn truncated_packets_are_dropped() {
        let gate = gate_with_viewer();
        let ours = our_cid(&gate);
        let whole = long_header(HANDSHAKE_BYTE, QUIC_V1, &ours, 100);
        for len in [0, 1, 5, 6, 13] {
            assert!(!gate.allow_datagram(&whole[..len], internet()), "{len} bytes");
        }
        assert!(gate.allow_datagram(&whole[..14], internet()));
        // A connection ID longer than QUIC allows.
        let mut too_long = long_header(INITIAL_BYTE, QUIC_V1, &[0; 21], 1200);
        assert!(!gate.allow_datagram(&too_long, internet()));
        too_long[5] = 255;
        assert!(!gate.allow_datagram(&too_long, internet()));
    }

    #[test]
    fn gro_batch_passes_only_if_every_datagram_does() {
        let gate = Gate::new();
        let ours = long_header(HANDSHAKE_BYTE, QUIC_V1, &our_cid(&gate), 100);
        let theirs = long_header(HANDSHAKE_BYTE, QUIC_V1, &random_bytes::<8>(), 100);
        assert!(gate.allow_received(&[ours.clone(), ours.clone(), ours[..60].to_vec()].concat(), 100, internet()));
        assert!(!gate.allow_received(&[ours.clone(), theirs.clone()].concat(), 100, internet()));
        assert!(gate.allow_received(&[ours, theirs].concat(), 100, "10.0.0.2:1".parse().unwrap()));
    }

    #[test]
    fn admit_accepts_retransmits_but_not_replays() {
        let gate = gate_with_viewer();
        let knock = knock();
        assert_eq!(gate.admit(&knock, internet()), Some(VIEWER));
        assert_eq!(gate.admit(&knock, internet()), Some(VIEWER));
        assert_eq!(gate.admit(&knock, "198.51.100.9:4000".parse().unwrap()), None);
        assert_eq!(gate.admit(&knock, internet()), Some(VIEWER));
        assert_eq!(gate.stats(), GateStats { admitted: 1, ignored: 1, replayed: 1, throttled: 0 });
    }

    #[test]
    fn admit_rejects_stale_knocks() {
        let gate = gate_with_viewer();
        let now = now_unix();
        let made_then = knock_cid(&access_key(&SECRET, &VIEWER), now - 3600);
        assert_eq!(gate.admit_at(&made_then, internet(), Instant::now(), now), None);
        assert_eq!(gate.stats().ignored, 1);
    }

    #[test]
    fn admit_stops_checking_when_the_budget_runs_out() {
        let gate = gate_with_viewer();
        let t0 = Instant::now();
        let mut checked = 0;
        while gate.admit_at(&random_bytes::<KNOCK_LEN>(), internet(), t0, now_unix()).is_none() && gate.stats().throttled == 0 {
            checked += 1;
        }
        assert_eq!(checked, 2000);
        // Even a valid knock waits for the budget to refill.
        let knock = knock();
        assert_eq!(gate.admit_at(&knock, internet(), t0, now_unix()), None);
        assert_eq!(gate.stats().throttled, 2);
        assert_eq!(gate.admit_at(&knock, internet(), t0 + Duration::from_millis(1), now_unix()), Some(VIEWER));
        // Lengths that can't be a knock cost nothing.
        assert_eq!(gate.admit_at(&[0; 8], internet(), t0, now_unix()), None);
        assert_eq!(gate.stats().throttled, 2);
    }

    #[tokio::test]
    async fn gated_socket_drops_denied_datagrams_in_place() {
        let gate = Gate::new();
        gate.set_loopback_is_internet(true);
        let std_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = std_socket.local_addr().unwrap();
        let inner = quinn::default_runtime().unwrap().wrap_udp_socket(std_socket).unwrap();
        let socket = GatedSocket::new(inner.clone(), gate.clone());
        assert_eq!(socket.local_addr().unwrap(), addr);
        assert_eq!(socket.max_transmit_segments(), inner.max_transmit_segments());
        assert_eq!(socket.max_receive_segments(), inner.max_receive_segments());
        assert_eq!(socket.may_fragment(), inner.may_fragment());

        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(&long_header(INITIAL_BYTE, 0x1a2a_3a4a, &[1; 8], 1200), addr).unwrap();
        sender.send_to(&[SHORT_BYTE, 1, 2, 3], addr).unwrap();
        let mut storage = vec![[0u8; 2048]; 4];
        let mut lens = Vec::new();
        while lens.len() < 2 {
            let mut bufs: Vec<IoSliceMut> = storage.iter_mut().map(|b| IoSliceMut::new(b)).collect();
            let mut meta = [RecvMeta::default(); 4];
            let n = std::future::poll_fn(|cx| socket.poll_recv(cx, &mut bufs, &mut meta)).await.unwrap();
            lens.extend(meta[..n].iter().map(|m| m.len));
        }
        assert_eq!(lens, [0, 4]);
        assert_eq!(gate.stats().ignored, 1);
    }

    fn server() -> SocketAddr {
        "198.51.100.1:3478".parse().unwrap()
    }

    /// One receive of `datagrams` (`stride` bytes apart) from `from`, through the gate: what quinn
    /// gets, if anything, with its stride and source.
    fn receive_batch(gate: &Gate, datagrams: &[u8], stride: usize, from: SocketAddr) -> Option<(Vec<u8>, usize, SocketAddr)> {
        // Receive buffers are larger than what arrives.
        let mut buf = [datagrams, &[0xee; 64]].concat();
        let mut meta = RecvMeta { addr: from, len: datagrams.len(), stride, ..RecvMeta::default() };
        gate.accept_received(&mut buf, &mut meta).then(|| (buf[..meta.len].to_vec(), meta.stride, meta.addr))
    }

    fn receive(gate: &Gate, datagram: &[u8], from: SocketAddr) -> Option<(Vec<u8>, SocketAddr)> {
        let (data, stride, from) = receive_batch(gate, datagram, datagram.len(), from)?;
        assert_eq!(stride, data.len());
        Some((data, from))
    }

    #[test]
    fn without_a_server_rendezvous_traffic_is_just_datagrams() {
        let gate = Gate::new();
        let mut control = gate.take_control_receiver();
        let message = Message::Unknown { nonce: [1; 16] }.encode();
        let punch = punch(&[1; 16]);
        // "LK…" reads as a short header, which passes; a relayed packet as a long header of some
        // other QUIC version, which doesn't.
        assert_eq!(receive(&gate, &message, server()), Some((message.clone(), server())));
        assert_eq!(receive(&gate, &punch, internet()), Some((punch.to_vec(), internet())));
        assert_eq!(receive(&gate, &wrap(&[1; SID_LEN], &[SHORT_BYTE; 30]), server()), None);
        assert!(control.try_recv().is_err());
    }

    #[test]
    fn control_messages_from_a_server_go_to_the_core() {
        let gate = gate_with_viewer();
        gate.set_servers(vec![server()]);
        let message = Message::Unknown { nonce: [1; 16] }.encode();
        // Nobody listening yet.
        assert_eq!(receive(&gate, &message, server()), None);
        let mut control = gate.take_control_receiver();
        assert_eq!(receive(&gate, &message, server()), None);
        assert_eq!(control.try_recv().unwrap(), (server(), message.clone()));
        // A dual-stack socket sees the server's address IPv4-mapped.
        assert_eq!(receive(&gate, &message, "[::ffff:198.51.100.1]:3478".parse().unwrap()), None);
        assert_eq!(control.try_recv().unwrap(), (server(), message.clone()));
        // From another port, it is no server's: a short header.
        assert!(receive(&gate, &message, "198.51.100.1:3479".parse().unwrap()).is_some());
        // Anything else from a server is dropped, even what would pass the filter.
        for other in [vec![], vec![SHORT_BYTE; 40], long_header(INITIAL_BYTE, QUIC_V1, &knock(), 1200)] {
            assert_eq!(receive(&gate, &other, server()), None);
        }
        assert!(control.try_recv().is_err());
        assert_eq!(gate.stats(), GateStats::default());

        // A new receiver takes over, and messages it hasn't read yet are capped.
        let mut newer = gate.take_control_receiver();
        for _ in 0..CONTROL_QUEUE + 10 {
            receive(&gate, &message, server());
        }
        assert!(control.try_recv().is_err());
        let mut queued = 0;
        while newer.try_recv().is_ok() {
            queued += 1;
        }
        assert_eq!(queued, CONTROL_QUEUE);
    }

    #[test]
    fn relayed_packets_are_unwrapped_and_then_filtered() {
        let gate = gate_with_viewer();
        gate.set_servers(vec![server()]);
        let sid = [3; SID_LEN];
        let relay = gate.add_relay(server(), sid);
        let knocked = long_header(INITIAL_BYTE, QUIC_V1, &knock(), 1200);
        assert_eq!(receive(&gate, &wrap(&sid, &knocked), server()), Some((knocked.clone(), relay)));
        assert_eq!(gate.stats().admitted, 1);
        let short = [SHORT_BYTE; 50];
        assert_eq!(receive(&gate, &wrap(&sid, &short), server()), Some((short.to_vec(), relay)));
        // Through the relay, a stranger's Initial is still a stranger's.
        let unknocked = long_header(INITIAL_BYTE, QUIC_V1, &random_bytes::<20>(), 1200);
        assert_eq!(receive(&gate, &wrap(&sid, &unknocked), server()), None);
        assert_eq!(gate.stats().ignored, 1);
        // A session that isn't running, an empty packet, or wrapped by someone who isn't a server.
        assert_eq!(receive(&gate, &wrap(&[4; SID_LEN], &short), server()), None);
        assert_eq!(receive(&gate, &wrap(&sid, &[]), server()), None);
        assert_eq!(receive(&gate, &wrap(&sid, &short), internet()), None);
        assert_eq!(gate.stats().ignored, 2);
    }

    #[test]
    fn on_a_dual_stack_socket_relay_addresses_are_ipv4_mapped() {
        let gate = Gate::new();
        gate.socket_v6.store(true, Relaxed);
        let mapped_server: SocketAddr = "[::ffff:198.51.100.1]:3478".parse().unwrap();
        gate.set_servers(vec![mapped_server]);
        assert_eq!(gate.servers(), [server()]);
        // What the gate hands out is what quinn then reports as the connection's remote address.
        let relay = gate.add_relay(server(), [3; SID_LEN]);
        assert_eq!(relay, "[::ffff:240.0.0.1]:1".parse().unwrap());
        assert_eq!(gate.add_relay(mapped_server, [3; SID_LEN]), relay);
        let (_, from) = receive(&gate, &wrap(&[3; SID_LEN], &[SHORT_BYTE; 30]), mapped_server).unwrap();
        assert_eq!(from, relay);
        assert!(gate.is_relayed(from));
        assert!(matches!(gate.route(from), Route::Relay(s, [3, ..]) if s == server()));
        assert_eq!(gate.remove_relay(server(), &[3; SID_LEN]), Some(relay));
    }

    /// A socket that only notes where datagrams are sent.
    #[derive(Debug)]
    struct Recorder {
        local: SocketAddr,
        sent: Mutex<Vec<SocketAddr>>,
    }

    impl AsyncUdpSocket for Recorder {
        fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
            unimplemented!("sends never block")
        }

        fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
            self.sent.lock().unwrap().push(transmit.destination);
            Ok(())
        }

        fn poll_recv(&self, _: &mut Context, _: &mut [IoSliceMut<'_>], _: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
            Poll::Pending
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.local)
        }
    }

    #[tokio::test]
    async fn relay_addresses_never_reach_the_network() {
        let gate = Gate::new();
        let recorder = Arc::new(Recorder { local: "0.0.0.0:47800".parse().unwrap(), sent: Mutex::default() });
        let socket = GatedSocket::new(recorder.clone(), gate.clone());
        let relay = gate.add_relay(server(), [1; SID_LEN]);
        let send = |destination: SocketAddr| {
            let transmit = Transmit { destination, ecn: None, contents: &[SHORT_BYTE; 30], segment_size: None, src_ip: None };
            socket.try_send(&transmit).unwrap();
        };
        send(relay);
        // With no server left (internet access turned off, say), quinn still has the connection
        // through the session for a while: its packets go nowhere, not to 240.0.0.0/4.
        gate.set_servers(vec![]);
        for destination in [relay, "[::ffff:240.0.0.1]:1".parse().unwrap(), "240.0.0.9:1".parse().unwrap()] {
            send(destination);
        }
        // Nor do punches or control messages, to an address a server named, say.
        socket.send_raw(relay, &punch(&[1; 16])).await.unwrap();
        gate.add_relay(server(), [2; SID_LEN]);
        socket.send_raw("255.255.255.254:9".parse().unwrap(), b"LKR1").await.unwrap();
        send(internet());
        assert_eq!(*recorder.sent.lock().unwrap(), [server(), internet()]);
    }

    #[test]
    fn relay_sessions_get_addresses_of_their_own() {
        let gate = Gate::new();
        let other_server: SocketAddr = "192.0.2.5:3478".parse().unwrap();
        let a = gate.add_relay(server(), [1; SID_LEN]);
        assert_eq!(gate.servers(), [server()]);
        let b = gate.add_relay(server(), [2; SID_LEN]);
        let c = gate.add_relay(other_server, [1; SID_LEN]);
        assert_eq!(gate.servers(), [server(), other_server]);
        assert_eq!(gate.add_relay(server(), [1; SID_LEN]), a);
        assert_eq!([a, b, c], ["240.0.0.1:1", "240.0.0.2:1", "240.0.0.3:1"].map(|s| s.parse().unwrap()));
        for addr in [a, b, c] {
            assert!(gate.is_relayed(addr) && gate.is_internet(addr), "{addr}");
        }
        for addr in ["255.255.255.254:9", "[::ffff:240.0.0.1]:1"] {
            assert!(gate.is_relayed(addr.parse().unwrap()), "{addr}");
        }
        for addr in [internet(), "127.0.0.1:1".parse().unwrap(), "239.255.255.255:1".parse().unwrap(), "[f000::1]:1".parse().unwrap()] {
            assert!(!gate.is_relayed(addr), "{addr}");
            assert!(matches!(gate.route(addr), Route::Direct));
        }
        assert!(matches!(gate.route(b), Route::Relay(s, [2, ..]) if s == server()));
        assert!(matches!(gate.route("240.0.0.9:1".parse().unwrap()), Route::Nowhere));

        assert_eq!(gate.remove_relay(server(), &[2; SID_LEN]), Some(b));
        assert_eq!(gate.remove_relay(server(), &[2; SID_LEN]), None);
        assert!(matches!(gate.route(b), Route::Nowhere));
        // Dropping a server ends its sessions; a new session never reuses a running one's address.
        gate.set_servers(vec![other_server]);
        assert_eq!(gate.remove_relay(server(), &[1; SID_LEN]), None);
        assert_eq!(gate.add_relay(other_server, [9; SID_LEN]), "240.0.0.4:1".parse().unwrap());
        assert_eq!(gate.remove_relay(other_server, &[1; SID_LEN]), Some(c));
        gate.set_servers(vec![]);
        assert!(gate.servers().is_empty() && !gate.any_server.load(Relaxed));
        assert!(gate.relays.read().unwrap().by_address.is_empty());
    }

    #[test]
    fn idle_relay_sessions_expire() {
        let mut relays = Relays::default();
        let idle = RELAY_IDLE.as_millis() as u64;
        let ip = relays.add(server(), [1; SID_LEN], 0);
        assert_eq!(relays.address(server(), &[1; SID_LEN], idle - 1), Some(ip));
        // Every packet keeps it going.
        assert_eq!(relays.session(ip, 2 * idle - 2), Some((server(), [1; SID_LEN])));
        assert_eq!(relays.address(server(), &[1; SID_LEN], 3 * idle - 2), None);
        assert_eq!(relays.session(ip, 3 * idle), None);
        // Forgotten when the next session starts.
        let next = relays.add(server(), [2; SID_LEN], 3 * idle);
        assert_ne!(next, ip);
        assert_eq!((relays.by_address.len(), relays.by_session.len()), (1, 1));
    }

    #[test]
    fn gro_batch_from_a_server_keeps_one_sessions_packets() {
        let gate = Gate::new();
        gate.set_servers(vec![server()]);
        let mut control = gate.take_control_receiver();
        let (a, b) = ([1; SID_LEN], [2; SID_LEN]);
        let relay_a = gate.add_relay(server(), a);
        gate.add_relay(server(), b);
        let packet = |n: u8| [&[SHORT_BYTE, n][..], &[n; 19]].concat();
        let message = Message::Unknown { nonce: [9; 16] }.encode();
        // 30 bytes apart: session a, session b, session a, and a (shorter) control message.
        let batch = [wrap(&a, &packet(1)), wrap(&b, &packet(2)), wrap(&a, &packet(3)), message.clone()].concat();
        let (data, stride, from) = receive_batch(&gate, &batch, 30, server()).unwrap();
        assert_eq!((data, stride, from), ([packet(1), packet(3)].concat(), 21, relay_a));
        assert_eq!(control.try_recv().unwrap(), (server(), message.clone()));
        // A shorter last packet stays last.
        let batch = [wrap(&a, &packet(1)), wrap(&a, &packet(2)[..10])].concat();
        let (data, stride, _) = receive_batch(&gate, &batch, 30, server()).unwrap();
        assert_eq!((data, stride), ([&packet(1)[..], &packet(2)[..10]].concat(), 21));
        assert_eq!(receive_batch(&gate, &[message.clone(), message].concat(), 21, server()), None);
    }

    #[test]
    fn punches_from_the_internet_are_dropped() {
        let gate = Gate::new();
        gate.set_servers(vec![server()]);
        let punch = punch(&[1; 16]);
        assert_eq!(receive(&gate, &punch, internet()), None);
        assert_eq!(receive_batch(&gate, &[punch, punch].concat(), PUNCH_LEN, internet()), None);
        // The local network is left alone, and so is anything that isn't quite a punch.
        assert!(receive(&gate, &punch, "192.168.1.20:4000".parse().unwrap()).is_some());
        assert!(receive(&gate, &punch[..PUNCH_LEN - 1], internet()).is_some());
        assert_eq!(gate.stats(), GateStats::default());
    }

    #[test]
    fn rendezvous_tokens_name_the_viewer_once_per_address() {
        let gate = gate_with_viewer();
        let key = access_key(&SECRET, &VIEWER);
        let (id, now, viewer) = ([2; 16], now_unix(), internet());
        let nonce = random_bytes::<16>();
        let token = rendezvous::token(&key, &id, &nonce, now);
        let first = ValidRequest { viewer: VIEWER, first: true };
        let again = ValidRequest { first: false, ..first };
        assert_eq!(gate.check_token(&id, &nonce, &token, viewer, now), Some(first));
        // The server passes the request on again when the viewer resends it, and so can anyone
        // who copied it on its way to the host: it is answered, but marked, so the host doesn't
        // punch again. A copy sent from somewhere else is a replay.
        assert_eq!(gate.check_token(&id, &nonce, &token, viewer, now), Some(again));
        assert_eq!(gate.check_token(&id, &nonce, &token, "198.51.100.9:4000".parse().unwrap(), now), None);
        assert_eq!(gate.check_token(&id, &nonce, &token, "[::ffff:203.0.113.7]:4000".parse().unwrap(), now), Some(again));

        for (skew, valid) in [(-600, true), (600, true), (-1200, false), (1200, false)] {
            let nonce = random_bytes::<16>();
            let made = rendezvous::token(&key, &id, &nonce, now.checked_add_signed(skew).unwrap());
            assert_eq!(gate.check_token(&id, &nonce, &made, viewer, now).is_some(), valid, "skew {skew} s");
        }

        // For another host, made with another host's key, from a forgotten viewer, or while
        // internet access is off.
        let nonce = random_bytes::<16>();
        let token = rendezvous::token(&key, &id, &nonce, now);
        assert_eq!(gate.check_token(&[3; 16], &nonce, &token, viewer, now), None);
        let foreign = rendezvous::token(&access_key(&[2; 32], &VIEWER), &id, &nonce, now);
        assert_eq!(gate.check_token(&id, &nonce, &foreign, viewer, now), None);
        gate.set_keys(Some(vec![([8; 32], access_key(&SECRET, &[8; 32]))]));
        assert_eq!(gate.check_token(&id, &nonce, &token, viewer, now), None);
        gate.set_keys(None);
        assert_eq!(gate.check_token(&id, &nonce, &token, viewer, now), None);
        // Knocks are counted on their own.
        assert_eq!(gate.stats(), GateStats::default());
    }

    #[test]
    fn token_checks_stop_when_the_budget_runs_out() {
        let gate = gate_with_viewer();
        let (id, now, t0) = ([2; 16], now_unix(), Instant::now());
        for _ in 0..2000 {
            assert_eq!(gate.check_token_at(&id, &random_bytes(), &[0; 16], internet(), t0, now), None);
        }
        let nonce = random_bytes::<16>();
        let token = rendezvous::token(&access_key(&SECRET, &VIEWER), &id, &nonce, now);
        assert_eq!(gate.check_token_at(&id, &nonce, &token, internet(), t0, now), None);
        assert_eq!(
            gate.check_token_at(&id, &nonce, &token, internet(), t0 + Duration::from_millis(1), now),
            Some(ValidRequest { viewer: VIEWER, first: true })
        );
    }

    #[tokio::test]
    async fn gated_socket_sends_to_relay_sessions_through_their_server() {
        let gate = Gate::new();
        let inner = quinn::default_runtime().unwrap().wrap_udp_socket(std::net::UdpSocket::bind("127.0.0.1:0").unwrap()).unwrap();
        let socket = GatedSocket::new(inner, gate.clone());
        let server = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        server.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let server_addr = server.local_addr().unwrap();
        let sid = [7; SID_LEN];
        let relay = gate.add_relay(server_addr, sid);
        let mut buf = [0u8; 2048];
        let mut next = || {
            let (len, from) = server.recv_from(&mut buf).unwrap();
            assert_eq!(from, socket.local_addr().unwrap());
            buf[..len].to_vec()
        };

        // Waits until the new socket is known to be writable, which quinn's own sends do too.
        socket.send_raw(server_addr, b"LKR1 and the rest").await.unwrap();
        assert_eq!(next(), b"LKR1 and the rest");

        // Three datagrams in one batch, where the platform batches sends.
        let contents: Vec<u8> = (0..25).collect();
        let segment_size = (socket.max_transmit_segments() >= 3).then_some(10);
        let transmit = Transmit { destination: relay, ecn: None, contents: &contents, segment_size, src_ip: None };
        socket.try_send(&transmit).unwrap();
        for packet in contents.chunks(segment_size.unwrap_or(contents.len())) {
            assert_eq!(next(), wrap(&sid, packet));
        }
        // To a session that has ended: gone without an error. Elsewhere: as it is.
        for destination in ["240.0.0.9:1".parse().unwrap(), server_addr] {
            socket.try_send(&Transmit { destination, ecn: None, contents: b"as it is", segment_size: None, src_ip: None }).unwrap();
        }
        assert_eq!(next(), b"as it is");
    }
}
