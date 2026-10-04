//! Rendezvous: how two Macs that both sit behind routers reach each other with nothing to set up
//! on either router.
//!
//! A host with internet access on registers with a small public server from its QUIC socket and
//! keeps the registration alive, which also keeps its router's mapping for that socket open. A
//! paired viewer asks the server to introduce it, with a token made from its access key; only the
//! host checks the token, and a stranger's request goes unanswered. The server tells each side the
//! other's public address, both send a few packets straight at it (punches), which opens both
//! routers, and the viewer then connects as over any internet path, knock included. Where that
//! fails (routers that give each destination its own public port, carrier-grade NAT, strict
//! firewalls), the server relays the QUIC packets instead. They stay end-to-end encrypted with the
//! certificates pinned: the server never sees inside a session and cannot pose as either Mac.
//!
//! This module has the wire format, the tokens, and the host's identity on the server (an Ed25519
//! key, so no one else can take over its registration). Telling server traffic apart from QUIC on
//! the shared socket, and relaying, is the [`Gate`](crate::gate::Gate)'s job.
//!
//! Every control message is [`MAGIC`], a type byte and a fixed-size body; addresses in it are
//! endpoints: family (4 or 6), the address, the port (big-endian). A message with the wrong
//! length, magic or type is dropped.

use std::fmt;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use ring::{digest, hmac};

use crate::identity::{Fingerprint, write_private};
use crate::knock::{AccessKey, KnockKeys, ct_eq};

/// The server Macs use unless the user picks another.
pub const DEFAULT_SERVER: &str = "178.156.129.211:3478";
/// Every control message starts with this.
pub const MAGIC: [u8; 4] = *b"LKR1";
/// A relayed QUIC packet is this byte, the relay session's ID, then the packet.
pub const DATA: u8 = 0xd0;
pub const DATA_HEADER_LEN: usize = 1 + SID_LEN;
/// A punch, sent from Mac to Mac and never to the server, is this and the request's nonce.
pub const PUNCH_MAGIC: [u8; 4] = *b"LKP1";
pub const PUNCH_LEN: usize = PUNCH_MAGIC.len() + NONCE_LEN;
/// The host's key for the server, in the LanKVM data directory.
pub const KEY_FILE: &str = "rendezvous-key.p8";

pub const ID_LEN: usize = 16;
pub const NONCE_LEN: usize = 16;
pub const TOKEN_LEN: usize = 16;
pub const COOKIE_LEN: usize = 16;
pub const SID_LEN: usize = 8;

/// A host's name on the server, derived from its public key.
pub type RendezvousId = [u8; ID_LEN];
/// Names one connect or relay request; random.
pub type Nonce = [u8; NONCE_LEN];
/// Proves a request comes from one of the host's viewers.
pub type Token = [u8; TOKEN_LEN];
/// The server's proof that a registration comes from the address it claims.
pub type Cookie = [u8; COOKIE_LEN];
/// A relay session.
pub type SessionId = [u8; SID_LEN];
pub type PublicKey = [u8; 32];
pub type Signature = [u8; 64];

/// How often a registered host checks in. Routers forget a UDP mapping after 30 s of silence or
/// more, and the server forgets a host it hasn't heard from in 75 s.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
/// When to resend a request that got no answer, counted from the previous send. A connect request
/// uses the first three; once three have gone unanswered the server counts as unreachable.
pub const RESEND_AFTER: [Duration; 4] =
    [Duration::from_millis(500), Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)];
/// The longest wait between attempts to reach a server that isn't answering.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// When the host punches toward a viewer, counted from accepting its request.
pub const HOST_PUNCHES: [Duration; 7] = [
    Duration::ZERO,
    Duration::from_millis(20),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
];
/// Punches the viewer sends to the host's address before connecting to it.
pub const VIEWER_PUNCHES: usize = 2;
/// How long the viewer gives the direct connection, counted from learning the host's address,
/// before asking for a relay.
pub const PUNCH_TIMEOUT: Duration = Duration::from_millis(2500);

