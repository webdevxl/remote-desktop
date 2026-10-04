//! How late video arrives over internet-like paths, to compare congestion controllers: real QUIC
//! with the internet transport settings, through a relay in the test that delays everything by a
//! one-way delay each way and, from the host to the viewer, may squeeze it through a bottleneck
//! with a drop-tail queue and lose some of it at random.
//!
//! The host side captures a frame 60 times a second (sizes from [`Profile`], following the
//! ceiling as [`encoded`] says) and sends it the way the host's encode thread does: held back while
//! more than [`rate::backlog_limit`] of video waits in QUIC's datagram buffer, a newer capture
//! replacing the one waiting, with [`RateControl`] fed a sample every tick and the connection's
//! [`Pace`] following its ceiling, as `follow_the_connection` does. Each datagram starts
//! with a [`Header`]; the viewer puts the frames back together, and a frame is delivered when its
//! last datagram arrives. One that misses a datagram is lost: datagrams are never resent.
//!
//! A frame's latency is its delivery less its capture less the one-way delay: what the backlog,
//! QUIC's congestion window and pacer, and the bottleneck's queue added (the raw figure keeps the
//! delay). Frames captured over the first [`WARM_UP`] don't count. The relay runs on threads of
//! its own and keeps time to a few µs ([`wait_until`]), so it adds next to nothing; what the path
//! without delay shows (`the_harness_adds_next_to_nothing`) is QUIC and loopback's own: a
//! fraction of a millisecond for most frames, a few for the biggest (macOS takes some 15 µs a
//! datagram on loopback). QUIC's timers run on tokio's as in the app, which holds the same
//! latency-critical activity: they fire a millisecond or two late, pacing included.
//!
//! Benchmarks, so `cargo test` skips them. Run them with
//!
//! ```text
//! cargo test --release -p lankvm-core --test wan_latency -- --ignored --nocapture --test-threads=1
//! ```
//!
//! - `LANKVM_BENCH_CC` picks the host's congestion controller (see [`Cc`]): `network` (the
//!   default: the internet server config exactly as the [`Network`] builds it, what production
//!   runs), `cubic128k` (what production ran before the paced controller), or `fixed:<bytes>`.
//! - `LANKVM_BENCH_FILTER` runs only the paths whose names contain one of its comma-separated
//!   parts: `74ms`, `10M`, `37ms/unl`, `0.5%`.
//! - `LANKVM_BENCH_TRACE` prints what the rate controller saw and did every tick.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lankvm_core::rate::{self, RateControl, Sample};
use platform_mac::system;
use quinn::congestion::{Controller, ControllerFactory, CubicConfig};
use quinn::{Connection, ConnectionStats, TransportConfig};
use transport::cc::Pace;
use transport::endpoint::Network;
use transport::identity::DeviceIdentity;

const FPS: f64 = 60.0;
/// The stream's LAN bitrate: the ceiling never goes above it.
const LAN_BPS: u32 = 60_000_000;
/// Each path is streamed over this long; frames captured over the first `WARM_UP` don't count.
const RUN: Duration = Duration::from_secs(6);
const WARM_UP: Duration = Duration::from_secs(2);
/// How often a frame held back checks again whether it may go (the host's `BACKLOG_RECHECK`).
const RECHECK: Duration = Duration::from_millis(2);
/// The relay's kernel buffers: room for the bursts a large window sends at once.
const RELAY_BUFFER: usize = 7 * 1024 * 1024;
/// The relay spins rather than sleeps this close to a packet's time.
const SPIN: Duration = Duration::from_micros(200);

