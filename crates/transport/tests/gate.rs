//! The internet gate on loopback, with the host treating loopback as the internet: strangers get
//! no answer at all, and a paired viewer that knocks gets in.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use quinn::{ConnectionError, ConnectionId, TokenStore, TransportErrorCode};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::net::UdpSocket;
use tokio::time::timeout;
use transport::cc::Pace;
use transport::endpoint::{Network, peer_fingerprint};
use transport::identity::DeviceIdentity;
use transport::knock::{AccessKey, access_key};

/// The host's secret its viewers' access keys come from.
const SECRET: [u8; 32] = [5; 32];
/// How long silence must last to count as no answer.
const SILENCE: Duration = Duration::from_secs(1);

fn identity(name: &str) -> DeviceIdentity {
    let dir = std::env::temp_dir().join(format!("lankvm-test-gate-{name}-{}", std::process::id()));
    DeviceIdentity::load_or_create(&dir).unwrap()
}

fn bind(identity: &DeviceIdentity) -> Network {
    Network::bind("127.0.0.1:0".parse().unwrap(), identity).unwrap()
}

/// A host with internet access on for `viewer`, reached over loopback as if over the internet.
fn open_host(identity: &DeviceIdentity, viewer: &DeviceIdentity) -> Network {
    let host = bind(identity);
    host.gate.set_loopback_is_internet(true);
    host.gate.set_keys(Some(vec![(viewer.fingerprint, access_key(&SECRET, &viewer.fingerprint))]));
    host
}

fn addr(network: &Network) -> SocketAddr {
    network.endpoint.local_addr().unwrap()
}

/// A client's first datagram as a scanner would send it: an Initial-type long header with
/// `version` and `dcid`, and 1200 bytes of junk in all.
fn probe(version: u32, dcid: &[u8]) -> Vec<u8> {
    let mut p = vec![0xc3];
    p.extend(version.to_be_bytes());
    p.push(dcid.len() as u8);
    p.extend(dcid);
    p.push(8);
    p.extend([9; 8]);
    p.push(0); // token length
    let rest = 1200 - p.len() - 2;
    p.extend((0x4000 | rest as u16).to_be_bytes());
    p.extend((0..rest).map(|i| (i * 31 % 251) as u8));
    p
}

async fn answer(socket: &UdpSocket) -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    let len = timeout(SILENCE, socket.recv(&mut buf)).await.ok()?.unwrap();
    Some(buf[..len].to_vec())
}

#[tokio::test]
async fn strangers_get_no_answer() {
    let host_id = identity("silent-host");
    let open = open_host(&host_id, &identity("silent-viewer"));
    let local = bind(&host_id);
    let mut random_dcid = [0u8; 20];
    SystemRandom::new().fill(&mut random_dcid).unwrap();
    // A short header (1-RTT packet) for a session that doesn't exist. quinn would answer it with
    // a stateless reset if the connection ID passed for one of its own.
    let mut short = [0u8; 61];
    SystemRandom::new().fill(&mut short).unwrap();
    short[0] = 0x40 | (short[0] & 0x3f);

    for (host, local_network) in [(&open, false), (&local, true)] {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect(addr(host)).await.unwrap();
        socket.send(&probe(0x1a2a_3a4a, &[1; 8])).await.unwrap();
        // On the local network quinn answers an unknown version with Version Negotiation, which
        // shows this test would see an answer if there were one.
        let negotiation = answer(&socket).await;
        assert_eq!(negotiation.is_some(), local_network, "Version Negotiation probe, local network: {local_network}");
        if let Some(packet) = negotiation {
            assert_eq!(packet[1..5], [0, 0, 0, 0]);
        }
        socket.send(&probe(1, &random_dcid)).await.unwrap();
        socket.send(&short).await.unwrap();
        assert_eq!(answer(&socket).await, None, "QUIC v1 Initial without a knock or short header, local network: {local_network}");
    }
    assert_eq!(open.gate.stats().ignored, 2);
    assert_eq!(local.gate.stats().ignored, 0);
}

