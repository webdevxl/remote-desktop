//! Client role: connect to a host, receive and decode its screen, hand frames to the view.

use std::ffi::c_void;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use platform_mac::decoder::Decoder;
use platform_mac::{CFRetained, CVPixelBuffer, clock, system};
use protocol::{ClientMsg, Codec, DEFAULT_PORT, HostMsg, PROTOCOL_VERSION, VideoFrame};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::sync::mpsc;
use transport::endpoint::peer_fingerprint;
use transport::framing::{read_msg, write_msg};
use transport::identity::Fingerprint;
use transport::pairing::client_start;
use serde::Serialize;
use transport::video::Reassembler;

use crate::Trust;
use crate::stats::{FrameTiming, Stats, StatsView};
use crate::view::{ViewHandle, ViewSlot};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_millis(500);
/// A frame missing packets for this long is considered lost even if no newer frame arrives.
const PARTIAL_FRAME_TIMEOUT: Duration = Duration::from_millis(60);
const KEYFRAME_RETRY: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub host_name: String,
    pub address: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Codec,
}

#[derive(Debug)]
pub enum SessionEvent {
    /// First connection to this host: the user must type the PIN it shows.
    PinNeeded,
    Connected(SessionInfo),
    Ended { error: Option<String> },
}

pub struct ReadyFrame {
    pub pixel_buffer: CFRetained<CVPixelBuffer>,
    pub timing: FrameTiming,
}

// SAFETY: CVPixelBuffer is a thread-safe, reference-counted CoreFoundation object.
unsafe impl Send for ReadyFrame {}

pub type SessionEvents = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// State shared between the network task and the render thread.
#[derive(Default)]
pub struct Shared {
    /// Hand-off to the render thread: holds only the newest decoded frame, never a queue.
    pub slot: Arc<ViewSlot>,
    pub stats: Mutex<Stats>,
    /// Timing of the access unit currently inside the (synchronous) decoder.
    decoding: Mutex<FrameTiming>,
}

pub struct Session {
    shared: Arc<Shared>,
    conn: Arc<Mutex<Option<Connection>>>,
    pin_tx: mpsc::UnboundedSender<String>,
    task: tokio::task::JoinHandle<()>,
    view: Mutex<Option<ViewHandle>>,
}

impl Session {
    pub fn start(
        rt: &tokio::runtime::Handle,
        endpoint: Endpoint,
        trust: Arc<Trust>,
        target: String,
        max_size: (u32, u32),
        events: SessionEvents,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let conn = Arc::new(Mutex::new(None));
        let (pin_tx, pin_rx) = mpsc::unbounded_channel();
        let task = rt.spawn({
            let (shared, conn) = (shared.clone(), conn.clone());
            async move {
                let ctx = RunCtx { trust, max_size, shared, conn_slot: conn, events: events.clone(), pin_rx };
                let result = run(endpoint, &target, ctx).await;
                let error = result.err().map(|e| format!("{e:#}"));
                if let Some(e) = &error {
                    tracing::info!("session ended: {e}");
                }
                events(SessionEvent::Ended { error });
            }
        });
        Self { shared, conn, pin_tx, task, view: Mutex::new(None) }
    }

    /// Starts drawing frames into `layer` (a `CAMetalLayer`) on a dedicated render thread.
    ///
    /// # Safety
    /// `layer` must be a valid `CAMetalLayer`.
    pub unsafe fn attach_view(&self, layer: *mut c_void, width: u32, height: u32) -> Result<()> {
        let handle = unsafe { ViewHandle::attach(layer, width, height, self.shared.clone()) }?;
        *self.view.lock().unwrap() = Some(handle);
        Ok(())
    }

    pub fn resize_view(&self, width: u32, height: u32) {
        self.shared.slot.resize(width, height);
    }

    /// Stops the render thread; returns once it no longer touches the layer.
    pub fn detach_view(&self) {
        self.view.lock().unwrap().take();
    }

    pub fn stats(&self) -> StatsView {
        self.shared.stats.lock().unwrap().view()
    }

    /// Hands the PIN the user typed to the pairing step.
    pub fn submit_pin(&self, pin: String) {
        let _ = self.pin_tx.send(pin);
    }