/// Nominal frame sizes (bytes), see [`Profile`].
const SMALL: (usize, usize) = (2_000, 8_000);
const MEDIUM: (usize, usize) = (20_000, 40_000);
const MEDIUM_SHARE: f64 = 0.2;
const BIG: (usize, usize) = (150_000, 300_000);
const BIG_EVERY: (usize, usize) = (20, 40);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run with --ignored"]
async fn the_harness_adds_next_to_nothing() {
    // No delay, no bottleneck, no loss: whatever the latency is, the harness (and QUIC on
    // loopback) made it.
    let _precise = system::begin_latency_critical_activity();
    let cc = Cc::from_env();
    let row = run(Path { one_way: Duration::ZERO, bottleneck: None, loss: 0.0 }, &cc).await;
    println!("{cc}\n{TITLE}\n{row}");
    let p50 = percentile(&row.latency, 0.5);
    assert!(p50 <= 1.0, "half the frames took over {p50:.2} ms");
    assert_eq!(row.lost, 0, "frames lost on a lossless path");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: run with --ignored"]
async fn latency_over_internet_paths() {
    // Timers as precise as the app's (`Core::start`).
    let _precise = system::begin_latency_critical_activity();
    let cc = Cc::from_env();
    let filter = std::env::var("LANKVM_BENCH_FILTER").unwrap_or_default();
    let wanted = |path: &Path| filter.is_empty() || filter.split(',').any(|part| path.name().contains(part.trim()));
    println!("{cc}\n{TITLE}");
    for path in paths().into_iter().filter(wanted) {
        println!("{}", run(path, &cc).await);
    }
}

/// From the host to the viewer and back.
#[derive(Clone, Copy, Debug)]
struct Path {
    /// Each way.
    one_way: Duration,
    /// From the host to the viewer; None: as fast as the relay goes.
    bottleneck: Option<Link>,
    /// The fraction of what the host sends that is lost on the way, at random.
    loss: f64,
}

/// A bottleneck's rate (bit/s), and the bytes its queue holds: a packet that finds it full is
/// dropped.
#[derive(Clone, Copy, Debug)]
struct Link {
    bps: f64,
    queue: usize,
}

impl Path {
    /// Like `74ms/10M/0.5%`.
    fn name(&self) -> String {
        let link = self.bottleneck.map_or("unl".to_string(), |l| format!("{}M", l.bps / 1e6));
        format!("{}ms/{link}/{}%", self.one_way.as_millis(), self.loss * 100.0)
    }
}

/// Without delay first (what the harness adds), then one way 10, 37 and 74 ms (a relayed path:
/// two 37 ms legs), each unlimited, at 40 Mbit/s with a 512 KiB queue and at 10 Mbit/s with a
/// 64 KiB one, each losing nothing and losing 0.5% of the packets.
fn paths() -> Vec<Path> {
    let links = [None, Some(Link { bps: 40e6, queue: 512 * 1024 }), Some(Link { bps: 10e6, queue: 64 * 1024 })];
    let mut paths = vec![Path { one_way: Duration::ZERO, bottleneck: None, loss: 0.0 }];
    for one_way in [10, 37, 74] {
        for bottleneck in links {
            for loss in [0.0, 0.005] {
                paths.push(Path { one_way: Duration::from_millis(one_way), bottleneck, loss });
            }
        }
    }
    paths
}

/// The host's congestion controller, from `LANKVM_BENCH_CC`.
#[derive(Clone, Copy, Debug)]
enum Cc {
    /// `network`: the internet server config as the [`Network`] builds it.
    Network,
    /// `cubic128k`: Cubic from a 128 KiB window, in transport settings mirroring `endpoint.rs`'s:
    /// the host's internet settings before the paced controller.
    Cubic128k,
    /// `fixed:<bytes>`: a window of that many bytes that never changes, in the same settings.
    /// quinn still paces it, at 1.25 windows per round trip.
    Fixed(u64),
}

impl Cc {
    fn from_env() -> Self {
        match std::env::var("LANKVM_BENCH_CC").unwrap_or_default().as_str() {
            "" | "network" => Self::Network,
            "cubic128k" => Self::Cubic128k,
            other => match other.strip_prefix("fixed:").and_then(|bytes| bytes.parse().ok()) {
                Some(window) => Self::Fixed(window),
                None => panic!("LANKVM_BENCH_CC is network, cubic128k or fixed:<bytes>, not {other:?}"),
            },
        }
    }
}

impl fmt::Display for Cc {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let what = match self {
            Self::Network => "network: the Network's internet server config".to_string(),
            Self::Cubic128k => "cubic128k: Cubic from a 128 KiB window".to_string(),
            Self::Fixed(window) => format!("fixed:{window}: a fixed {} KiB window", window / 1024),
        };
        write!(f, "LANKVM_BENCH_CC={what}. Latency less the one-way delay (ms), of frames captured after {WARM_UP:?} of {RUN:?}")
    }
}

