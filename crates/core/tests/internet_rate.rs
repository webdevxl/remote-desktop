//! The internet rate controller on a shaped path: real QUIC with the internet transport settings,
//! through a relay in the test that carries 10 Mbit/s from the host to the viewer with a 64 KiB
//! queue (more is dropped), and delays everything by 20 ms each way. The host side offers video at
//! the controller's ceiling and holds back while the backlog is over its limit, as the host's
//! encode thread does. The ceiling must settle under the bottleneck (on average: probing above it
//! now and then is how the controller finds it, staying above it would be a queue), with little
//! loss; and when the path also loses 1% of the packets for no reason, it must follow what QUIC
//! still carries.
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
use transport::endpoint::Network;
use transport::identity::DeviceIdentity;

const BOTTLENECK_BPS: f64 = 10e6;
const QUEUE_BYTES: f64 = 64.0 * 1024.0;
const ONE_WAY: Duration = Duration::from_millis(20);
/// The stream's LAN bitrate: far above the path.
const LAN_BPS: u32 = 60_000_000;
const FPS: f64 = 60.0;
const RUN: Duration = Duration::from_secs(8);
/// The ceiling is judged over the end of the run, once it had time to find the path.
const SETTLED_AFTER: Duration = Duration::from_secs(3);

#[tokio::test(flavor = "multi_thread")]
async fn the_ceiling_settles_under_the_bottleneck() {
    let (mean, low, loss) = settle("clean", 0.0).await;
    assert!((4.0..=10.0).contains(&mean), "settled at {mean:.1} Mbit/s on average");
    assert!(low >= 4.0, "fell to {low:.1} Mbit/s");
    assert!(loss < 0.05, "{:.1}% of the video lost", loss * 100.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn random_loss_is_followed_not_fled() {
    // With 1% of the packets lost for no reason, QUIC's Cubic carries about 3 Mbit/s at this round
    // trip, whatever the ceiling (some 1.2 packets per round trip over the square root of the loss
    // rate). The ceiling must follow that, not fall to the floor, though QUIC counts a congestion
    // event for nearly every one of those losses.
    let (mean, _, loss) = settle("lossy", 0.01).await;
    assert!((2.0..=10.0).contains(&mean), "settled at {mean:.1} Mbit/s on average");
    assert!(loss < 0.04, "{:.1}% of the video lost", loss * 100.0);
}

/// Runs video over the shaped path, which also drops `random_loss` of what the host sends; returns
/// the ceiling's mean and lowest once settled (Mbit/s), and the fraction of the video lost.
async fn settle(side: &str, random_loss: f64) -> (f64, f64, f64) {
    let host = network(&format!("host-{side}"));
    let viewer = network(&format!("viewer-{side}"));
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let relay_addr = socket.local_addr().unwrap();
    let path = tokio::spawn(relay(socket, host.endpoint.local_addr().unwrap(), viewer.endpoint.local_addr().unwrap(), random_loss));

    let accept = tokio::spawn({
        let host = host.clone();
        async move {
            let incoming = host.endpoint.accept().await.unwrap();
            incoming.accept_with(host.internet_server_config()).unwrap().await.unwrap()
        }
    });
    let connecting = viewer.endpoint.connect_with(viewer.internet_client_config([7; 32]), relay_addr, "lankvm").unwrap();
    let viewer_conn = connecting.await.unwrap();
    let host_conn = accept.await.unwrap();

    let received = Arc::new(AtomicU64::new(0));
    let receiver = tokio::spawn({
        let received = received.clone();
        async move {
            while viewer_conn.read_datagram().await.is_ok() {
                received.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    let (trace, sent) = send_at_the_ceiling(&host_conn).await;
    tokio::time::sleep(ONE_WAY * 10).await;
    let received = received.load(Ordering::Relaxed);
    let stats = host_conn.stats();
    host_conn.close(0u32.into(), b"done");
    receiver.abort();
    path.abort();

    let shown: Vec<String> = trace.iter().step_by(5).map(|(t, c)| format!("{:.1}s {:.1}", t.as_secs_f64(), f64::from(*c) / 1e6)).collect();
    let loss = 1.0 - received as f64 / sent as f64;
    eprintln!(
        "ceiling (Mbit/s): {}\nvideo datagrams: {sent} sent, {received} received ({:.2}% lost); QUIC: {} of {} packets lost, {} congestion events",
        shown.join(", "),
        loss * 100.0,
        stats.path.lost_packets,
        stats.path.sent_packets,
        stats.path.congestion_events,
    );
    let settled: Vec<f64> = trace.iter().filter(|(t, _)| *t >= SETTLED_AFTER).map(|(_, c)| f64::from(*c) / 1e6).collect();
    let mean = settled.iter().sum::<f64>() / settled.len() as f64;
    let low = settled.iter().copied().fold(f64::MAX, f64::min);
    eprintln!("after {SETTLED_AFTER:?}: {mean:.2} Mbit/s on average, {low:.2} at the lowest");
    (mean, low, loss)
}

fn network(side: &str) -> Network {
    let dir = std::env::temp_dir().join(format!("lankvm-test-rate-{side}-{}", std::process::id()));
    Network::bind("127.0.0.1:0".parse().unwrap(), &DeviceIdentity::load_or_create(&dir).unwrap()).unwrap()
}

/// Offers video at the controller's ceiling for [`RUN`]: a frame's worth of datagrams 60 times a
/// second, each held back while the backlog is over its limit (the newest one goes once it
/// drained, as the host's encode thread does), and the controller fed a sample every tick.
/// Returns the ceiling after each tick, and the datagrams sent.
async fn send_at_the_ceiling(conn: &Connection) -> (Vec<(Duration, u32)>, u64) {
    let empty_space = conn.datagram_send_buffer_space();
    let backlog = || empty_space.saturating_sub(conn.datagram_send_buffer_space());
    let mut control = RateControl::new(LAN_BPS);
    let mut trace = Vec::new();
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
                (at, stats) = (now, next);
                trace.push((start.elapsed(), control.ceiling()));
                continue;
            }
        }
        if !waiting || backlog() > rate::backlog_limit(control.ceiling()) {
            continue;
        }
        waiting = false;
        let max = conn.max_datagram_size().unwrap();
        let mut left = (f64::from(control.ceiling()) / 8.0 / FPS) as usize;
        while left > 0 {
            let size = left.min(max);
            conn.send_datagram(vec![0u8; size].into()).unwrap();
            left -= size;
            sent += 1;
        }
    }
    (trace, sent)
}

/// Relays between the viewer and the host. Everything takes [`ONE_WAY`]; what the host sends first
/// squeezes through a [`BOTTLENECK_BPS`] link, whose queue drops packets that find more than
/// [`QUEUE_BYTES`] ahead of them. `random_loss` of what the host sends is dropped on the way too.
async fn relay(socket: Arc<UdpSocket>, host: SocketAddr, viewer: SocketAddr, random_loss: f64) {
    let (to_viewer, to_viewer_rx) = mpsc::unbounded_channel();
    let (to_host, to_host_rx) = mpsc::unbounded_channel();
    tokio::spawn(deliver(socket.clone(), to_viewer_rx, viewer));
    tokio::spawn(deliver(socket.clone(), to_host_rx, host));
    let mut busy_until = Instant::now();
    let mut buf = vec![0u8; 2048];
    // xorshift64: the same losses every run.
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut lose = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 11) as f64 / (1u64 << 53) as f64) < random_loss
    };
    loop {
        let Ok((len, from)) = socket.recv_from(&mut buf).await else { return };
        let now = Instant::now();
        let packet = buf[..len].to_vec();
        if from != host {
            let _ = to_host.send((now + ONE_WAY, packet));
            continue;
        }
        let queued = busy_until.saturating_duration_since(now).as_secs_f64() * BOTTLENECK_BPS / 8.0;
        if queued + len as f64 > QUEUE_BYTES || lose() {
            continue;
        }
        busy_until = busy_until.max(now) + Duration::from_secs_f64(len as f64 * 8.0 / BOTTLENECK_BPS);
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
