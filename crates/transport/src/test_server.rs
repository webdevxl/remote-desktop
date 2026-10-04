//! A rendezvous server for LanKVM's own tests: the protocol of the real one (the
//! `lankvm-rendezvous` service), in process on tokio, without its rate limits and caps. It can
//! also lie about where each Mac is, so punching fails and the Macs fall back to its relay.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ring::hmac;
use ring::signature::{ED25519, UnparsedPublicKey};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::knock::{now_unix, random_bytes};
use crate::rendezvous::{self, Cookie, Message, Nonce, RendezvousId, SessionId, encode_endpoint, id_for, register_payload};

const REGISTRATION_TTL: Duration = Duration::from_secs(75);
/// How long a connect request or relay offer waits for the host.
const PENDING_TTL: Duration = Duration::from_secs(30);
const RELAY_IDLE: Duration = Duration::from_secs(60);
const COOKIE_LABEL: &[u8] = b"lankvm cookie v1";
const SWEEP_EVERY: Duration = Duration::from_millis(100);

/// A running server. Stops when dropped.
pub struct TestServer {
    addr: SocketAddr,
    /// Where it says the Macs are when lying: a socket that never answers.
    black_hole: SocketAddr,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestServerStats {
    /// Relayed datagrams passed on to the other end of their session.
    pub relayed: u64,
    /// Relayed datagrams dropped: for no running session, or not from one of its two ends.
    pub dropped: u64,
}

impl TestServer {
    /// A server on a free port of 127.0.0.1.
    pub async fn start() -> io::Result<Self> {
        Self::bind("127.0.0.1:0".parse().expect("valid address")).await
    }

    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let black_hole = UdpSocket::bind(SocketAddr::new(addr.ip(), 0)).await?;
        let (addr, black_hole_addr) = (socket.local_addr()?, black_hole.local_addr()?);
        let state = Arc::new(Mutex::new(State::new()));
        let task = tokio::spawn(serve(socket, black_hole, state.clone()));
        Ok(Self { addr, black_hole: black_hole_addr, state, task })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Reports an address that never answers instead of each Mac's own, from now on: the Macs
    /// punch and connect to nowhere, and have to fall back to the relay.
    pub fn set_lie_about_endpoints(&self, on: bool) {
        self.state.lock().unwrap().lie = on.then_some(self.black_hole);
    }

    /// How long a relay session may go unused before the server ends it (60 s by default).
    pub fn set_relay_idle(&self, idle: Duration) {
        self.state.lock().unwrap().relay_idle = idle;
    }

    /// The address host `id` registered from, if it is registered.
    pub fn registered(&self, id: &RendezvousId) -> Option<SocketAddr> {
        self.state.lock().unwrap().hosts.get(id).map(|host| host.ep)
    }

    pub fn relay_sessions(&self) -> usize {
        self.state.lock().unwrap().sessions.len()
    }