/// The server config the host accepts the viewer with, on a connection sending at `pace` (only
/// the `network` controller reads it).
fn server_config(host: &Network, cc: &Cc, pace: &Pace) -> Arc<quinn::ServerConfig> {
    let controller: Arc<dyn ControllerFactory + Send + Sync> = match *cc {
        Cc::Network => return host_server_config(host, pace),
        Cc::Cubic128k => {
            let mut cubic = CubicConfig::default();
            cubic.initial_window(128 * 1024);
            Arc::new(cubic)
        }
        Cc::Fixed(window) => Arc::new(FixedWindow(window)),
    };
    let mut config = (*host.internet_server_config(pace)).clone();
    config.transport_config(Arc::new(internet_transport(controller)));
    Arc::new(config)
}

/// What production accepts a connection from the internet with, sending at `pace`.
fn host_server_config(host: &Network, pace: &Pace) -> Arc<quinn::ServerConfig> {
    host.internet_server_config(pace)
}

/// The host's internet transport settings as `endpoint.rs` makes them (`transport_config`, over
/// the internet), with `controller` for the congestion controller.
fn internet_transport(controller: Arc<dyn ControllerFactory + Send + Sync>) -> TransportConfig {
    let mut t = TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_millis(20)));
    t.max_idle_timeout(Some(Duration::from_secs(15).try_into().unwrap()));
    t.datagram_send_buffer_size(4 * 1024 * 1024);
    t.congestion_controller_factory(controller);
    t.initial_mtu(1200);
    t.max_concurrent_bidi_streams(1u32.into());
    t.max_concurrent_uni_streams(2u32.into());
    t.receive_window((8u32 * 1024 * 1024).into());
    t.datagram_receive_buffer_size(Some(64 * 1024));
    t
}

/// A congestion window that never changes, like `transport::cc::LanController`'s but of any size.
#[derive(Clone, Copy, Debug)]
struct FixedWindow(u64);

impl Controller for FixedWindow {
    fn on_congestion_event(&mut self, _now: Instant, _sent: Instant, _persistent: bool, _lost: u64) {}

    fn on_mtu_update(&mut self, _new_mtu: u16) {}

    fn window(&self) -> u64 {
        self.0
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(*self)
    }

