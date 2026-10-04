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

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll, ready};
use std::time::Instant;

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};

use crate::cid::CidKey;
use crate::identity::{Fingerprint, short_hex};
use crate::knock::{AccessKey, Budget, KNOCK_LEN, KnockKeys, ReplayCache, Seen, now_unix};
use crate::net::is_local_network;

const LONG_HEADER: u8 = 0x80;
const QUIC_V1: u32 = 1;
const MAX_CID_LEN: usize = 20;
const INITIAL: u8 = 0;
/// Clients pad the datagram carrying their first packet to at least this, and quinn ignores
/// smaller ones.
const MIN_INITIAL_SIZE: usize = 1200;

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

    pub(crate) fn cid_key(&self) -> Arc<CidKey> {
        self.cid_key.clone()
    }

    /// The filter: false if the endpoint should never see this datagram from `remote`.
    pub(crate) fn allow_datagram(&self, data: &[u8], remote: SocketAddr) -> bool {
        !self.is_internet(remote) || self.allow_from_internet(data, remote)
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
            .field("stats", &self.stats())
            .finish_non_exhaustive()
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
}

impl GatedSocket {
    pub(crate) fn new(inner: Arc<dyn AsyncUdpSocket>, gate: Arc<Gate>) -> Self {
        Self { inner, gate }
    }
}

impl AsyncUdpSocket for GatedSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    /// Drops a denied datagram by setting its length to 0, which quinn skips without copying
    /// anything. Returns after one receive even if all of it was dropped, so quinn's limit on
    /// time spent receiving still holds during a flood.
    fn poll_recv(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        let n = ready!(self.inner.poll_recv(cx, bufs, meta))?;
        for (meta, buf) in meta[..n].iter_mut().zip(bufs.iter()) {
            if !self.gate.allow_received(&buf[..meta.len], meta.stride, meta.addr) {
                meta.len = 0;
            }
        }
        Poll::Ready(Ok(n))
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
}