#[tokio::test]
async fn paired_viewer_knocks_and_gets_in() {
    let host_id = identity("knock-host");
    let viewer_id = identity("knock-viewer");
    let host = open_host(&host_id, &viewer_id);
    let viewer = bind(&viewer_id);
    // So the viewer's own filter has to let the host's answers through too.
    viewer.gate.set_loopback_is_internet(true);
    let viewer_fp = viewer_id.fingerprint;

    let server = {
        let host = host.clone();
        tokio::spawn(async move {
            // What the host does with an address it hasn't validated yet: Retry first.
            let incoming = host.endpoint.accept().await.unwrap();
            assert_eq!(host.gate.admit(&incoming.orig_dst_cid(), incoming.remote_address()), Some(viewer_fp));
            assert!(!incoming.remote_address_validated());
            incoming.retry().unwrap();
            let incoming = host.endpoint.accept().await.unwrap();
            assert!(incoming.remote_address_validated());
            // Still the knock: quinn carries it through the Retry token.
            assert_eq!(host.gate.admit(&incoming.orig_dst_cid(), incoming.remote_address()), Some(viewer_fp));
            let conn = incoming.accept_with(host.internet_server_config(&Pace::new())).unwrap().await.unwrap();
            assert_eq!(peer_fingerprint(&conn), Some(viewer_fp));
            conn
        })
    };

    let key = access_key(&SECRET, &viewer_fp);
    let connecting = viewer.endpoint.connect_with(viewer.internet_client_config(key), addr(&host), "lankvm").unwrap();
    let conn = timeout(Duration::from_secs(5), connecting).await.expect("connected in time").unwrap();
    assert_eq!(peer_fingerprint(&conn), Some(host_id.fingerprint));
    let host_conn = timeout(Duration::from_secs(5), server).await.expect("accepted in time").unwrap();

    // Both ends use the internet settings: the viewer's Cubic and the host's paced window, not the
    // LAN controller's 32 MiB.
    assert!(conn.congestion_state().window() < 32 * 1024 * 1024);
    assert!(host_conn.congestion_state().window() < 32 * 1024 * 1024);
    host_conn.send_datagram(bytes::Bytes::from_static(b"frame")).unwrap();
    let datagram = timeout(Duration::from_secs(5), conn.read_datagram()).await.unwrap().unwrap();
    assert_eq!(&datagram[..], b"frame");
    let stats = host.gate.stats();
    assert_eq!((stats.admitted, stats.replayed, stats.throttled), (1, 0, 0));
}

/// The viewer tries to connect with `key`; neither side should hear anything from the other.
async fn assert_silent(host: &Network, viewer: &Network, key: AccessKey) {
    let accepting = {
        let endpoint = host.endpoint.clone();
        tokio::spawn(async move { endpoint.accept().await.is_some() })
    };
    let connecting = viewer.endpoint.connect_with(viewer.internet_client_config(key), addr(host), "lankvm").unwrap();
    assert!(timeout(SILENCE * 2, connecting).await.is_err(), "the attempt ended instead of going unanswered");
    assert!(!accepting.is_finished(), "the host's endpoint saw the attempt");
    accepting.abort();
    let stats = host.gate.stats();
    assert!(stats.ignored >= 1, "{stats:?}");
    assert_eq!(stats.admitted, 0);
}

#[tokio::test]
async fn wrong_key_gets_no_answer() {
    let viewer_id = identity("wrong-key-viewer");
    let host = open_host(&identity("wrong-key-host"), &viewer_id);
    let viewer = bind(&viewer_id);
    assert_silent(&host, &viewer, access_key(&[6; 32], &viewer_id.fingerprint)).await;
}

#[tokio::test]
async fn internet_access_off_gets_no_answer() {
    let viewer_id = identity("off-viewer");
    let host = open_host(&identity("off-host"), &viewer_id);
    host.gate.set_keys(None);
    let viewer = bind(&viewer_id);
    assert_silent(&host, &viewer, access_key(&SECRET, &viewer_id.fingerprint)).await;
}

/// Hands out the same token on every connection attempt.
struct CopiedToken(Bytes);

impl TokenStore for CopiedToken {
    fn insert(&self, _server_name: &str, _token: Bytes) {}

    fn take(&self, _server_name: &str) -> Option<Bytes> {
        Some(self.0.clone())
    }
}