    fn initial_window(&self) -> u64 {
        self.0
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl ControllerFactory for FixedWindow {
    fn build(self: Arc<Self>, _now: Instant, _current_mtu: u16) -> Box<dyn Controller> {
        Box::new(*self)
    }
}

/// Streams over `path`, the host on `cc`.
async fn run(path: Path, cc: &Cc) -> Row {
    let host = network("host");
    let viewer = network("viewer");
    let relay = Relay::start(path, host.endpoint.local_addr().unwrap(), viewer.endpoint.local_addr().unwrap());
    let pace = Pace::new();
    let accept = tokio::spawn({
        let (host, config) = (host.clone(), server_config(&host, cc, &pace));
        async move { host.endpoint.accept().await.unwrap().accept_with(config).unwrap().await.unwrap() }
    });
    let connecting = viewer.endpoint.connect_with(viewer.internet_client_config([7; 32]), relay.addr, "lankvm").unwrap();
    let viewer_conn = connecting.await.unwrap();
    let host_conn = accept.await.unwrap();

    let start = Instant::now();
    let progress = Arc::new(Progress::default());
    let watching = tokio::spawn(watch(viewer_conn.clone(), start, progress.clone()));
    let sent = stream(&host_conn, &pace, start).await;
    // Until every frame sent arrived, or nothing has for a while: the rest is lost.
    let quiet = path.one_way * 2 + Duration::from_millis(250);
    let give_up = Instant::now() + Duration::from_secs(3);
    while progress.frames.load(Ordering::Relaxed) < sent.frames.len()
        && Instant::now() < give_up
        && start.elapsed().saturating_sub(Duration::from_nanos(progress.last_ns.load(Ordering::Relaxed))) < quiet
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let stats = host_conn.stats();
    viewer_conn.close(0u32.into(), b"done");
    host_conn.close(0u32.into(), b"done");
    let delivered = watching.await.unwrap();
    relay.stop();
    Row::new(path, &sent, &delivered, stats)
}

fn network(side: &str) -> Network {
    let dir = std::env::temp_dir().join(format!("lankvm-bench-wan-{side}-{}", std::process::id()));
    Network::bind("127.0.0.1:0".parse().unwrap(), &DeviceIdentity::load_or_create(&dir).unwrap()).unwrap()
}

/// What the host side did: the frames it sent, when the ones held back and replaced were
/// captured, and the ceiling after each tick (all times since the run's start).
#[derive(Default)]
struct Sent {
    frames: Vec<SentFrame>,
    skipped: Vec<Duration>,
    ceilings: Vec<(Duration, u32)>,
}

/// A frame sent: its id is its place in [`Sent::frames`].
struct SentFrame {
    captured: Duration,
    bytes: usize,
}

/// Captures and sends frames for [`RUN`] as the host's encode thread does (see the module's
/// description), and runs the rate controller and sets `pace` as `follow_the_connection` does.
async fn stream(conn: &Connection, pace: &Pace, start: Instant) -> Sent {
    let empty_space = conn.datagram_send_buffer_space();
    let backlog = || empty_space.saturating_sub(conn.datagram_send_buffer_space());
    let mut control = RateControl::new(LAN_BPS);
    pace.set(rate::pace_for(control.ceiling()));
    let mut profile = Profile::new();
    let mut sent = Sent::default();
    let mut frames = tokio::time::interval(Duration::from_secs_f64(1.0 / FPS));
    let mut recheck = tokio::time::interval(RECHECK);
    let mut ticks = tokio::time::interval(rate::TICK);
    ticks.tick().await;
    let (mut at, mut stats) = (Instant::now(), conn.stats());
    // The capture waiting to go out: when it was taken, and its nominal size.
    let mut waiting: Option<(Duration, usize)> = None;
    while start.elapsed() < RUN {
        tokio::select! {
            _ = frames.tick() => {
                let (captured, mut size) = (start.elapsed(), profile.next());
                if let Some((older, older_size)) = waiting {
                    // Replaced: the newer capture carries its changes too, so it is at least as
                    // big.
                    sent.skipped.push(older);
                    size = size.max(older_size);
                }
                waiting = Some((captured, size));
            }
            _ = recheck.tick() => {}
            _ = ticks.tick() => {
                let (now, next) = (Instant::now(), conn.stats());
                let sample = Sample::between(&stats, &next, now - at, backlog());
                control.on_sample(&sample);
                pace.set(rate::pace_for(control.ceiling()));
                if std::env::var_os("LANKVM_BENCH_TRACE").is_some() {
                    eprintln!("{:>5.2}s ceil {:>5.1} pace {:>6.1} sent {:>5.1} Mb/s lost {:>3}/{:<4} rtt {:>5.1} backlog {:>7}",
                        start.elapsed().as_secs_f64(), f64::from(control.ceiling()) / 1e6, rate::pace_for(control.ceiling()) as f64 / 1e6,
                        sample.sent_bytes as f64 * 8.0 / sample.interval.as_secs_f64() / 1e6, sample.lost_packets, sample.sent_packets,
                        sample.rtt.as_secs_f64() * 1e3, sample.backlog);
                }
                (at, stats) = (now, next);
                sent.ceilings.push((start.elapsed(), control.ceiling()));
            }
        }
        let Some((captured, size)) = waiting else { continue };
        if backlog() > rate::backlog_limit(control.ceiling()) {
            continue;
        }
        waiting = None;
        let bytes = encoded(size, control.ceiling());
        send_frame(conn, sent.frames.len() as u32, captured, bytes);
        sent.frames.push(SentFrame { captured, bytes });
    }
    sent.skipped.extend(waiting.map(|(captured, _)| captured));
    sent
}

/// Sends `bytes` of frame `frame` in datagrams as large as the path takes, each with its header.
fn send_frame(conn: &Connection, frame: u32, captured: Duration, bytes: usize) {
    let room = conn.max_datagram_size().unwrap() - Header::LEN;
    let count = bytes.div_ceil(room);
    for index in 0..count {
        let mut datagram = Vec::with_capacity(Header::LEN + room);
        Header { frame, index: index as u16, count: count as u16, captured }.write(&mut datagram);
        datagram.resize(Header::LEN + room.min(bytes - index * room), 0);
        conn.send_datagram(datagram.into()).unwrap();
    }
}

/// What each datagram starts with: its frame, its place among the frame's datagrams, and when
/// the frame was captured, since the run's start (the viewer is in the same process, on the same
/// clock).
struct Header {
    frame: u32,
    index: u16,
    count: u16,
    captured: Duration,
}

impl Header {
    const LEN: usize = 16;

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.frame.to_le_bytes());
        out.extend_from_slice(&self.index.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&(self.captured.as_nanos() as u64).to_le_bytes());
    }