const ID_LABEL: &[u8] = b"lankvm rendezvous id v1";
const TOKEN_LABEL: &[u8] = b"lankvm rendezvous v1";
const REGISTER_LABEL: &[u8] = b"lankvm register v1";
/// Tokens are tied to 10-minute periods of the Unix clock, like knocks, and the host accepts the
/// period on either side of its own.
const BUCKET_SECS: u64 = 600;

/// Type bytes. Requests to the server have the top bit clear, its answers set.
mod kind {
    pub const REGISTER_BEGIN: u8 = 0x01;
    pub const REGISTER: u8 = 0x02;
    pub const KEEPALIVE: u8 = 0x03;
    pub const CONNECT: u8 = 0x04;
    pub const ACCEPT: u8 = 0x05;
    pub const RELAY_REQUEST: u8 = 0x06;
    pub const RELAY_ACCEPT: u8 = 0x07;
    pub const UNREGISTER: u8 = 0x08;
    pub const CHALLENGE: u8 = 0x81;
    pub const REGISTERED: u8 = 0x82;
    pub const ALIVE: u8 = 0x83;
    pub const INCOMING: u8 = 0x84;
    pub const PEER: u8 = 0x85;
    pub const RELAY_OFFER: u8 = 0x86;
    pub const RELAY_READY: u8 = 0x87;
    pub const RELAY_END: u8 = 0x88;
    pub const UNKNOWN: u8 = 0x89;
}