#[tokio::test]
async fn retry_token_copied_off_the_wire_gets_no_answer() {
    let host_id = identity("retry-host");
    let viewer_id = identity("retry-viewer");
    let host = open_host(&host_id, &viewer_id);
    let viewer = bind(&viewer_id);
    let viewer_fp = viewer_id.fingerprint;

    // Someone on the path between the viewer and the host, keeping a copy of the host's Retry.
    let relay = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = relay.local_addr().unwrap();
    let retry = Arc::new(Mutex::new(None::<Vec<u8>>));
    let relaying = {
        let (host_addr, retry) = (addr(&host), retry.clone());
        tokio::spawn(async move {
            let mut viewer_addr = None;
            let mut buf = [0u8; 2048];
            loop {
                let (len, from) = relay.recv_from(&mut buf).await.unwrap();
                let packet = &buf[..len];
                if from != host_addr {
                    viewer_addr = Some(from);
                    relay.send_to(packet, host_addr).await.unwrap();
                    continue;
                }
                if packet[0] & 0xf0 == 0xf0 {
                    *retry.lock().unwrap() = Some(packet.to_vec());
                }
                if let Some(viewer_addr) = viewer_addr {
                    relay.send_to(packet, viewer_addr).await.unwrap();
                }
            }
        })
    };

    let server = {
        let host = host.clone();
        tokio::spawn(async move {
            let incoming = host.endpoint.accept().await.unwrap();
            assert_eq!(host.gate.admit(&incoming.orig_dst_cid(), incoming.remote_address()), Some(viewer_fp));
            incoming.retry().unwrap();
            let incoming = host.endpoint.accept().await.unwrap();
            assert_eq!(host.gate.admit(&incoming.orig_dst_cid(), incoming.remote_address()), Some(viewer_fp));
            incoming.accept_with(host.internet_server_config(&Pace::new())).unwrap().await.unwrap()
        })
    };
    let key = access_key(&SECRET, &viewer_fp);
    let connecting = viewer.endpoint.connect_with(viewer.internet_client_config(key), relay_addr, "lankvm").unwrap();
    let conn = timeout(Duration::from_secs(5), connecting).await.expect("connected in time").unwrap();
    let host_conn = timeout(Duration::from_secs(5), server).await.expect("accepted in time").unwrap();
    // Once the session is over, quinn no longer knows the Retry's connection ID and checks the
    // token itself.
    conn.close(0u32.into(), b"");
    host_conn.close(0u32.into(), b"");
    timeout(Duration::from_secs(5), host.endpoint.wait_idle()).await.expect("session closed in time");
    relaying.abort();

    // Retry: first byte, version, destination ID, source ID, token, 16-byte integrity tag.
    let retry = retry.lock().unwrap().clone().expect("the host sent a Retry");
    let dcid_len = retry[5] as usize;
    let scid_len = retry[6 + dcid_len] as usize;
    let scid = ConnectionId::new(&retry[7 + dcid_len..7 + dcid_len + scid_len]);
    let token = Bytes::copy_from_slice(&retry[7 + dcid_len + scid_len..retry.len() - 16]);

    // A stranger sends the copied token from its own address, to the Retry's connection ID.
    let stranger = bind(&identity("retry-stranger"));
    let mut config = stranger.internet_client_config([0; 32]);
    config.initial_dst_cid_provider(Arc::new(move || scid));
    config.token_store(Arc::new(CopiedToken(token)));
    let ignored = host.gate.stats().ignored;
    let connecting = stranger.endpoint.connect_with(config.clone(), addr(&host), "lankvm").unwrap();
    assert!(timeout(SILENCE * 2, connecting).await.is_err(), "the host answered a copied Retry token");
    assert!(host.gate.stats().ignored > ignored);

    // Without the gate, quinn rejects the token out loud: the silence above is the gate's doing.
    host.gate.set_loopback_is_internet(false);
    let connecting = stranger.endpoint.connect_with(config, addr(&host), "lankvm").unwrap();
    match timeout(SILENCE * 2, connecting).await.expect("the host answered") {
        Err(ConnectionError::ConnectionClosed(close)) => assert_eq!(close.error_code, TransportErrorCode::INVALID_TOKEN),
        other => panic!("expected INVALID_TOKEN, got {other:?}"),
    }
}