    fn read(bytes: &[u8]) -> Self {
        Self {
            frame: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            index: u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
            count: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
            captured: Duration::from_nanos(u64::from_le_bytes(bytes[8..16].try_into().unwrap())),
        }
    }
}

/// What the viewer got so far: frames put together, and when a datagram last arrived (ns since
/// the run's start).
#[derive(Default)]
struct Progress {
    frames: AtomicUsize,
    last_ns: AtomicU64,
}

/// Puts the frames back together until the connection closes; returns each delivered frame's
/// raw latency, by id.
async fn watch(conn: Connection, start: Instant, progress: Arc<Progress>) -> HashMap<u32, Duration> {
    let mut parts: HashMap<u32, u16> = HashMap::new();
    let mut delivered = HashMap::new();
    while let Ok(datagram) = conn.read_datagram().await {
        let now = start.elapsed();
        progress.last_ns.store(now.as_nanos() as u64, Ordering::Relaxed);
        let header = Header::read(&datagram);
        assert!(header.index < header.count, "datagram {} of {}", header.index, header.count);
        let got = parts.entry(header.frame).or_default();
        *got += 1;
        if *got == header.count {
            parts.remove(&header.frame);
            delivered.insert(header.frame, now.saturating_sub(header.captured));
            progress.frames.fetch_add(1, Ordering::Relaxed);
        }
    }
    delivered
}

/// Nominal frame sizes, as a desktop makes them: mostly small updates (typing, the cursor:
/// [`SMALL`]), [`MEDIUM_SHARE`] of them medium ([`MEDIUM`]), and every [`BIG_EVERY`] frames a
/// big one ([`BIG`]: a scroll, a window's tiles anew). The same sizes every run.
struct Profile {
    random: XorShift,
    /// Frames until the next big one.
    big_in: usize,
}

impl Profile {
    fn new() -> Self {
        Self { random: XorShift(0x2545_F491_4F6C_DD1D), big_in: 0 }
    }

    /// The next capture's nominal size (bytes).
    fn next(&mut self) -> usize {
        if self.big_in == 0 {
            self.big_in = self.random.between(BIG_EVERY) - 1;
            return self.random.between(BIG);
        }
        self.big_in -= 1;
        if self.random.chance(MEDIUM_SHARE) { self.random.between(MEDIUM) } else { self.random.between(SMALL) }
    }
}

/// The profile's average bitrate: about 8.2 Mbit/s.
fn nominal_bps() -> f64 {
    let mid = |(low, high): (usize, usize)| (low + high) as f64 / 2.0;
    let others = (1.0 - MEDIUM_SHARE) * mid(SMALL) + MEDIUM_SHARE * mid(MEDIUM);
    (mid(BIG) + (mid(BIG_EVERY) - 1.0) * others) / mid(BIG_EVERY) * 8.0 * FPS
}