/// A control message between a Mac and the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Host: starts registering; the server answers with a [`Message::Challenge`].
    RegisterBegin { id: RendezvousId },
    /// Host: registers, signing the challenge's cookie with the key its ID derives from.
    Register { public_key: PublicKey, cookie: Cookie, signature: Signature },
    /// Host: still here, at the same address. Every [`KEEPALIVE_INTERVAL`].
    Keepalive { id: RendezvousId },
    /// Viewer: asks to be introduced to host `id`.
    Connect { id: RendezvousId, nonce: Nonce, token: Token },
    /// Host: accepts the viewer's connect request `nonce`, after checking its token.
    Accept { id: RendezvousId, nonce: Nonce },
    /// Viewer: asks for a relay to host `id`, when punching didn't work.
    RelayRequest { id: RendezvousId, nonce: Nonce, token: Token },
    /// Host: accepts the relay session `sid`, after checking the request's token.
    RelayAccept { id: RendezvousId, sid: SessionId },
    /// Host: no longer reachable (internet access turned off, or quitting).
    Unregister { id: RendezvousId },
    /// Server: register (again), with this cookie.
    Challenge { cookie: Cookie },
    /// Server: registered for `ttl_secs`; `observed` is the host's public address.
    Registered { ttl_secs: u16, observed: SocketAddr },
    /// Server: answer to a keepalive, with the host's public address.
    Alive { observed: SocketAddr },
    /// Server to host: the viewer at `viewer` asks to connect.
    Incoming { nonce: Nonce, token: Token, viewer: SocketAddr },
    /// Server to viewer: the host accepted request `nonce` and is at `host`.
    Peer { nonce: Nonce, host: SocketAddr },
    /// Server to host: the viewer at `viewer` asks for relay session `sid`.
    RelayOffer { nonce: Nonce, token: Token, sid: SessionId, viewer: SocketAddr },
    /// Server to viewer: the host accepted relay request `nonce`, as session `sid`.
    RelayReady { nonce: Nonce, sid: SessionId },
    /// Server: relay session `sid` is over.
    RelayEnd { sid: SessionId },
    /// Server to viewer: no host with that ID is registered (not running, or internet access off).
    Unknown { nonce: Nonce },
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(&MAGIC);
        out.push(self.kind());
        match self {
            Self::RegisterBegin { id } | Self::Keepalive { id } | Self::Unregister { id } => out.extend_from_slice(id),
            Self::Register { public_key, cookie, signature } => {
                out.extend_from_slice(public_key);
                out.extend_from_slice(cookie);
                out.extend_from_slice(signature);
            }
            Self::Connect { id, nonce, token } | Self::RelayRequest { id, nonce, token } => {
                out.extend_from_slice(id);
                out.extend_from_slice(nonce);
                out.extend_from_slice(token);
            }
            Self::Accept { id, nonce } => {
                out.extend_from_slice(id);
                out.extend_from_slice(nonce);
            }
            Self::RelayAccept { id, sid } => {
                out.extend_from_slice(id);
                out.extend_from_slice(sid);
            }
            Self::Challenge { cookie } => out.extend_from_slice(cookie),
            Self::Registered { ttl_secs, observed } => {
                out.extend_from_slice(&ttl_secs.to_be_bytes());
                encode_endpoint(&mut out, *observed);
            }
            Self::Alive { observed } => encode_endpoint(&mut out, *observed),
            Self::Incoming { nonce, token, viewer } => {
                out.extend_from_slice(nonce);
                out.extend_from_slice(token);
                encode_endpoint(&mut out, *viewer);
            }
            Self::Peer { nonce, host } => {
                out.extend_from_slice(nonce);
                encode_endpoint(&mut out, *host);
            }
            Self::RelayOffer { nonce, token, sid, viewer } => {
                out.extend_from_slice(nonce);
                out.extend_from_slice(token);
                out.extend_from_slice(sid);
                encode_endpoint(&mut out, *viewer);
            }
            Self::RelayReady { nonce, sid } => {
                out.extend_from_slice(nonce);
                out.extend_from_slice(sid);
            }
            Self::RelayEnd { sid } => out.extend_from_slice(sid),
            Self::Unknown { nonce } => out.extend_from_slice(nonce),
        }
        out
    }

    /// The message in `datagram`, if it is exactly one well-formed control message.
    pub fn decode(datagram: &[u8]) -> Option<Self> {
        let (&kind, body) = datagram.strip_prefix(&MAGIC)?.split_first()?;
        let mut r = Reader(body);
        let message = match kind {
            kind::REGISTER_BEGIN => Self::RegisterBegin { id: r.array()? },
            kind::REGISTER => Self::Register { public_key: r.array()?, cookie: r.array()?, signature: r.array()? },
            kind::KEEPALIVE => Self::Keepalive { id: r.array()? },
            kind::CONNECT => Self::Connect { id: r.array()?, nonce: r.array()?, token: r.array()? },
            kind::ACCEPT => Self::Accept { id: r.array()?, nonce: r.array()? },
            kind::RELAY_REQUEST => Self::RelayRequest { id: r.array()?, nonce: r.array()?, token: r.array()? },
            kind::RELAY_ACCEPT => Self::RelayAccept { id: r.array()?, sid: r.array()? },
            kind::UNREGISTER => Self::Unregister { id: r.array()? },
            kind::CHALLENGE => Self::Challenge { cookie: r.array()? },
            kind::REGISTERED => Self::Registered { ttl_secs: u16::from_be_bytes(r.array()?), observed: r.endpoint()? },
            kind::ALIVE => Self::Alive { observed: r.endpoint()? },
            kind::INCOMING => Self::Incoming { nonce: r.array()?, token: r.array()?, viewer: r.endpoint()? },
            kind::PEER => Self::Peer { nonce: r.array()?, host: r.endpoint()? },
            kind::RELAY_OFFER => {
                Self::RelayOffer { nonce: r.array()?, token: r.array()?, sid: r.array()?, viewer: r.endpoint()? }
            }
            kind::RELAY_READY => Self::RelayReady { nonce: r.array()?, sid: r.array()? },
            kind::RELAY_END => Self::RelayEnd { sid: r.array()? },
            kind::UNKNOWN => Self::Unknown { nonce: r.array()? },
            _ => return None,
        };
        r.0.is_empty().then_some(message)
    }

    fn kind(&self) -> u8 {
        match self {
            Self::RegisterBegin { .. } => kind::REGISTER_BEGIN,
            Self::Register { .. } => kind::REGISTER,
            Self::Keepalive { .. } => kind::KEEPALIVE,
            Self::Connect { .. } => kind::CONNECT,
            Self::Accept { .. } => kind::ACCEPT,
            Self::RelayRequest { .. } => kind::RELAY_REQUEST,
            Self::RelayAccept { .. } => kind::RELAY_ACCEPT,
            Self::Unregister { .. } => kind::UNREGISTER,
            Self::Challenge { .. } => kind::CHALLENGE,
            Self::Registered { .. } => kind::REGISTERED,
            Self::Alive { .. } => kind::ALIVE,
            Self::Incoming { .. } => kind::INCOMING,
            Self::Peer { .. } => kind::PEER,
            Self::RelayOffer { .. } => kind::RELAY_OFFER,
            Self::RelayReady { .. } => kind::RELAY_READY,
            Self::RelayEnd { .. } => kind::RELAY_END,
            Self::Unknown { .. } => kind::UNKNOWN,
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }

    fn endpoint(&mut self) -> Option<SocketAddr> {
        let ip = match self.array::<1>()? {
            [4] => IpAddr::V4(Ipv4Addr::from(self.array::<4>()?)),
            [6] => IpAddr::V6(Ipv6Addr::from(self.array::<16>()?)),
            _ => return None,
        };
        Some(SocketAddr::new(ip, u16::from_be_bytes(self.array()?)))
    }
}

