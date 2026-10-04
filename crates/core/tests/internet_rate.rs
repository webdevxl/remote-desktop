//! The internet rate controller on a shaped path: real QUIC with the internet transport settings,
//! through a relay in the test that squeezes what the host sends through a bottleneck with a
//! 64 KiB queue (more is dropped), and delays everything by 20 ms each way. The host side offers
//! video at the controller's ceiling (or what the screen needs, when that is less), holds back
//! while the backlog is over its limit and paces the connection from the ceiling, as the host
//! does.
//!
//! The ceiling must settle under the bottleneck (on average: probing above it now and then is how
//! the controller finds it, staying above it would be a queue), with little loss; when the path
//! also loses 1% of the packets for no reason, it must follow what the bottleneck carries, not
//! flee to the floor; when the screen needs more than a slower path carries, it must come down
//! to the path; and when the path slows down, it must follow within a second or so. QUIC doesn't
//! back off on loss (see `transport::cc::WanController`), so all of that is the controller's.
//!
//! How the controller rides out stalls, idle screens and round trips that change for good is
//! tested on its own, in `rate.rs`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lankvm_core::rate::{self, RateControl, Sample};
use quinn::Connection;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use transport::cc::Pace;
use transport::endpoint::Network;
use transport::identity::DeviceIdentity;

const QUEUE_BYTES: f64 = 64.0 * 1024.0;
const ONE_WAY: Duration = Duration::from_millis(20);
/// The stream's LAN bitrate: far above the path.
const LAN_BPS: u32 = 60_000_000;
const FPS: f64 = 60.0;
const RUN: Duration = Duration::from_secs(8);
/// The ceiling and the loss are judged over the end of the run, once the controller had time to
/// find the path.
const SETTLED_AFTER: Duration = Duration::from_secs(3);