/// The size of a frame of `nominal` size, encoded for a `ceiling_bps` stream. Below the profile's
/// average ([`nominal_bps`]), every frame shrinks in proportion, as an encoder told that bitrate
/// makes them: the average follows the ceiling, and big frames stay big next to the others (a
/// clamp would flatten exactly the bursts this measures). Above it, frames keep their size: a
/// screen changing this little doesn't fill a higher bitrate, and the bursts stay the sizes a
/// desktop makes rather than growing with the ceiling. (The rate controller doesn't raise a
/// ceiling the stream uses less than 70% of, so it stays near 12 Mbit/s on a path with room.)
fn encoded(nominal: usize, ceiling_bps: u32) -> usize {
    let scale = (f64::from(ceiling_bps) / nominal_bps()).min(1.0);
    ((nominal as f64 * scale) as usize).max(1)
}

/// xorshift64: the same numbers every run.
struct XorShift(u64);

impl XorShift {
    /// In [0, 1).
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }

    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// In [low, high].
    fn between(&mut self, (low, high): (usize, usize)) -> usize {
        low + (self.unit() * (high - low + 1) as f64) as usize
    }
}

/// The path, as a socket of its own between the viewer and the host. One thread takes the packets
/// in and decides their fate; one for each direction sends them on when they are due.
struct Relay {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

type Due = mpsc::Sender<(Instant, Vec<u8>)>;

impl Relay {
    fn start(path: Path, host: SocketAddr, viewer: SocketAddr) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let udp = quinn::udp::UdpSocketState::new((&socket).into()).unwrap();
        udp.set_recv_buffer_size((&socket).into(), RELAY_BUFFER).unwrap();
        udp.set_send_buffer_size((&socket).into(), RELAY_BUFFER).unwrap();
        // Blocking again (quinn's state makes it non-blocking); the timeout is for `stop`.
        socket.set_nonblocking(false).unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let addr = socket.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (to_viewer, to_viewer_rx) = mpsc::channel();
        let (to_host, to_host_rx) = mpsc::channel();
        let threads = vec![
            std::thread::spawn({
                let socket = socket.try_clone().unwrap();
                move || deliver(socket, to_viewer_rx, viewer)
            }),
            std::thread::spawn({
                let socket = socket.try_clone().unwrap();
                move || deliver(socket, to_host_rx, host)
            }),
            std::thread::spawn({
                let stop = stop.clone();
                move || shape(socket, path, host, to_viewer, to_host, &stop)
            }),
        ];
        Self { addr, stop, threads }
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads {
            thread.join().unwrap();
        }
    }
}

/// Takes the packets in until `stop`. The viewer's go on to the host after the delay. The host's
/// may be lost at random; then they queue for the bottleneck, if any, and are dropped when they
/// find its queue full; then they take the delay.
fn shape(socket: UdpSocket, path: Path, host: SocketAddr, to_viewer: Due, to_host: Due, stop: &AtomicBool) {
    system::set_thread_interactive();
    let mut random = XorShift(0x9E37_79B9_7F4A_7C15);
    let mut busy_until = Instant::now();
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        let Ok((len, from)) = socket.recv_from(&mut buf) else { continue };
        let now = Instant::now();
        let packet = buf[..len].to_vec();
        if from != host {
            let _ = to_host.send((now + path.one_way, packet));
            continue;
        }
        if random.chance(path.loss) {
            continue;
        }
        let mut sent_at = now;
        if let Some(link) = path.bottleneck {
            let queued = busy_until.saturating_duration_since(now).as_secs_f64() * link.bps / 8.0;
            if queued + len as f64 > link.queue as f64 {
                continue;
            }
            busy_until = busy_until.max(now) + Duration::from_secs_f64(len as f64 * 8.0 / link.bps);
            sent_at = busy_until;
        }
        let _ = to_viewer.send((sent_at + path.one_way, packet));
    }
}