/// Appends `ep` as an endpoint. An IPv4-mapped IPv6 address goes out as IPv4.
pub(crate) fn encode_endpoint(out: &mut Vec<u8>, ep: SocketAddr) {
    match ep.ip().to_canonical() {
        IpAddr::V4(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(6);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&ep.port().to_be_bytes());
}

/// The host ID that belongs to `public_key`.
pub fn id_for(public_key: &PublicKey) -> RendezvousId {
    let mut ctx = digest::Context::new(&digest::SHA256);
    ctx.update(ID_LABEL);
    ctx.update(public_key);
    ctx.finish().as_ref()[..ID_LEN].try_into().expect("SHA-256 is longer than an ID")
}

/// The token a viewer holding `access_key` sends with request `nonce` to host `id`, valid around
/// `now_unix`.
pub fn token(access_key: &AccessKey, id: &RendezvousId, nonce: &Nonce, now_unix: u64) -> Token {
    token_in(&hmac::Key::new(hmac::HMAC_SHA256, access_key), id, nonce, now_unix / BUCKET_SECS)
}

fn token_in(key: &hmac::Key, id: &RendezvousId, nonce: &Nonce, bucket: u64) -> Token {
    let mut ctx = hmac::Context::with_key(key);
    ctx.update(TOKEN_LABEL);
    ctx.update(id);
    ctx.update(nonce);
    ctx.update(&bucket.to_be_bytes());
    ctx.sign().as_ref()[..TOKEN_LEN].try_into().expect("HMAC-SHA256 is longer than a token")
}

/// The viewer whose access key made `token`, if it is valid for host `id` and request `nonce` in
/// the period of `now_unix` or the one on either side. Replays are the gate's business.
pub(crate) fn verify_token(keys: &KnockKeys, id: &RendezvousId, nonce: &Nonce, token: &Token, now_unix: u64) -> Option<Fingerprint> {
    let bucket = now_unix / BUCKET_SECS;
    let buckets = [Some(bucket), bucket.checked_sub(1), bucket.checked_add(1)];
    keys.iter()
        .find(|(_, key)| buckets.iter().flatten().any(|&b| ct_eq(&token_in(key, id, nonce, b), token)))
        .map(|(fp, _)| *fp)
}

/// What the host signs to register: the label and the server's cookie.
pub(crate) fn register_payload(cookie: &Cookie) -> Vec<u8> {
    [REGISTER_LABEL, cookie].concat()
}

/// A punch for request `nonce`.
pub fn punch(nonce: &Nonce) -> [u8; PUNCH_LEN] {
    let mut punch = [0; PUNCH_LEN];
    punch[..PUNCH_MAGIC.len()].copy_from_slice(&PUNCH_MAGIC);
    punch[PUNCH_MAGIC.len()..].copy_from_slice(nonce);
    punch
}

pub fn is_punch(datagram: &[u8]) -> bool {
    datagram.len() == PUNCH_LEN && datagram.starts_with(&PUNCH_MAGIC)
}

/// `packet` wrapped for relay session `sid`.
pub fn wrap(sid: &SessionId, packet: &[u8]) -> Vec<u8> {
    [&[DATA][..], sid, packet].concat()
}

/// The relay session and packet in a relayed datagram.
pub fn unwrap(datagram: &[u8]) -> Option<(SessionId, &[u8])> {
    let (sid, packet) = datagram.strip_prefix(&[DATA])?.split_first_chunk::<SID_LEN>()?;
    Some((*sid, packet))
}

/// Wraps every datagram in `contents`, which are `segment_size` bytes apart as in a quinn
/// `Transmit` (the last may be shorter; None means one datagram), and returns them with their new
/// segment size, so a batch stays one batch.
pub(crate) fn wrap_segments(sid: &SessionId, contents: &[u8], segment_size: Option<usize>) -> (Vec<u8>, Option<usize>) {
    let segment = segment_size.filter(|&s| s > 0).unwrap_or(contents.len()).max(1);
    let mut out = Vec::with_capacity(contents.len() + contents.len().div_ceil(segment) * DATA_HEADER_LEN);
    for packet in contents.chunks(segment) {
        out.push(DATA);
        out.extend_from_slice(sid);
        out.extend_from_slice(packet);
    }
    (out, segment_size.map(|s| s + DATA_HEADER_LEN))
}

/// What a datagram from a rendezvous server carries.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FromServer {
    Control,
    /// A relayed QUIC packet (not empty), after a [`DATA_HEADER_LEN`]-byte header naming its
    /// session.
    Data(SessionId),
    Other,
}