#[tokio::test(flavor = "multi_thread")]
async fn the_ceiling_settles_under_the_bottleneck() {
    let run = settle("clean", Path { bottleneck: |_| 10e6, random_loss: 0.0 }, at_the_ceiling).await;
    assert!((4.0..=10.0).contains(&run.mean), "settled at {:.1} Mbit/s on average", run.mean);
    assert!(run.low >= 4.0, "fell to {:.1} Mbit/s", run.low);
    assert!(run.loss < 0.05, "{:.1}% of the video lost", run.loss * 100.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn random_loss_is_followed_not_fled() {
    // With 1% of the packets lost for no reason, the ceiling must not fall to the floor, though
    // QUIC counts a congestion event for nearly every one of those losses. (Under Cubic, whose
    // window shrank with each loss, the connection carried some 3 Mbit/s here.)
    let run = settle("lossy", Path { bottleneck: |_| 10e6, random_loss: 0.01 }, at_the_ceiling).await;
    assert!((4.0..=10.0).contains(&run.mean), "settled at {:.1} Mbit/s on average", run.mean);
    assert!(run.loss < 0.04, "{:.1}% of the video lost", run.loss * 100.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_screen_needing_more_than_the_path_carries_comes_down_to_it() {
    // The screen needs 6 Mbit/s, half the starting ceiling, and the path carries 3: the stream
    // never uses 70% of its ceiling, and QUIC sends all of it. Most would be lost, for good.
    let run = settle("slow", Path { bottleneck: |_| 3e6, random_loss: 0.0 }, |ceiling| ceiling.min(6e6)).await;
    assert!(run.mean <= 3.3, "settled at {:.1} Mbit/s on average", run.mean);
    assert!(run.loss < 0.05, "{:.1}% of the video lost", run.loss * 100.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_path_that_slows_down_is_followed() {
    // 20 Mbit/s, then 5 from 2 s on (someone else's download, a Wi-Fi rate drop): the ceiling
    // must be under it within a second, not seconds later.
    let slowing = Path { bottleneck: |t| if t < Duration::from_secs(2) { 20e6 } else { 5e6 }, random_loss: 0.0 };
    let run = settle("slowing", slowing, at_the_ceiling).await;
    let late: Vec<f64> = run.ceilings.iter().filter(|(t, _)| *t >= SETTLED_AFTER).map(|(_, c)| f64::from(*c) / 1e6).collect();
    assert!(late.iter().all(|&c| c <= 6.0), "{late:?}");
    assert!(run.loss < 0.05, "{:.1}% of the video lost", run.loss * 100.0);
}

/// What the relay does to what the host sends: the bottleneck's rate (bit/s) over time since the
/// relay started, and the fraction lost at random on the way.
#[derive(Clone, Copy)]
struct Path {
    bottleneck: fn(Duration) -> f64,
    random_loss: f64,
}

/// A screen that needs whatever the ceiling allows (bit/s).
fn at_the_ceiling(ceiling: f64) -> f64 {
    ceiling
}

/// What a run showed: the ceiling after each tick, its mean and lowest once settled (Mbit/s), and
/// the fraction of the video sent once settled that was lost.
struct Run {
    ceilings: Vec<(Duration, u32)>,
    mean: f64,
    low: f64,
    loss: f64,
}

/// Runs video over `path`, the screen needing `offer(ceiling)` bit/s.
async fn settle(side: &str, path: Path, offer: fn(f64) -> f64) -> Run {
    let host = network(&format!("host-{side}"));
    let viewer = network(&format!("viewer-{side}"));
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let relay_addr = socket.local_addr().unwrap();
    let relay = tokio::spawn(relay(socket, host.endpoint.local_addr().unwrap(), viewer.endpoint.local_addr().unwrap(), path));

    let pace = Pace::new();
    let accept = tokio::spawn({
        let (host, pace) = (host.clone(), pace.clone());
        async move {
            let incoming = host.endpoint.accept().await.unwrap();
            incoming.accept_with(host.internet_server_config(&pace)).unwrap().await.unwrap()
        }
    });
    let connecting = viewer.endpoint.connect_with(viewer.internet_client_config([7; 32]), relay_addr, "lankvm").unwrap();
    let viewer_conn = connecting.await.unwrap();
    let host_conn = accept.await.unwrap();

    // Each datagram says in its first byte whether it went out once settled.
    let received = Arc::new(AtomicU64::new(0));
    let receiver = tokio::spawn({
        let received = received.clone();
        async move {
            while let Ok(datagram) = viewer_conn.read_datagram().await {
                if datagram[0] == 1 {
                    received.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    });

    let (ceilings, sent) = send_at_the_ceiling(&host_conn, &pace, offer).await;
    tokio::time::sleep(ONE_WAY * 10).await;
    let received = received.load(Ordering::Relaxed);
    let stats = host_conn.stats();
    host_conn.close(0u32.into(), b"done");
    receiver.abort();
    relay.abort();

    let shown: Vec<String> = ceilings.iter().step_by(5).map(|(t, c)| format!("{:.1}s {:.1}", t.as_secs_f64(), f64::from(*c) / 1e6)).collect();
    let loss = 1.0 - received as f64 / sent as f64;
    eprintln!(
        "ceiling (Mbit/s): {}\nvideo datagrams once settled: {sent} sent, {received} received ({:.2}% lost); QUIC: {} of {} packets lost, {} congestion events",
        shown.join(", "),
        loss * 100.0,
        stats.path.lost_packets,
        stats.path.sent_packets,
        stats.path.congestion_events,
    );
    let settled: Vec<f64> = ceilings.iter().filter(|(t, _)| *t >= SETTLED_AFTER).map(|(_, c)| f64::from(*c) / 1e6).collect();
    let mean = settled.iter().sum::<f64>() / settled.len() as f64;
    let low = settled.iter().copied().fold(f64::MAX, f64::min);
    eprintln!("after {SETTLED_AFTER:?}: {mean:.2} Mbit/s on average, {low:.2} at the lowest");
    Run { ceilings, mean, low, loss }
}

fn network(side: &str) -> Network {
    let dir = std::env::temp_dir().join(format!("lankvm-test-rate-{side}-{}", std::process::id()));
    Network::bind("127.0.0.1:0".parse().unwrap(), &DeviceIdentity::load_or_create(&dir).unwrap()).unwrap()
}

/// Offers video for [`RUN`]: a frame's worth of datagrams of what the screen needs at the ceiling
/// (`offer`) 60 times a second, each held back while the backlog is over its limit (the newest one
/// goes once it drained, as the host's encode thread does), and the controller fed a sample every
/// tick, which sets the connection's `pace` as the host does. Returns the ceiling after each tick,
/// and the datagrams sent once settled.
async fn send_at_the_ceiling(conn: &Connection, pace: &Pace, offer: fn(f64) -> f64) -> (Vec<(Duration, u32)>, u64) {
    let empty_space = conn.datagram_send_buffer_space();
    let backlog = || empty_space.saturating_sub(conn.datagram_send_buffer_space());
    let mut control = RateControl::new(LAN_BPS);
    pace.set(rate::pace_for(control.ceiling()));
    let mut ceilings = Vec::new();
    let mut sent = 0u64;
    let start = Instant::now();
    let mut frames = tokio::time::interval(Duration::from_secs_f64(1.0 / FPS));
    let mut recheck = tokio::time::interval(Duration::from_millis(2));
    let mut ticks = tokio::time::interval(rate::TICK);
    ticks.tick().await;
    let (mut at, mut stats) = (Instant::now(), conn.stats());
    let mut waiting = false;
    while start.elapsed() < RUN {
        tokio::select! {
            _ = frames.tick() => waiting = true,
            _ = recheck.tick() => {}
            _ = ticks.tick() => {
                let (now, next) = (Instant::now(), conn.stats());
                control.on_sample(&Sample::between(&stats, &next, now - at, backlog()));
                pace.set(rate::pace_for(control.ceiling()));
                (at, stats) = (now, next);
                ceilings.push((start.elapsed(), control.ceiling()));
                continue;
            }
        }
        if !waiting || backlog() > rate::backlog_limit(control.ceiling()) {
            continue;
        }
        waiting = false;
        let settled = start.elapsed() >= SETTLED_AFTER;
        let max = conn.max_datagram_size().unwrap();
        let mut left = (offer(f64::from(control.ceiling())) / 8.0 / FPS) as usize;
        while left > 0 {
            let size = left.min(max);
            let mut datagram = vec![0u8; size];
            datagram[0] = u8::from(settled);
            conn.send_datagram(datagram.into()).unwrap();
            left -= size;
            sent += u64::from(settled);
        }
    }
    (ceilings, sent)
}

/// Relays between the viewer and the host. Everything takes [`ONE_WAY`]; what the host sends
/// first squeezes through the path's bottleneck, whose queue drops packets that find more than
/// [`QUEUE_BYTES`] ahead of them, and some of it is lost at random on the way.
async fn relay(socket: Arc<UdpSocket>, host: SocketAddr, viewer: SocketAddr, path: Path) {
    let (to_viewer, to_viewer_rx) = mpsc::unbounded_channel();
    let (to_host, to_host_rx) = mpsc::unbounded_channel();
    tokio::spawn(deliver(socket.clone(), to_viewer_rx, viewer));
    tokio::spawn(deliver(socket.clone(), to_host_rx, host));
    let start = Instant::now();
    let mut busy_until = Instant::now();
    let mut buf = vec![0u8; 2048];
    // xorshift64: the same losses every run.
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut lose = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 11) as f64 / (1u64 << 53) as f64) < path.random_loss
    };
    loop {
        let Ok((len, from)) = socket.recv_from(&mut buf).await else { return };
        let now = Instant::now();
        let packet = buf[..len].to_vec();
        if from != host {
            let _ = to_host.send((now + ONE_WAY, packet));
            continue;
        }
        let bps = (path.bottleneck)(now - start);
        let queued = busy_until.saturating_duration_since(now).as_secs_f64() * bps / 8.0;
        if queued + len as f64 > QUEUE_BYTES || lose() {
            continue;
        }
        busy_until = busy_until.max(now) + Duration::from_secs_f64(len as f64 * 8.0 / bps);
        let _ = to_viewer.send((busy_until + ONE_WAY, packet));
    }
}

/// Sends each packet to `to` when it is due (they come due in order).
async fn deliver(socket: Arc<UdpSocket>, mut packets: mpsc::UnboundedReceiver<(Instant, Vec<u8>)>, to: SocketAddr) {
    while let Some((at, packet)) = packets.recv().await {
        tokio::time::sleep_until(at.into()).await;
        let _ = socket.send_to(&packet, to).await;
    }
}