    pub fn stats(&self) -> TestServerStats {
        self.state.lock().unwrap().stats
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(socket: UdpSocket, _black_hole: UdpSocket, state: Arc<Mutex<State>>) {
    let mut buf = vec![0u8; 65_536];
    let mut sweep = tokio::time::interval(SWEEP_EVERY);
    loop {
        let out = tokio::select! {
            received = socket.recv_from(&mut buf) => match received {
                Ok((len, from)) => state.lock().unwrap().handle(&buf[..len], from, Instant::now()),
                Err(_) => continue,
            },
            _ = sweep.tick() => state.lock().unwrap().sweep(Instant::now()),
        };
        for (to, datagram) in out {
            let _ = socket.send_to(&datagram, to).await;
        }
    }
}

struct State {
    secret: hmac::Key,
    hosts: HashMap<RendezvousId, Host>,
    connects: HashMap<Nonce, Request>,
    offers: HashMap<SessionId, Request>,
    sessions: HashMap<SessionId, Session>,
    lie: Option<SocketAddr>,
    relay_idle: Duration,
    stats: TestServerStats,
}

struct Host {
    ep: SocketAddr,
    seen: Instant,
}

/// A viewer's request waiting for the host.
struct Request {
    id: RendezvousId,
    nonce: Nonce,
    viewer: SocketAddr,
    at: Instant,
}

struct Session {
    host: SocketAddr,
    viewer: SocketAddr,
    active: Instant,
}

type Out = Vec<(SocketAddr, Vec<u8>)>;

impl State {
    fn new() -> Self {
        Self {
            secret: hmac::Key::new(hmac::HMAC_SHA256, &random_bytes::<32>()),
            hosts: HashMap::new(),
            connects: HashMap::new(),
            offers: HashMap::new(),
            sessions: HashMap::new(),
            lie: None,
            relay_idle: RELAY_IDLE,
            stats: TestServerStats::default(),
        }
    }

    fn handle(&mut self, datagram: &[u8], from: SocketAddr, now: Instant) -> Out {
        if let Some((sid, _)) = rendezvous::unwrap(datagram) {
            return self.relay(sid, datagram, from, now).into_iter().collect();
        }
        let Some(message) = Message::decode(datagram) else { return vec![] };
        let reply = |message: Message| vec![(from, message.encode())];
        match message {
            Message::RegisterBegin { .. } => reply(Message::Challenge { cookie: self.cookie(from, now_unix() / 60) }),
            Message::Register { public_key, cookie, signature } => {
                let minute = now_unix() / 60;
                let fresh = [minute, minute.saturating_sub(1)].into_iter().any(|m| self.cookie(from, m) == cookie);
                let signed =
                    UnparsedPublicKey::new(&ED25519, public_key).verify(&register_payload(&cookie), &signature).is_ok();
                if !fresh || !signed {
                    return vec![];
                }
                self.hosts.insert(id_for(&public_key), Host { ep: from, seen: now });
                reply(Message::Registered { ttl_secs: REGISTRATION_TTL.as_secs() as u16, observed: from })
            }
            Message::Keepalive { id } => match self.hosts.get_mut(&id) {
                Some(host) if host.ep == from => {
                    host.seen = now;
                    reply(Message::Alive { observed: from })
                }
                // Moved (its router gave it a new public address), forgotten, or a stranger.
                _ => reply(Message::Challenge { cookie: self.cookie(from, now_unix() / 60) }),
            },
            Message::Unregister { id } => {
                if self.is_host(&id, from) {
                    self.hosts.remove(&id);
                }
                vec![]
            }
            Message::Connect { id, nonce, token } => {
                let Some(host) = self.hosts.get(&id).map(|host| host.ep) else { return reply(Message::Unknown { nonce }) };
                self.connects.insert(nonce, Request { id, nonce, viewer: from, at: now });
                vec![(host, Message::Incoming { nonce, token, viewer: self.lie.unwrap_or(from) }.encode())]
            }
            Message::Accept { id, nonce } => {
                if !self.is_host(&id, from) || !self.connects.get(&nonce).is_some_and(|request| request.id == id) {
                    return vec![];
                }
                let request = self.connects.remove(&nonce).expect("checked above");
                vec![(request.viewer, Message::Peer { nonce, host: self.lie.unwrap_or(from) }.encode())]
            }
            Message::RelayRequest { id, nonce, token } => {
                let Some(host) = self.hosts.get(&id).map(|host| host.ep) else { return reply(Message::Unknown { nonce }) };
                // A resent request gets the session offered before.
                let sid = self
                    .offers
                    .iter()
                    .find(|(_, offer)| offer.nonce == nonce && offer.viewer == from)
                    .map_or_else(random_bytes, |(sid, _)| *sid);
                self.offers.insert(sid, Request { id, nonce, viewer: from, at: now });
                vec![(host, Message::RelayOffer { nonce, token, sid, viewer: self.lie.unwrap_or(from) }.encode())]
            }
            Message::RelayAccept { id, sid } => {
                if !self.is_host(&id, from) || !self.offers.get(&sid).is_some_and(|offer| offer.id == id) {
                    return vec![];
                }
                let offer = self.offers.remove(&sid).expect("checked above");
                self.sessions.insert(sid, Session { host: from, viewer: offer.viewer, active: now });
                vec![(offer.viewer, Message::RelayReady { nonce: offer.nonce, sid }.encode())]
            }
            // The server's own messages, sent to it.
            _ => vec![],
        }
    }

    /// Passes a relayed datagram on, as it is, to the other end of its session.
    fn relay(&mut self, sid: SessionId, datagram: &[u8], from: SocketAddr, now: Instant) -> Option<(SocketAddr, Vec<u8>)> {
        let to = self.sessions.get_mut(&sid).and_then(|session| {
            let to = if from == session.host {
                session.viewer
            } else if from == session.viewer {
                session.host
            } else {
                return None;
            };
            session.active = now;
            Some(to)
        });
        match to {
            Some(_) => self.stats.relayed += 1,
            None => self.stats.dropped += 1,
        }
        to.map(|to| (to, datagram.to_vec()))
    }

    fn sweep(&mut self, now: Instant) -> Out {
        self.hosts.retain(|_, host| now.duration_since(host.seen) < REGISTRATION_TTL);
        self.connects.retain(|_, request| now.duration_since(request.at) < PENDING_TTL);
        self.offers.retain(|_, offer| now.duration_since(offer.at) < PENDING_TTL);
        let idle: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, session)| now.duration_since(session.active) >= self.relay_idle)
            .map(|(sid, _)| *sid)
            .collect();
        let mut out = Vec::new();
        for sid in idle {
            let session = self.sessions.remove(&sid).expect("listed above");
            let end = Message::RelayEnd { sid }.encode();
            out.push((session.host, end.clone()));
            out.push((session.viewer, end));
        }
        out
    }

