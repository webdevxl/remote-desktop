//! Rendezvous on loopback, with both Macs treating loopback as the internet: a host registers with
//! the in-process test server, and a paired viewer it introduces connects straight to the host
//! after punching, or through the server's relay when the server's directions lead nowhere.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use quinn::Connection;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};
use transport::cc::Pace;
use transport::endpoint::{Network, peer_fingerprint};
use transport::gate::ValidRequest;
use transport::identity::{DeviceIdentity, Fingerprint};
use transport::knock::{AccessKey, access_key, now_unix, random_bytes};
use transport::rendezvous::{
    HOST_PUNCHES, Message, Nonce, PUNCH_TIMEOUT, RendezvousId, RendezvousIdentity, VIEWER_PUNCHES, punch, token, wrap,
};
use transport::test_server::TestServer;

/// The host's secret its viewers' access keys come from.
const SECRET: [u8; 32] = [5; 32];
const WAIT: Duration = Duration::from_secs(5);
/// How long silence must last to count as no answer.
const SILENCE: Duration = Duration::from_secs(1);

type Control = mpsc::Receiver<(SocketAddr, Vec<u8>)>;

fn temp_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("lankvm-test-rendezvous-{name}-{}", std::process::id()))
}

/// Binds an IPv4 socket, as LanKVM does.
const V4: &str = "127.0.0.1:0";
/// Binds a dual-stack socket, which knows IPv4 addresses IPv4-mapped.
const DUAL_STACK: &str = "[::]:0";

/// A Mac using `server` from a socket bound to `bind`, with loopback counting as the internet.
fn mac(name: &str, server: &TestServer, bind: &str) -> (Network, Control, DeviceIdentity) {
    let identity = DeviceIdentity::load_or_create(&temp_dir(name)).unwrap();
    let network = Network::bind(bind.parse().unwrap(), &identity).unwrap();
    network.gate.set_loopback_is_internet(true);
    network.gate.set_servers(vec![server.addr()]);
    let control = network.gate.take_control_receiver();
    (network, control, identity)
}

/// Where the server, and anyone else, sees `network`.
fn addr(network: &Network) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, network.endpoint.local_addr().unwrap().port()))
}

async fn next_message(control: &mut Control) -> (SocketAddr, Message) {
    let (from, bytes) = timeout(WAIT, control.recv()).await.expect("a message from the server in time").unwrap();
    (from, Message::decode(&bytes).expect("a well-formed message"))
}

/// A host with internet access on for `viewer`, registered with `server`.
async fn open_host(
    name: &str,
    server: &TestServer,
    viewer: &DeviceIdentity,
    bind: &str,
) -> (Network, Control, DeviceIdentity, RendezvousId) {
    let (network, mut control, identity) = mac(name, server, bind);
    network.gate.set_keys(Some(vec![(viewer.fingerprint, access_key(&SECRET, &viewer.fingerprint))]));
    let rendezvous = RendezvousIdentity::load_or_create(&temp_dir(name)).unwrap();
    let id = rendezvous.id();

    network.send_raw(server.addr(), &Message::RegisterBegin { id }.encode()).await.unwrap();
    let (from, Message::Challenge { cookie }) = next_message(&mut control).await else { panic!("no challenge") };
    assert_eq!(from, server.addr());
    network.send_raw(server.addr(), &rendezvous.register(&cookie).encode()).await.unwrap();
    let (_, registered) = next_message(&mut control).await;
    assert_eq!(registered, Message::Registered { ttl_secs: 75, observed: addr(&network) });
    network.send_raw(server.addr(), &Message::Keepalive { id }.encode()).await.unwrap();
    assert_eq!(next_message(&mut control).await.1, Message::Alive { observed: addr(&network) });
    assert_eq!(server.registered(&id), Some(addr(&network)));
    (network, control, identity, id)
}

#[derive(Debug, PartialEq)]
enum HostEvent {
    /// Accepted a connect request, and punched if it was its first copy.
    Accepted(ValidRequest),
    /// Accepted a relay session from this viewer, known by this address.
    Relayed(Fingerprint, SocketAddr),
}