/// Sends each packet on to `to` when it is due (they come due in order).
fn deliver(socket: UdpSocket, packets: mpsc::Receiver<(Instant, Vec<u8>)>, to: SocketAddr) {
    system::set_thread_interactive();
    for (due, packet) in packets {
        wait_until(due);
        let _ = socket.send_to(&packet, to);
    }
}

/// Waits until `due`, to within a few µs. macOS lets a sleep run late by up to half its length
/// (timer coalescing; measured 5 ms late for 10 ms), so this sleeps a third of what is left at a
/// time, and spins over the last [`SPIN`].
fn wait_until(due: Instant) {
    loop {
        let left = due.saturating_duration_since(Instant::now());
        if left <= SPIN {
            break;
        }
        std::thread::sleep(left / 3);
    }
    while Instant::now() < due {
        std::hint::spin_loop();
    }
}

/// One path's results, over the frames captured after [`WARM_UP`].
struct Row {
    path: Path,
    /// Frames delivered a second, frames held back and replaced, frames sent but never complete.
    fps: f64,
    skipped: usize,
    lost: usize,
    /// Each delivered frame's latency (ms, sorted): less the one-way delay, and raw.
    latency: Vec<f64>,
    raw: Vec<f64>,
    /// The video delivered, and the ceiling on average (Mbit/s).
    video_mbps: f64,
    ceiling_mbps: f64,
    /// The host's QUIC statistics at the end.
    stats: ConnectionStats,
}

const TITLE: &str = "path               fps  skip  lost |    p50    p95    p99     max | raw p50 | video  ceil | cwnd KiB  rtt ms  lost/sent pkts  cong";

impl Row {
    fn new(path: Path, sent: &Sent, delivered: &HashMap<u32, Duration>, stats: ConnectionStats) -> Self {
        let counted = |captured: &Duration| *captured >= WARM_UP;
        let seconds = (RUN - WARM_UP).as_secs_f64();
        let (mut latency, mut raw, mut bytes, mut lost) = (Vec::new(), Vec::new(), 0, 0);
        for (id, frame) in sent.frames.iter().enumerate().filter(|(_, f)| counted(&f.captured)) {
            match delivered.get(&(id as u32)) {
                Some(&took) => {
                    raw.push(ms(took));
                    latency.push(ms(took) - ms(path.one_way));
                    bytes += frame.bytes;
                }
                None => lost += 1,
            }
        }
        latency.sort_by(f64::total_cmp);
        raw.sort_by(f64::total_cmp);
        let ceilings: Vec<f64> = sent.ceilings.iter().filter(|(t, _)| counted(t)).map(|(_, c)| f64::from(*c) / 1e6).collect();
        Self {
            path,
            fps: latency.len() as f64 / seconds,
            skipped: sent.skipped.iter().filter(|t| counted(t)).count(),
            lost,
            latency,
            raw,
            video_mbps: bytes as f64 * 8.0 / seconds / 1e6,
            ceiling_mbps: ceilings.iter().sum::<f64>() / ceilings.len() as f64,
            stats,
        }
    }
}

impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let p = |q| percentile(&self.latency, q);
        let path = &self.stats.path;
        write!(
            f,
            "{:<16} {:>5.1} {:>5} {:>5} | {:>6.1} {:>6.1} {:>6.1} {:>7.1} | {:>7.1} | {:>5.1} {:>5.1} | {:>8} {:>7.1} {:>7}/{:<7} {:>4}",
            self.path.name(),
            self.fps,
            self.skipped,
            self.lost,
            p(0.5),
            p(0.95),
            p(0.99),
            p(1.0),
            percentile(&self.raw, 0.5),
            self.video_mbps,
            self.ceiling_mbps,
            path.cwnd / 1024,
            ms(path.rtt),
            path.lost_packets,
            path.sent_packets,
            path.congestion_events,
        )
    }
}

/// The `q` quantile of `sorted` (nearest rank); NaN if empty.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.clamp(1, sorted.len().max(1)) - 1).copied().unwrap_or(f64::NAN)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}