    pub fn close(&self) {
        if let Some(conn) = self.conn.lock().unwrap().as_ref() {
            conn.close(0u32.into(), b"viewer closed");
        }
        self.task.abort();
        self.detach_view();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

async fn resolve(target: &str) -> Result<SocketAddr> {
    let target = target.trim();
    if let Ok(addr) = target.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let with_port = if target.contains(':') { target.to_string() } else { format!("{target}:{DEFAULT_PORT}") };
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(&with_port)
        .await
        .with_context(|| format!("can't resolve {target}"))?
        .collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied().ok_or_else(|| anyhow!("no address for {target}"))
}

struct RunCtx {
    trust: Arc<Trust>,
    max_size: (u32, u32),
    shared: Arc<Shared>,
    conn_slot: Arc<Mutex<Option<Connection>>>,
    events: SessionEvents,
    pin_rx: mpsc::UnboundedReceiver<String>,
}

async fn run(endpoint: Endpoint, target: &str, mut ctx: RunCtx) -> Result<()> {
    let addr = resolve(target).await?;
    let conn = tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(addr, "lankvm")?)
        .await
        .map_err(|_| anyhow!("no answer from {addr} — is LanKVM running there?"))?
        .context("connect")?;
    *ctx.conn_slot.lock().unwrap() = Some(conn.clone());
    let host_fp = peer_fingerprint(&conn).context("host sent no certificate")?;
    let trusts_host = ctx.trust.hosts.lock().unwrap().contains(&host_fp);

    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        device_name: system::device_name(),
        max_width: ctx.max_size.0,
        max_height: ctx.max_size.1,
        fps: 60,
        trusts_host,
    };
    write_msg(&mut send, &hello).await?;
    let info = loop {
        match read_msg::<HostMsg>(&mut recv).await?.context("host closed the connection")? {
            HostMsg::Welcome { device_name, width, height, fps, codec } => {
                if !trusts_host {
                    ctx.trust.hosts.lock().unwrap().add(host_fp, &device_name)?;
                }
                break SessionInfo { host_name: device_name, address: addr.to_string(), width, height, fps, codec };
            }
            HostMsg::PairingRequired => {
                (ctx.events)(SessionEvent::PinNeeded);
                let pin = ctx.pin_rx.recv().await.context("pairing cancelled")?;
                if let Err(e) = pair(&mut send, &mut recv, &ctx.trust.fingerprint, &host_fp, &pin).await {
                    conn.close(0u32.into(), b"pairing failed");
                    return Err(e);
                }
            }
            HostMsg::Rejected { reason } => {
                conn.close(0u32.into(), b"rejected");
                // Hosts from protocol 1 said only "protocol version N not supported (host speaks 1)".
                if reason.starts_with("protocol version ") {
                    bail!("LanKVM on {addr} is older than this Mac's. Update LanKVM there, then quit and reopen it.");
                }
                bail!("{reason}");
            }
            other => bail!("unexpected reply {other:?}"),
        }
    };
    let RunCtx { shared, events, .. } = ctx;
    tracing::info!(?info, "connected");
    events(SessionEvent::Connected(info));

    // Control stream: one writer task fed by a channel, one reader task for pongs.
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = ctl_rx.recv().await {
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });
    let reader = tokio::spawn({
        let shared = shared.clone();
        async move {
            while let Ok(Some(msg)) = read_msg::<HostMsg>(&mut recv).await {
                if let HostMsg::Pong { client_time_us, host_time_us } = msg {
                    shared.stats.lock().unwrap().clock.add(client_time_us, clock::now_us(), host_time_us);
                }
            }
        }
    });
    let pinger = tokio::spawn({
        let ctl_tx = ctl_tx.clone();
        async move {
            let mut tick = tokio::time::interval(PING_INTERVAL);
            loop {
                tick.tick().await;
                if ctl_tx.send(ClientMsg::Ping { client_time_us: clock::now_us() }).is_err() {
                    break;
                }
            }
        }
    });

    let result = receive_video(&conn, &shared, &ctl_tx).await;
    pinger.abort();
    reader.abort();
    writer.abort();
    result
}

/// Client half of PIN pairing (see `transport::pairing`).
async fn pair(send: &mut SendStream, recv: &mut RecvStream, client_fp: &Fingerprint, host_fp: &Fingerprint, pin: &str) -> Result<()> {
    let (pairing, spake) = client_start(pin, client_fp, host_fp);
    write_msg(send, &ClientMsg::PairStart { spake }).await?;
    let Some(HostMsg::PairReply { spake, mac }) = read_msg(recv).await? else {
        bail!("host ended pairing");
    };
    let mac = pairing
        .finish(&spake, &mac)
        .map_err(|_| anyhow!("Wrong code. Connect again to get a new one."))?;
    write_msg(send, &ClientMsg::PairConfirm { mac }).await
}