pub(crate) fn classify(datagram: &[u8]) -> FromServer {
    if datagram.starts_with(&MAGIC) {
        return FromServer::Control;
    }
    match unwrap(datagram) {
        Some((sid, packet)) if !packet.is_empty() => FromServer::Data(sid),
        _ => FromServer::Other,
    }
}

/// The host's identity on the rendezvous server: an Ed25519 key made once per install. Its ID is
/// what the host's viewers ask the server for, and only the holder of the key can register it.
pub struct RendezvousIdentity {
    key: Ed25519KeyPair,
    public_key: PublicKey,
    id: RendezvousId,
}

impl RendezvousIdentity {
    /// Loads [`KEY_FILE`] (PKCS#8) from `dir`, creating it (mode 0600) if missing.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let path = dir.join(KEY_FILE);
        match std::fs::read(&path) {
            Ok(pkcs8) => return Self::from_pkcs8(&pkcs8).with_context(|| format!("read {}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        }
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| anyhow::anyhow!("couldn't generate a rendezvous key"))?;
        write_private(&path, pkcs8.as_ref()).with_context(|| format!("write {}", path.display()))?;
        let identity = Self::from_pkcs8(pkcs8.as_ref())?;
        // Not the ID: with it, anyone could ask the server whether this Mac is online.
        tracing::info!("created the rendezvous key in {}", dir.display());
        Ok(identity)
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        let Ok(key) = Ed25519KeyPair::from_pkcs8(pkcs8) else { bail!("not an Ed25519 key in PKCS#8 form") };
        let public_key: PublicKey = key.public_key().as_ref().try_into().expect("Ed25519 public keys are 32 bytes");
        Ok(Self { key, public_key, id: id_for(&public_key) })
    }

    pub fn id(&self) -> RendezvousId {
        self.id
    }

    pub fn public_key(&self) -> PublicKey {
        self.public_key
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        self.key.sign(message).as_ref().try_into().expect("Ed25519 signatures are 64 bytes")
    }

    /// The registration answering the server's challenge with `cookie`.
    pub fn register(&self, cookie: &Cookie) -> Message {
        Message::Register { public_key: self.public_key, cookie: *cookie, signature: self.sign(&register_payload(cookie)) }
    }
}

/// Shows the start of the ID only, as logs do with fingerprints: the whole ID would let whoever
/// reads a log ask the server whether this Mac is online.
impl fmt::Debug for RendezvousIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RendezvousIdentity").field("id", &format_args!("{}…", hex(&self.id[..4]))).finish_non_exhaustive()
    }
}