/// The host's side of rendezvous, as the core does it: answers the requests whose token checks
/// out, and punches toward the viewer (once per request).
fn host_agent(network: Network, id: RendezvousId, mut control: Control) -> (JoinHandle<()>, mpsc::UnboundedReceiver<HostEvent>) {
    let (events, received) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        while let Some((server, bytes)) = control.recv().await {
            match Message::decode(&bytes) {
                Some(Message::Incoming { nonce, token, viewer }) => {
                    let Some(request) = network.gate.check_token(&id, &nonce, &token, viewer, now_unix()) else { continue };
                    network.send_raw(server, &Message::Accept { id, nonce }.encode()).await.unwrap();
                    if request.first {
                        let network = network.clone();
                        tokio::spawn(async move {
                            let start = Instant::now();
                            for at in HOST_PUNCHES {
                                sleep_until(start + at).await;
                                network.send_raw(viewer, &punch(&nonce)).await.unwrap();
                            }
                        });
                    }
                    events.send(HostEvent::Accepted(request)).unwrap();
                }
                Some(Message::RelayOffer { nonce, token, sid, viewer }) => {
                    let Some(request) = network.gate.check_token(&id, &nonce, &token, viewer, now_unix()) else { continue };
                    if request.first {
                        let relay = network.gate.add_relay(server, sid);
                        events.send(HostEvent::Relayed(request.viewer, relay)).unwrap();
                    }
                    network.send_raw(server, &Message::RelayAccept { id, sid }.encode()).await.unwrap();
                }
                _ => {}
            }
        }
    });
    (task, received)
}

/// The host's accept loop for `viewer`, Retry first as the core does it.
fn accept(host: &Network, viewer: Fingerprint) -> JoinHandle<Connection> {
    let host = host.clone();
    tokio::spawn(async move {
        loop {
            let incoming = host.endpoint.accept().await.unwrap();
            if host.gate.admit(&incoming.orig_dst_cid(), incoming.remote_address()) != Some(viewer) {
                incoming.ignore();
            } else if !incoming.remote_address_validated() {
                incoming.retry().unwrap();
            } else {
                return incoming.accept_with(host.internet_server_config(&Pace::new())).unwrap().await.unwrap();
            }
        }
    })
}

/// Asks the server to introduce the viewer to host `id`: the host's address, as the server
/// reports it.
async fn introduce(viewer: &Network, control: &mut Control, server: &TestServer, id: RendezvousId, key: AccessKey) -> (Nonce, SocketAddr) {
    let nonce = random_bytes();
    let connect = Message::Connect { id, nonce, token: token(&key, &id, &nonce, now_unix()) };
    viewer.send_raw(server.addr(), &connect.encode()).await.unwrap();
    let (from, Message::Peer { nonce: answered, host }) = next_message(control).await else { panic!("no peer") };
    assert_eq!((from, answered), (server.addr(), nonce));
    (nonce, host)
}

/// Video one way and input the other, over a session.
async fn exchange(viewer: &Connection, host: &Connection, n: u8) {
    host.send_datagram(Bytes::from(vec![n; 1000])).unwrap();
    let datagram = timeout(WAIT, viewer.read_datagram()).await.unwrap().unwrap();
    assert_eq!(datagram[..], [n; 1000]);
    let (mut send, _) = viewer.open_bi().await.unwrap();
    send.write_all(&[n; 3000]).await.unwrap();
    send.finish().unwrap();
    let (_, mut receive) = timeout(WAIT, host.accept_bi()).await.unwrap().unwrap();
    assert_eq!(timeout(WAIT, receive.read_to_end(4096)).await.unwrap().unwrap(), [n; 3000]);
}

#[tokio::test]
async fn viewer_connects_straight_to_the_host_after_punching() {
    let server = TestServer::start().await.unwrap();
    let (viewer, mut viewer_control, viewer_id) = mac("punch-viewer", &server, V4);
    let (host, host_control, host_id, id) = open_host("punch-host", &server, &viewer_id, V4).await;
    let (agent, mut events) = host_agent(host.clone(), id, host_control);
    let accepting = accept(&host, viewer_id.fingerprint);

    let key = access_key(&SECRET, &viewer_id.fingerprint);
    let (nonce, host_addr) = introduce(&viewer, &mut viewer_control, &server, id, key).await;
    assert_eq!(host_addr, addr(&host));
    let accepted = ValidRequest { viewer: viewer_id.fingerprint, first: true };
    assert_eq!(timeout(WAIT, events.recv()).await.unwrap(), Some(HostEvent::Accepted(accepted)));
    // The request again, as when the answer got lost: answered again, but not as the first.
    let connect = Message::Connect { id, nonce, token: token(&key, &id, &nonce, now_unix()) };
    viewer.send_raw(server.addr(), &connect.encode()).await.unwrap();
    assert_eq!(next_message(&mut viewer_control).await.1, Message::Peer { nonce, host: host_addr });
    let again = ValidRequest { first: false, ..accepted };
    assert_eq!(timeout(WAIT, events.recv()).await.unwrap(), Some(HostEvent::Accepted(again)));
    for _ in 0..VIEWER_PUNCHES {
        viewer.send_raw(host_addr, &punch(&nonce)).await.unwrap();
    }
    let connecting = viewer.endpoint.connect_with(viewer.internet_client_config(key), host_addr, "lankvm").unwrap();
    let conn = timeout(PUNCH_TIMEOUT, connecting).await.expect("connected in time").unwrap();
    let host_conn = timeout(WAIT, accepting).await.expect("accepted in time").unwrap();
    assert_eq!(peer_fingerprint(&conn), Some(host_id.fingerprint));
    assert_eq!(peer_fingerprint(&host_conn), Some(viewer_id.fingerprint));
    assert!(!viewer.gate.is_relayed(conn.remote_address()) && !host.gate.is_relayed(host_conn.remote_address()));
    exchange(&conn, &host_conn, 1).await;
    // Straight from Mac to Mac: nothing went through the server.
    assert_eq!((server.relay_sessions(), server.stats().relayed), (0, 0));
    let stats = host.gate.stats();
    assert_eq!((stats.admitted, stats.replayed), (1, 0));
    agent.abort();
}