/// Datagrams → frames → hardware decoder. Runs until the connection closes.
async fn receive_video(
    conn: &Connection,
    shared: &Arc<Shared>,
    ctl: &mpsc::UnboundedSender<ClientMsg>,
) -> Result<()> {
    let mut reassembler = Reassembler::new();
    let mut decoder: Option<Decoder> = None;
    let mut need_keyframe = true;
    let mut last_request: Option<Instant> = None;
    let mut check = tokio::time::interval(Duration::from_millis(20));

    let request_keyframe = |reassembler: &mut Reassembler, last_request: &mut Option<Instant>| {
        if last_request.is_some_and(|t| t.elapsed() < KEYFRAME_RETRY) {
            return;
        }
        *last_request = Some(Instant::now());
        reassembler.clear_partial();
        shared.stats.lock().unwrap().keyframe_requests += 1;
        let _ = ctl.send(ClientMsg::RequestKeyframe);
    };

    loop {
        tokio::select! {
            datagram = conn.read_datagram() => {
                let datagram = match datagram {
                    Ok(d) => d,
                    Err(quinn::ConnectionError::ApplicationClosed(_) | quinn::ConnectionError::LocallyClosed) => return Ok(()),
                    Err(e) => return Err(e).context("connection lost"),
                };
                let Some(frame) = reassembler.push(&datagram) else { continue };
                let received_us = clock::now_us();
                if frame.skipped > 0 {
                    shared.stats.lock().unwrap().frames_lost += u64::from(frame.skipped);
                    need_keyframe = true;
                }
                let video: VideoFrame = match protocol::decode(&frame.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("bad frame: {e}");
                        continue;
                    }
                };
                record_arrival(shared, &video, frame.data.len(), received_us);

                if video.keyframe {
                    if !decoder.as_ref().is_some_and(|d| d.matches(video.codec, &video.param_sets)) {
                        decoder = Some(new_decoder(&video, shared)?);
                    }
                    need_keyframe = false;
                    last_request = None;
                }
                if need_keyframe {
                    request_keyframe(&mut reassembler, &mut last_request);
                    continue;
                }
                let Some(dec) = decoder.as_ref() else {
                    need_keyframe = true;
                    continue;
                };
                {
                    let mut timing = shared.decoding.lock().unwrap();
                    timing.received_us = received_us;
                    timing.capture_local_us = shared.stats.lock().unwrap().clock.to_local(video.capture_time_us);
                }
                if let Err(e) = dec.decode(&video.data, 0) {
                    tracing::warn!("decode: {e:#}");
                    need_keyframe = true;
                }
            }
            _ = check.tick() => {
                if need_keyframe || reassembler.has_stale_partial(PARTIAL_FRAME_TIMEOUT) {
                    need_keyframe = true;
                    request_keyframe(&mut reassembler, &mut last_request);
                }
            }
        }
    }
}

fn record_arrival(shared: &Shared, video: &VideoFrame, bytes: usize, received_us: u64) {
    let mut stats = shared.stats.lock().unwrap();
    stats.on_frame_received(bytes);
    stats.capture.add(video.encode_start_us.saturating_sub(video.capture_time_us) as f64);
    stats.encode.add(video.encoded_time_us.saturating_sub(video.encode_start_us) as f64);
    if let Some(encoded_local) = stats.clock.to_local(video.encoded_time_us) {
        stats.network.add(received_us.saturating_sub(encoded_local) as f64);
    }
}

fn new_decoder(video: &VideoFrame, shared: &Arc<Shared>) -> Result<Decoder> {
    let shared = shared.clone();
    Decoder::new(video.codec, &video.param_sets, video.nal_length_size, move |decoded| {
        let mut timing = *shared.decoding.lock().unwrap();
        timing.decoded_us = clock::now_us();
        {
            let mut stats = shared.stats.lock().unwrap();
            stats.decode.add(timing.decoded_us.saturating_sub(timing.received_us) as f64);
            stats.frames_decoded += 1;
        }
        shared.slot.publish(ReadyFrame { pixel_buffer: decoded.pixel_buffer, timing });
    })
}