/// Lowercase hex, as IDs are shown and stored.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use ring::signature::{ED25519, UnparsedPublicKey};

    use super::*;
    use crate::knock::random_bytes;

    const VIEWER: Fingerprint = [7; 32];
    const OTHER_VIEWER: Fingerprint = [8; 32];

    fn v4() -> SocketAddr {
        "203.0.113.7:47800".parse().unwrap()
    }

    fn v6() -> SocketAddr {
        "[2001:db8::1:2]:61000".parse().unwrap()
    }

    fn every_message() -> Vec<Message> {
        let (id, nonce, token, sid) = ([1; ID_LEN], [2; NONCE_LEN], [3; TOKEN_LEN], [4; SID_LEN]);
        vec![
            Message::RegisterBegin { id },
            Message::Register { public_key: [5; 32], cookie: [6; COOKIE_LEN], signature: [7; 64] },
            Message::Keepalive { id },
            Message::Connect { id, nonce, token },
            Message::Accept { id, nonce },
            Message::RelayRequest { id, nonce, token },
            Message::RelayAccept { id, sid },
            Message::Unregister { id },
            Message::Challenge { cookie: [6; COOKIE_LEN] },
            Message::Registered { ttl_secs: 75, observed: v4() },
            Message::Registered { ttl_secs: 65535, observed: v6() },
            Message::Alive { observed: v4() },
            Message::Alive { observed: v6() },
            Message::Incoming { nonce, token, viewer: v4() },
            Message::Incoming { nonce, token, viewer: v6() },
            Message::Peer { nonce, host: v4() },
            Message::Peer { nonce, host: v6() },
            Message::RelayOffer { nonce, token, sid, viewer: v4() },
            Message::RelayOffer { nonce, token, sid, viewer: v6() },
            Message::RelayReady { nonce, sid },
            Message::RelayEnd { sid },
            Message::Unknown { nonce },
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for message in every_message() {
            let bytes = message.encode();
            assert!(bytes.starts_with(&MAGIC));
            assert_eq!(Message::decode(&bytes), Some(message.clone()), "{message:?}");
        }
    }

    #[test]
    fn messages_have_the_sizes_in_the_spec() {
        let sizes: Vec<usize> = every_message().iter().map(|m| m.encode().len() - MAGIC.len() - 1).collect();
        assert_eq!(sizes, [16, 112, 16, 48, 32, 48, 24, 16, 16, 2 + 7, 2 + 19, 7, 19, 39, 51, 23, 35, 47, 59, 24, 8, 16]);
        // The server never answers an unverified source with more than it sent.
        let len = |m: Message| m.encode().len();
        let (id, nonce) = ([1; ID_LEN], [2; NONCE_LEN]);
        assert!(len(Message::Challenge { cookie: [0; COOKIE_LEN] }) <= len(Message::RegisterBegin { id }));
        assert!(len(Message::Challenge { cookie: [0; COOKIE_LEN] }) <= len(Message::Keepalive { id }));
        assert!(len(Message::Unknown { nonce }) <= len(Message::Connect { id, nonce, token: [0; TOKEN_LEN] }));
    }

    #[test]
    fn endpoints_are_family_address_port() {
        let bytes = Message::Alive { observed: v4() }.encode();
        assert_eq!(bytes[5..], [4, 203, 0, 113, 7, 0xba, 0xb8]);
        // An IPv4-mapped address from a dual-stack socket goes out as IPv4.
        let mapped: SocketAddr = "[::ffff:203.0.113.7]:47800".parse().unwrap();
        assert_eq!(Message::Alive { observed: mapped }.encode(), bytes);
        let bytes = Message::Alive { observed: v6() }.encode();
        let IpAddr::V6(ip) = v6().ip() else { unreachable!() };
        assert_eq!(bytes[5], 6);
        assert_eq!(bytes[6..22], ip.octets());
        assert_eq!(bytes[22..], 61000u16.to_be_bytes());
    }

    #[test]
    fn malformed_messages_are_rejected() {
        for message in every_message() {
            let bytes = message.encode();
            for len in 0..bytes.len() {
                assert_eq!(Message::decode(&bytes[..len]), None, "{message:?} cut to {len}");
            }
            assert_eq!(Message::decode(&[&bytes[..], &[0]].concat()), None, "{message:?} with a byte more");
            let mut wrong_magic = bytes.clone();
            wrong_magic[3] = b'2';
            assert_eq!(Message::decode(&wrong_magic), None);
        }
        // Unknown or reserved types.
        for kind in [0x00, 0x09, 0x80, 0x8a, 0xd0, 0xff] {
            assert_eq!(Message::decode(&[&MAGIC[..], &[kind], &[0; 16]].concat()), None, "type {kind:#x}");
        }
        // An endpoint of an unknown family, or whose length doesn't match its family.
        let mut bytes = Message::Alive { observed: v4() }.encode();
        bytes[5] = 5;
        assert_eq!(Message::decode(&bytes), None);
        bytes[5] = 6;
        assert_eq!(Message::decode(&bytes), None);
        let mut bytes = Message::Alive { observed: v6() }.encode();
        bytes[5] = 4;
        assert_eq!(Message::decode(&bytes), None);
        // Relayed data and punches aren't control messages.
        assert_eq!(Message::decode(&wrap(&[1; SID_LEN], b"packet")), None);
        assert_eq!(Message::decode(&punch(&[1; NONCE_LEN])), None);
    }

    #[test]
    fn random_bytes_never_panic() {
        for len in 0..200 {
            let mut bytes = random_bytes::<200>()[..len].to_vec();
            let _ = Message::decode(&bytes);
            if len >= 5 {
                bytes[..4].copy_from_slice(&MAGIC);
                let _ = Message::decode(&bytes);
            }
        }
    }

    #[test]
    fn token_matches_the_spec_test_vector() {
        let unix = 1_700_000_000;
        assert_eq!(unix / BUCKET_SECS, 2_833_333);
        let token = token(&[0x11; 32], &[0x22; 16], &[0x33; 16], unix);
        assert_eq!(hex(&token), "b9cbc668ed9c3c141c803d68a5e630b9");
    }

    #[test]
    fn id_matches_the_spec_test_vector() {
        let public_key: PublicKey = std::array::from_fn(|i| i as u8);
        assert_eq!(hex(&id_for(&public_key)), "523fd7834d53dc823718e155dbb8861c");
    }

    #[test]
    fn tokens_verify_for_their_viewer_host_and_request_within_a_period() {
        let secret = [1; 32];
        let keys = KnockKeys::new([
            (VIEWER, crate::knock::access_key(&secret, &VIEWER)),
            (OTHER_VIEWER, crate::knock::access_key(&secret, &OTHER_VIEWER)),
        ]);
        let (id, nonce, now) = ([2; ID_LEN], [3; NONCE_LEN], 1_790_000_000);
        let made = token(&crate::knock::access_key(&secret, &OTHER_VIEWER), &id, &nonce, now);
        assert_eq!(verify_token(&keys, &id, &nonce, &made, now), Some(OTHER_VIEWER));
        for skew in [-(BUCKET_SECS as i64), BUCKET_SECS as i64] {
            assert_eq!(verify_token(&keys, &id, &nonce, &made, now.checked_add_signed(skew).unwrap()), Some(OTHER_VIEWER));
        }
        for skew in [-2 * BUCKET_SECS as i64, 2 * BUCKET_SECS as i64] {
            assert_eq!(verify_token(&keys, &id, &nonce, &made, now.checked_add_signed(skew).unwrap()), None);
        }
        // For another host, another request, or made with another host's key.
        assert_eq!(verify_token(&keys, &[9; ID_LEN], &nonce, &made, now), None);
        assert_eq!(verify_token(&keys, &id, &[9; NONCE_LEN], &made, now), None);
        let foreign = token(&crate::knock::access_key(&[2; 32], &VIEWER), &id, &nonce, now);
        assert_eq!(verify_token(&keys, &id, &nonce, &foreign, now), None);
        assert_eq!(verify_token(&KnockKeys::new([]), &id, &nonce, &made, now), None);
        // At the start of the clock there is no earlier period.
        let early = token(&crate::knock::access_key(&secret, &VIEWER), &id, &nonce, 0);
        assert_eq!(verify_token(&keys, &id, &nonce, &early, 0), Some(VIEWER));
    }

    #[test]
    fn punches_carry_the_nonce() {
        let punch = punch(&[0xab; NONCE_LEN]);
        assert_eq!(punch[..4], *b"LKP1");
        assert_eq!(punch[4..], [0xab; NONCE_LEN]);
        assert!(is_punch(&punch));
        assert!(!is_punch(&punch[..PUNCH_LEN - 1]));
        assert!(!is_punch(&[&punch[..], &[0]].concat()));
        assert!(!is_punch(&Message::Unknown { nonce: [0; NONCE_LEN] }.encode()[..PUNCH_LEN]));
    }

    #[test]
    fn data_wraps_and_unwraps() {
        let sid = [0x5a; SID_LEN];
        let wrapped = wrap(&sid, b"quic packet");
        assert_eq!(wrapped[0], 0xd0);
        assert_eq!(wrapped[1..9], sid);
        assert_eq!(&wrapped[9..], b"quic packet");
        assert_eq!(unwrap(&wrapped), Some((sid, &b"quic packet"[..])));
        assert_eq!(unwrap(&wrapped[..DATA_HEADER_LEN]), Some((sid, &[][..])));
        assert_eq!(unwrap(&wrapped[..DATA_HEADER_LEN - 1]), None);
        assert_eq!(unwrap(&[0xc0; 20]), None);
        assert_eq!(classify(&wrapped), FromServer::Data(sid));
        assert_eq!(classify(&wrapped[..DATA_HEADER_LEN]), FromServer::Other);
        assert_eq!(classify(&Message::Unknown { nonce: [0; NONCE_LEN] }.encode()), FromServer::Control);
        assert_eq!(classify(b"LKR"), FromServer::Other);
        assert_eq!(classify(&punch(&[0; NONCE_LEN])), FromServer::Other);
        assert_eq!(classify(&[]), FromServer::Other);
    }

    #[test]
    fn segments_are_wrapped_one_by_one() {
        let sid = [9; SID_LEN];
        let contents: Vec<u8> = (0..25).collect();
        // Three datagrams of 10, 10 and 5 bytes.
        let (wrapped, segment) = wrap_segments(&sid, &contents, Some(10));
        assert_eq!(segment, Some(19));
        assert_eq!(wrapped.len(), 25 + 3 * DATA_HEADER_LEN);
        let datagrams: Vec<&[u8]> = wrapped.chunks(19).collect();
        assert_eq!(datagrams.len(), 3);
        for (i, datagram) in datagrams.iter().enumerate() {
            let (s, packet) = unwrap(datagram).unwrap();
            assert_eq!(s, sid);
            assert_eq!(packet, &contents[i * 10..(i * 10 + 10).min(25)]);
        }
        // One datagram.
        assert_eq!(wrap_segments(&sid, &contents, None), (wrap(&sid, &contents), None));
        assert_eq!(wrap_segments(&sid, &contents, Some(25)), (wrap(&sid, &contents), Some(34)));
    }

    #[test]
    fn identity_is_created_once_and_signs_registrations() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("lankvm-test-rendezvous-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let identity = RendezvousIdentity::load_or_create(&dir).unwrap();
        let mode = std::fs::metadata(dir.join(KEY_FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(identity.id(), id_for(&identity.public_key()));
        let again = RendezvousIdentity::load_or_create(&dir).unwrap();
        assert_eq!((again.id(), again.public_key()), (identity.id(), identity.public_key()));

        let cookie = [0x44; COOKIE_LEN];
        let Message::Register { public_key, cookie: signed, signature } = identity.register(&cookie) else { panic!() };
        assert_eq!((public_key, signed), (identity.public_key(), cookie));
        let verifier = UnparsedPublicKey::new(&ED25519, public_key);
        assert!(verifier.verify(&[&b"lankvm register v1"[..], &cookie].concat(), &signature).is_ok());
        assert!(verifier.verify(&[&b"lankvm register v1"[..], &[0x45; COOKIE_LEN]].concat(), &signature).is_err());
        let shown = format!("{identity:?}");
        assert!(!shown.contains("key:") && !shown.contains(&hex(&identity.id()[4..])), "{shown}");

        std::fs::write(dir.join(KEY_FILE), b"not a key").unwrap();
        assert!(RendezvousIdentity::load_or_create(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