#[tokio::test]
async fn viewer_connects_through_the_relay_when_punching_fails() {
    relay_when_punching_fails("relay", V4).await;
}

#[tokio::test]
async fn relay_addresses_are_the_ones_quinn_uses_on_dual_stack_sockets() {
    relay_when_punching_fails("relay-dual-stack", DUAL_STACK).await;
}

/// Both Macs bind `bind`; the server lies about where each one is.
async fn relay_when_punching_fails(name: &str, bind: &str) {
    let server = TestServer::start().await.unwrap();
    server.set_lie_about_endpoints(true);
    let (viewer, mut viewer_control, viewer_id) = mac(&format!("{name}-viewer"), &server, bind);
    let (host, host_control, host_id, id) = open_host(&format!("{name}-host"), &server, &viewer_id, bind).await;
    let (agent, mut events) = host_agent(host.clone(), id, host_control);
    let accepting = accept(&host, viewer_id.fingerprint);

    // The server sends the viewer somewhere that never answers.
    let key = access_key(&SECRET, &viewer_id.fingerprint);
    let (nonce, nowhere) = introduce(&viewer, &mut viewer_control, &server, id, key).await;
    assert_ne!(nowhere, addr(&host));
    let accepted = ValidRequest { viewer: viewer_id.fingerprint, first: true };
    assert_eq!(timeout(WAIT, events.recv()).await.unwrap(), Some(HostEvent::Accepted(accepted)));
    for _ in 0..VIEWER_PUNCHES {
        viewer.send_raw(nowhere, &punch(&nonce)).await.unwrap();
    }
    let connecting = viewer.endpoint.connect_with(viewer.internet_client_config(key), nowhere, "lankvm").unwrap();
    assert!(timeout(SILENCE, connecting).await.is_err(), "connected to nowhere");

    let nonce = random_bytes();
    let request = Message::RelayRequest { id, nonce, token: token(&key, &id, &nonce, now_unix()) };
    viewer.send_raw(server.addr(), &request.encode()).await.unwrap();
    let (from, Message::RelayReady { nonce: answered, sid }) = next_message(&mut viewer_control).await else { panic!("no relay") };
    assert_eq!((from, answered), (server.addr(), nonce));
    let Some(HostEvent::Relayed(fp, host_side)) = timeout(WAIT, events.recv()).await.unwrap() else { panic!("the host didn't relay") };
    assert_eq!(fp, viewer_id.fingerprint);
    let relay = viewer.gate.add_relay(server.addr(), sid);
    assert!(viewer.gate.is_relayed(relay) && host.gate.is_relayed(host_side));

    let connecting = viewer.endpoint.connect_with(viewer.internet_client_config(key), relay, "lankvm").unwrap();
    let conn = timeout(WAIT, connecting).await.expect("connected in time").unwrap();
    let host_conn = timeout(WAIT, accepting).await.expect("accepted in time").unwrap();
    assert_eq!(peer_fingerprint(&conn), Some(host_id.fingerprint));
    assert_eq!(peer_fingerprint(&host_conn), Some(viewer_id.fingerprint));
    assert_eq!((conn.remote_address(), host_conn.remote_address()), (relay, host_side));
    exchange(&conn, &host_conn, 2).await;
    // Every packet went through the server, wrapped for the session by one Mac and unwrapped by
    // the other, and the server found nothing to drop.
    let relayed = server.stats();
    assert!(relayed.relayed > 10, "{relayed:?}");
    assert_eq!((relayed.dropped, server.relay_sessions()), (0, 1));
    assert_eq!(host.gate.stats().admitted, 1);

    // Someone else sending packets for the session: the server drops them, and so does the host
    // when they come straight to it, before quinn sees them.
    let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let junk = wrap(&sid, &[0x43; 1200]);
    stranger.send_to(&junk, server.addr()).await.unwrap();
    let ignored = host.gate.stats().ignored;
    stranger.send_to(&junk, addr(&host)).await.unwrap();
    timeout(WAIT, async {
        while server.stats().dropped == 0 || host.gate.stats().ignored == ignored {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("dropped in time");
    assert_eq!(server.stats().dropped, 1);
    exchange(&conn, &host_conn, 3).await;

    // Once the viewer ends the session, its packets for it go nowhere.
    assert_eq!(viewer.gate.remove_relay(server.addr(), &sid), Some(relay));
    conn.send_datagram(Bytes::from_static(b"after")).unwrap();
    assert!(timeout(SILENCE / 2, host_conn.read_datagram()).await.is_err());
    agent.abort();
}

#[tokio::test]
async fn a_relayed_connection_moves_straight_to_the_host() {
    let server = TestServer::start().await.unwrap();
    let (viewer, mut viewer_control, viewer_id) = mac("direct-viewer", &server, V4);
    let (host, host_control, _, id) = open_host("direct-host", &server, &viewer_id, V4).await;
    let (agent, mut events) = host_agent(host.clone(), id, host_control);
    let accepting = accept(&host, viewer_id.fingerprint);
    let key = access_key(&SECRET, &viewer_id.fingerprint);
    let nonce = random_bytes();
    let request = Message::RelayRequest { id, nonce, token: token(&key, &id, &nonce, now_unix()) };
    viewer.send_raw(server.addr(), &request.encode()).await.unwrap();
    let (_, Message::RelayReady { sid, .. }) = next_message(&mut viewer_control).await else { panic!("no relay") };
    let Some(HostEvent::Relayed(_, host_side)) = timeout(WAIT, events.recv()).await.unwrap() else { panic!("the host didn't relay") };
    let relay = viewer.gate.add_relay(server.addr(), sid);
    let connecting = viewer.endpoint.connect_with(viewer.internet_client_config(key), relay, "lankvm").unwrap();
    let conn = timeout(WAIT, connecting).await.expect("connected in time").unwrap();
    let host_conn = timeout(WAIT, accepting).await.expect("accepted in time").unwrap();
    exchange(&conn, &host_conn, 1).await;
    assert_eq!(host_conn.remote_address(), host_side);

    // The host turns out to be reachable straight (on the same network): the connection moves
    // there and carries on, the host following the viewer's packets to their new address.
    let path = viewer.go_direct(relay, addr(&host)).unwrap();
    exchange(&conn, &host_conn, 2).await;
    exchange(&conn, &host_conn, 3).await;
    assert!(path.heard() > 0);
    assert_eq!(conn.remote_address(), relay, "the viewer's quinn still knows the host by the relay's address");
    assert!(!host.gate.is_relayed(host_conn.remote_address()), "the host moved to {}", host_conn.remote_address());
    let through_server = server.stats().relayed;
    exchange(&conn, &host_conn, 4).await;
    assert!(server.stats().relayed <= through_server + 2, "{} more went through the server", server.stats().relayed - through_server);

    // Without the direct path, back through the relay: the host follows the viewer's next
    // packets (keep-alives) there. What it sent the other way meanwhile is lost.
    drop(path);
    timeout(WAIT, async {
        while host_conn.remote_address() != host_side {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("back through the relay in time");
    exchange(&conn, &host_conn, 5).await;
    assert!(server.stats().relayed > through_server + 2);
    agent.abort();
}

#[tokio::test]
async fn strangers_and_unknown_hosts_get_no_introduction() {
    let server = TestServer::start().await.unwrap();
    let (viewer, mut viewer_control, viewer_id) = mac("stranger-viewer", &server, V4);
    let (host, host_control, _, id) = open_host("stranger-host", &server, &viewer_id, V4).await;
    let (agent, mut events) = host_agent(host.clone(), id, host_control);

    // A token made with a key the host never gave out: the host stays silent, so the viewer
    // hears nothing at all.
    let wrong_key = access_key(&[6; 32], &viewer_id.fingerprint);
    let nonce: Nonce = random_bytes();
    let connect = Message::Connect { id, nonce, token: token(&wrong_key, &id, &nonce, now_unix()) };
    viewer.send_raw(server.addr(), &connect.encode()).await.unwrap();
    assert!(timeout(SILENCE, viewer_control.recv()).await.is_err());
    assert!(events.try_recv().is_err());

    // A host that isn't registered (any more).
    host.send_raw(server.addr(), &Message::Unregister { id }.encode()).await.unwrap();
    timeout(WAIT, async {
        while server.registered(&id).is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("unregistered in time");
    let key = access_key(&SECRET, &viewer_id.fingerprint);
    let nonce: Nonce = random_bytes();
    let connect = Message::Connect { id, nonce, token: token(&key, &id, &nonce, now_unix()) };
    viewer.send_raw(server.addr(), &connect.encode()).await.unwrap();
    assert_eq!(next_message(&mut viewer_control).await.1, Message::Unknown { nonce });
    agent.abort();
}