    fn is_host(&self, id: &RendezvousId, from: SocketAddr) -> bool {
        self.hosts.get(id).is_some_and(|host| host.ep == from)
    }

    fn cookie(&self, source: SocketAddr, minute: u64) -> Cookie {
        let mut message = COOKIE_LABEL.to_vec();
        encode_endpoint(&mut message, source);
        message.extend_from_slice(&minute.to_be_bytes());
        hmac::sign(&self.secret, &message).as_ref()[..rendezvous::COOKIE_LEN].try_into().expect("HMAC-SHA256 is longer than a cookie")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rendezvous::RendezvousIdentity;

    fn host_identity() -> RendezvousIdentity {
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        RendezvousIdentity::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    fn one(out: Out) -> (SocketAddr, Message) {
        assert_eq!(out.len(), 1, "{out:?}");
        let (to, bytes) = out.into_iter().next().unwrap();
        (to, Message::decode(&bytes).unwrap())
    }

    fn register(state: &mut State, identity: &RendezvousIdentity, from: SocketAddr, now: Instant) {
        let (_, Message::Challenge { cookie }) = one(state.handle(&Message::RegisterBegin { id: identity.id() }.encode(), from, now)) else {
            panic!("no challenge")
        };
        let (to, registered) = one(state.handle(&identity.register(&cookie).encode(), from, now));
        assert_eq!((to, registered), (from, Message::Registered { ttl_secs: 75, observed: from }));
    }

    #[test]
    fn registration_needs_the_cookie_for_its_address_and_the_key() {
        let mut state = State::new();
        let (host, other) = ("203.0.113.7:47800".parse().unwrap(), "198.51.100.9:47800".parse().unwrap());
        let identity = host_identity();
        let now = Instant::now();
        let (_, Message::Challenge { cookie }) = one(state.handle(&Message::RegisterBegin { id: identity.id() }.encode(), other, now)) else {
            panic!()
        };
        // A cookie for another address, or a signature by another key.
        assert!(state.handle(&identity.register(&cookie).encode(), host, now).is_empty());
        let Message::Register { public_key, cookie, .. } = identity.register(&cookie) else { unreachable!() };
        let forged = Message::Register { public_key, cookie, signature: host_identity().sign(&register_payload(&cookie)) };
        assert!(state.handle(&forged.encode(), other, now).is_empty());
        assert!(state.hosts.is_empty());

        register(&mut state, &identity, host, now);
        assert_eq!(state.hosts[&identity.id()].ep, host);
        let keepalive = Message::Keepalive { id: identity.id() }.encode();
        assert_eq!(one(state.handle(&keepalive, host, now)).1, Message::Alive { observed: host });
        assert!(matches!(one(state.handle(&keepalive, other, now)).1, Message::Challenge { .. }));
        assert!(state.handle(&Message::Unregister { id: identity.id() }.encode(), other, now).is_empty());
        assert!(state.hosts.contains_key(&identity.id()));
        state.handle(&Message::Unregister { id: identity.id() }.encode(), host, now);
        assert!(state.hosts.is_empty());
    }

    #[test]
    fn relays_only_between_the_two_ends() {
        let mut state = State::new();
        let (host, viewer, stranger): (SocketAddr, SocketAddr, SocketAddr) =
            ("203.0.113.7:47800".parse().unwrap(), "198.51.100.9:5000".parse().unwrap(), "192.0.2.1:9".parse().unwrap());
        let identity = host_identity();
        let now = Instant::now();
        register(&mut state, &identity, host, now);
        let request = Message::RelayRequest { id: identity.id(), nonce: [1; 16], token: [2; 16] }.encode();
        let (to, Message::RelayOffer { sid, viewer: offered, .. }) = one(state.handle(&request, viewer, now)) else { panic!() };
        assert_eq!((to, offered), (host, viewer));
        // Resent: the same session.
        let (_, Message::RelayOffer { sid: again, .. }) = one(state.handle(&request, viewer, now)) else { panic!() };
        assert_eq!(again, sid);
        assert!(state.handle(&Message::RelayAccept { id: identity.id(), sid }.encode(), stranger, now).is_empty());
        let ready = one(state.handle(&Message::RelayAccept { id: identity.id(), sid }.encode(), host, now));
        assert_eq!(ready, (viewer, Message::RelayReady { nonce: [1; 16], sid }));

        let data = rendezvous::wrap(&sid, b"packet");
        assert_eq!(state.handle(&data, host, now), [(viewer, data.clone())]);
        assert_eq!(state.handle(&data, viewer, now), [(host, data.clone())]);
        assert!(state.handle(&data, stranger, now).is_empty());
        assert!(state.handle(&rendezvous::wrap(&[0; 8], b"packet"), host, now).is_empty());
        assert_eq!(state.stats, TestServerStats { relayed: 2, dropped: 2 });

        let ended = state.sweep(now + RELAY_IDLE);
        assert_eq!(ended.len(), 2);
        assert!(state.sessions.is_empty());
    }
}
