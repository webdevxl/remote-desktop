//! Client role: connect to a host, receive and decode its screen, hand frames to the view.

use std::ffi::c_void;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use platform_mac::decoder::Decoder;
use platform_mac::{CFRetained, CVPixelBuffer, clock, system};
use protocol::{
    Arrangement, ClientMsg, Codec, ControlState, CursorState, DEFAULT_PORT, DisplayChoice, DisplayReason, DisplayState, HostMsg, InputMsg,
    PROTOCOL_VERSION, VideoFrame,
};
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
/// While frames can't be decoded (a size this Mac's decoder doesn't take), ask for a keyframe only
/// this often rather than flooding the host with requests that can't help.
const UNDECODABLE_RETRY: Duration = Duration::from_secs(2);
/// A display change is announced once its first frame is on screen, or after this long anyway.
const DISPLAY_SHOWN_TIMEOUT: Duration = Duration::from_secs(2);
/// The host says what it shows right after Welcome.
const FIRST_DISPLAY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub host_name: String,
    /// The host's device id (stable across addresses), e.g. to remember per-host choices.
    pub host_id: String,
    /// The host is this same Mac (another copy of LanKVM): pointer and keyboard are shared.
    pub same_machine: bool,
    pub address: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Codec,
    /// The host display shown.
    pub display: DisplayView,
    /// Whether this session may ask for a virtual display (`DisplayReason`, 0 if it may), and why
    /// not in words.
    pub display_available: u16,
    pub display_unavailable: String,
}

/// The host display a session shows, as the UI sees it.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DisplayView {
    /// "main" (the host's own screen) or "virtual" (a display it made for this Mac).
    pub kind: &'static str,
    /// Pixels; for a virtual display, its size (also the stream's).
    pub width: u32,
    pub height: u32,
    pub hidpi: bool,
    pub refresh_hz: u32,
    /// "extend", "main" or "only" (virtual displays).
    pub arrangement: &'static str,
}

impl DisplayView {
    fn new(display: &DisplayChoice, stream: (u32, u32), fps: u32) -> Self {
        match display {
            DisplayChoice::Main => Self { kind: "main", width: stream.0, height: stream.1, hidpi: false, refresh_hz: fps, arrangement: "extend" },
            DisplayChoice::Virtual(spec) => Self {
                kind: "virtual",
                width: spec.width,
                height: spec.height,
                hidpi: spec.hidpi,
                refresh_hz: spec.refresh_hz,
                arrangement: arrangement_name(spec.arrangement),
            },
        }
    }
}

fn arrangement_name(arrangement: Arrangement) -> &'static str {
    match arrangement {
        Arrangement::MAIN => "main",
        Arrangement::ONLY => "only",
        _ => "extend",
    }
}

#[derive(Debug)]
pub enum SessionEvent {
    /// First connection to this host: the user must type the PIN it shows.
    PinNeeded,
    Connected(SessionInfo),
    Ended { error: Option<String> },
    /// The host granted, refused or ended control.
    Control(ControlState),
    /// A cursor image to draw locally while controlling (PNG at 2x; sizes in points).
    CursorShape { id: u32, png: Vec<u8>, width: f32, height: f32, hot_x: f32, hot_y: f32 },
    Cursor(CursorState),
    /// What the session shows now (answering `request`, or 0 when the host changed it), sent once
    /// the new picture is on screen. `reason` (a `DisplayReason`) and `message` say why it isn't
    /// what was asked for, or what happened.
    Display { request: u32, info: SessionInfo, reason: u16, message: String },
    /// The video of this size can't be shown (e.g. this Mac can't decode it).
    StreamError { message: String, width: u32, height: u32 },
}

/// Input written per batch at most; anything more waits for the next write.
const MAX_INPUT_BATCH: usize = 256;

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
    /// Whether the host lets us control it right now. Input is dropped otherwise.
    controlling: AtomicBool,
    /// Id of our latest control request and whether it asked for control; answers to older
    /// ones are stale.
    latest_request: Mutex<(u32, bool)>,
    /// Test hook: sees every decoded frame before it's drawn.
    frame_probe: Mutex<Option<FrameProbe>>,
    /// Id of our latest display request (they count apart from control requests).
    latest_display_request: Mutex<u32>,
    /// What the session shows, as the host last said.
    info: Mutex<Option<SessionInfo>>,
    /// A display change waiting for its first frame (see [`DISPLAY_SHOWN_TIMEOUT`]).
    pending_display: Mutex<Option<PendingDisplay>>,
}

/// A `Display` event to send once a frame of `size` is decoded.
struct PendingDisplay {
    event: (u32, SessionInfo, u16, String),
    size: (u32, u32),
    since: Instant,
}

/// Called with each decoded frame and its timing (see [`Session::set_frame_probe`]).
pub type FrameProbe = Box<dyn Fn(&CVPixelBuffer, FrameTiming) + Send + Sync>;

pub struct Session {
    shared: Arc<Shared>,
    conn: Arc<Mutex<Option<Connection>>>,
    pin_tx: mpsc::UnboundedSender<String>,
    /// Control messages from the app (queued until connected).
    ctl_tx: mpsc::UnboundedSender<ClientMsg>,
    /// Input for the host, with our clock (µs) when it happened.
    input_tx: mpsc::UnboundedSender<(InputMsg, u64)>,
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
        max_fps: u32,
        events: SessionEvents,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let conn = Arc::new(Mutex::new(None));
        let (pin_tx, pin_rx) = mpsc::unbounded_channel();
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let task = rt.spawn({
            let (shared, conn, ctl_tx) = (shared.clone(), conn.clone(), ctl_tx.clone());
            async move {
                let ctx = RunCtx {
                    trust,
                    max_size,
                    max_fps,
                    shared,
                    conn_slot: conn,
                    events: events.clone(),
                    pin_rx,
                    ctl: (ctl_tx, ctl_rx),
                    input_rx,
                };
                let result = run(endpoint, &target, ctx).await;
                let error = result.err().map(|e| format!("{e:#}"));
                if let Some(e) = &error {
                    tracing::info!("session ended: {e}");
                }
                events(SessionEvent::Ended { error });
            }
        });
        Self { shared, conn, pin_tx, ctl_tx, input_tx, task, view: Mutex::new(None) }
    }

    /// Asks the host for control (true) or to only view it (false); `take_over` takes control
    /// from another device that has it. The answer arrives as a [`SessionEvent::Control`] for the
    /// returned request id.
    pub fn set_control(&self, on: bool, take_over: bool) -> u32 {
        let mut latest = self.shared.latest_request.lock().unwrap();
        let request = latest.0.wrapping_add(1).max(1);
        *latest = (request, on);
        if !on {
            // Let go of everything first (the input stream is ordered before the request); the
            // host also releases on its side.
            self.send_input(InputMsg::ReleaseAll);
            self.shared.controlling.store(false, Ordering::Release);
        }
        let _ = self.ctl_tx.send(ClientMsg::SetControl { on, request, take_over });
        request
    }

    /// Asks the host to show `display`. The answer arrives as a [`SessionEvent::Display`] for the
    /// returned request id.
    pub fn set_display(&self, display: DisplayChoice) -> u32 {
        let mut latest = self.shared.latest_display_request.lock().unwrap();
        let request = latest.wrapping_add(1).max(1);
        *latest = request;
        let _ = self.ctl_tx.send(ClientMsg::SetDisplay { request, display });
        request
    }

    /// While controlling: whether this window has the focus and forwards input.
    pub fn set_focus(&self, forwarding: bool) {
        if self.shared.controlling.load(Ordering::Acquire) {
            let _ = self.ctl_tx.send(ClientMsg::Focus { forwarding });
        }
    }

    /// Lets a test look at every decoded frame (e.g. to time how fast input shows on screen).
    pub fn set_frame_probe(&self, probe: Option<FrameProbe>) {
        *self.shared.frame_probe.lock().unwrap() = probe;
    }

    /// Sends input to the host, if it lets us control it.
    pub fn send_input(&self, msg: InputMsg) {
        if self.shared.controlling.load(Ordering::Acquire) {
            let _ = self.input_tx.send((msg, clock::now_us()));
        }
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
    /// The viewer screen's refresh rate: no point streaming faster.
    max_fps: u32,
    shared: Arc<Shared>,
    conn_slot: Arc<Mutex<Option<Connection>>>,
    events: SessionEvents,
    pin_rx: mpsc::UnboundedReceiver<String>,
    ctl: (mpsc::UnboundedSender<ClientMsg>, mpsc::UnboundedReceiver<ClientMsg>),
    input_rx: mpsc::UnboundedReceiver<(InputMsg, u64)>,
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
        fps: ctx.max_fps,
        trusts_host,
    };
    write_msg(&mut send, &hello).await?;
    let info = loop {
        match read_msg::<HostMsg>(&mut recv).await?.context("host closed the connection")? {
            HostMsg::Welcome { device_name, width, height, fps, codec } => {
                if !trusts_host {
                    ctx.trust.hosts.lock().unwrap().add(host_fp, &device_name)?;
                }
                break SessionInfo {
                    host_name: device_name,
                    host_id: transport::identity::short_hex(&host_fp),
                    same_machine: crate::is_this_mac(addr.ip()),
                    address: addr.to_string(),
                    width,
                    height,
                    fps,
                    codec,
                    // The host says what it is right after (a reconnect may find its virtual display).
                    display: DisplayView::new(&DisplayChoice::Main, (width, height), fps),
                    display_available: DisplayReason::NONE.0,
                    display_unavailable: String::new(),
                };
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
    // Right after Welcome the host says what it shows (a device that comes back may find its
    // virtual display) and whether it may ask for one: part of what "connected" means. Giving up
    // waiting ends the session: a message cut off half-way would leave the stream unreadable.
    let mut info = info;
    let first = tokio::time::timeout(FIRST_DISPLAY_TIMEOUT, read_msg::<HostMsg>(&mut recv))
        .await
        .map_err(|_| anyhow!("{} didn't say what it shows", info.host_name))??
        .context("host closed the connection")?;
    let early = match first {
        HostMsg::Display(state) => {
            apply_display(&mut info, &state);
            None
        }
        other => Some(other),
    };
    let RunCtx { shared, events, ctl: (ctl_tx, mut ctl_rx), input_rx, .. } = ctx;
    tracing::info!(?info, "connected");
    *shared.info.lock().unwrap() = Some(info.clone());
    events(SessionEvent::Connected(info));
    if let Some(msg) = early {
        handle_host_msg(msg, &shared, &events);
    }

    // Control stream: one writer task fed by a channel (pings, keyframe requests and the app's
    // control requests), one reader task for everything the host sends.
    let writer = tokio::spawn(async move {
        while let Some(msg) = ctl_rx.recv().await {
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });
    let reader = tokio::spawn({
        let (shared, events) = (shared.clone(), events.clone());
        async move {
            while let Ok(Some(msg)) = read_msg::<HostMsg>(&mut recv).await {
                handle_host_msg(msg, &shared, &events);
            }
        }
    });
    let input = tokio::spawn({
        let (conn, shared) = (conn.clone(), shared.clone());
        async move {
            if let Err(e) = write_input(&conn, input_rx, &shared).await {
                tracing::info!("input stream: {e:#}");
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

    let result = receive_video(&conn, &shared, &ctl_tx, &events).await;
    pinger.abort();
    reader.abort();
    writer.abort();
    input.abort();
    shared.controlling.store(false, Ordering::Release);
    result
}

fn handle_host_msg(msg: HostMsg, shared: &Shared, events: &SessionEvents) {
    match msg {
        HostMsg::Pong { client_time_us, host_time_us } => {
            shared.stats.lock().unwrap().clock.add(client_time_us, clock::now_us(), host_time_us);
        }
        HostMsg::Control(state) => {
            let latest = shared.latest_request.lock().unwrap();
            // An answer to a request we've since replaced would flip the state back: ignore it.
            if state.request != 0 && state.request != latest.0 {
                tracing::debug!(?state, "stale control answer");
                return;
            }
            // The host never grants control unasked, nor when we asked only to view.
            if state.active && (state.request == 0 || !latest.1) {
                tracing::warn!(?state, "ignoring control we didn't ask for");
                return;
            }
            shared.controlling.store(state.active, Ordering::Release);
            drop(latest);
            events(SessionEvent::Control(state));
        }
        HostMsg::CursorShape { id, png, width, height, hot_x, hot_y } => {
            events(SessionEvent::CursorShape { id, png, width, height, hot_x, hot_y });
        }
        HostMsg::Cursor(state) => events(SessionEvent::Cursor(state)),
        HostMsg::InputAck { seq, received_us, injected_us } => shared.stats.lock().unwrap().on_input_ack(seq, received_us, injected_us),
        HostMsg::Display(state) => on_display(state, shared, events),
        other => tracing::debug!("ignoring {other:?}"),
    }
}

/// The host says what the session shows now. That is always the truth about the stream, even when
/// it answers an older request (only the UI cares which request it answers), so it always applies.
fn on_display(state: DisplayState, shared: &Shared, events: &SessionEvents) {
    let (width, height) = (state.width, state.height);
    let mut before = (width, height);
    let Some(info) = ({
        let mut current = shared.info.lock().unwrap();
        current.as_mut().map(|info| {
            before = (info.width, info.height);
            apply_display(info, &state);
            info.clone()
        })
    }) else {
        return;
    };
    let DisplayState { request, display: shown, fps, reason, message, .. } = state;
    tracing::info!(request, width, height, fps, ?shown, ?reason, "display");
    let event = (request, info, reason.0, message);
    let mut pending = shared.pending_display.lock().unwrap();
    // A change still waiting for its picture is overtaken: say it now, in order.
    if let Some(older) = pending.take() {
        emit_display(older.event, events);
    }
    if before != (width, height) {
        // Announce it with its first frame, so the window's idea of the picture (where clicks go)
        // changes when the picture does.
        *pending = Some(PendingDisplay { event, size: (width, height), since: Instant::now() });
    } else {
        emit_display(event, events);
    }
}

/// Updates `info` with what the host says it shows.
fn apply_display(info: &mut SessionInfo, state: &DisplayState) {
    info.width = state.width;
    info.height = state.height;
    info.fps = state.fps;
    info.display = DisplayView::new(&state.display, (state.width, state.height), state.fps);
    info.display_available = state.available.0;
    info.display_unavailable = state.unavailable.clone();
}

fn emit_display((request, info, reason, message): (u32, SessionInfo, u16, String), events: &SessionEvents) {
    events(SessionEvent::Display { request, info, reason, message });
}

/// Sends a waiting display change once a frame of its size is shown, or once it waited too long.
fn flush_display(shared: &Shared, events: &SessionEvents, shown: Option<(u32, u32)>) {
    let mut pending = shared.pending_display.lock().unwrap();
    let due = pending.as_ref().is_some_and(|p| shown == Some(p.size) || p.since.elapsed() >= DISPLAY_SHOWN_TIMEOUT);
    if due && let Some(p) = pending.take() {
        emit_display(p.event, events);
    }
}

/// Writes input to the host on its own stream, opened with the first input. Sends each event
/// as soon as it arrives; only when events pile up (we fell behind) are consecutive moves and
/// scroll steps merged, never across a click or key, so order is kept. If the host gives up on
/// the stream (bad input), it ends control too; the next input after a new grant opens a new one.
async fn write_input(conn: &Connection, mut rx: mpsc::UnboundedReceiver<(InputMsg, u64)>, shared: &Shared) -> Result<()> {
    let mut send: Option<SendStream> = None;
    let mut batch: Vec<InputMsg> = Vec::new();
    let mut bytes = Vec::new();
    let mut seq = 0u64;
    while let Some((first, mut at_us)) = rx.recv().await {
        batch.clear();
        batch.push(first);
        while batch.len() < MAX_INPUT_BATCH {
            let Ok((msg, t)) = rx.try_recv() else { break };
            at_us = t;
            if !batch.last_mut().is_some_and(|last| last.coalesce(&msg)) {
                batch.push(msg);
            }
        }
        bytes.clear();
        for msg in &batch {
            bytes.extend_from_slice(&protocol::encode_framed(msg)?);
        }
        seq += batch.len() as u64;
        shared.stats.lock().unwrap().on_input_sent(seq, at_us);
        // Once more on a new stream if the host stopped reading this one.
        for attempt in 0..2 {
            if send.is_none() {
                let stream = conn.open_uni().await.context("open input stream")?;
                // Ahead of any other stream we might add later (it doesn't outrank video datagrams).
                let _ = stream.set_priority(100);
                send = Some(stream);
            }
            match send.as_mut().expect("opened above").write_all(&bytes).await {
                Ok(()) => break,
                Err(quinn::WriteError::Stopped(code)) => {
                    tracing::info!(%code, attempt, "host stopped reading our input stream");
                    send = None;
                }
                Err(e) => return Err(e).context("write input"),
            }
        }
    }
    Ok(())
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
    events: &SessionEvents,
) -> Result<()> {
    let mut reassembler = Reassembler::new();
    let mut decoder: Option<Decoder> = None;
    let mut need_keyframe = true;
    let mut last_request: Option<Instant> = None;
    let mut check = tokio::time::interval(Duration::from_millis(20));
    // Parameter sets this Mac's decoder refused: their frames can't be shown, however often the
    // host sends a keyframe.
    let mut undecodable: Option<Vec<Vec<u8>>> = None;

    let request_keyframe = |reassembler: &mut Reassembler, last_request: &mut Option<Instant>, undecodable: bool| {
        let retry = if undecodable { UNDECODABLE_RETRY } else { KEYFRAME_RETRY };
        if last_request.is_some_and(|t| t.elapsed() < retry) {
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
                        decoder = None;
                        if undecodable.as_ref() == Some(&video.param_sets) {
                            continue;
                        }
                        match new_decoder(&video, shared) {
                            Ok(d) => {
                                decoder = Some(d);
                                undecodable = None;
                            }
                            Err(e) => {
                                // Keep the session: the host can still switch to something this
                                // Mac decodes (its own screen, a smaller display).
                                tracing::warn!(width = video.width, height = video.height, "decoder: {e:#}");
                                events(SessionEvent::StreamError {
                                    message: format!("This Mac can't decode the {}×{} picture ({e:#}).", video.width, video.height),
                                    width: video.width,
                                    height: video.height,
                                });
                                undecodable = Some(video.param_sets.clone());
                                need_keyframe = true;
                                continue;
                            }
                        }
                    }
                    need_keyframe = false;
                    last_request = None;
                }
                if need_keyframe {
                    request_keyframe(&mut reassembler, &mut last_request, undecodable.is_some());
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
                } else {
                    flush_display(shared, events, Some((video.width, video.height)));
                }
            }
            _ = check.tick() => {
                flush_display(shared, events, None);
                if need_keyframe || reassembler.has_stale_partial(PARTIAL_FRAME_TIMEOUT) {
                    need_keyframe = true;
                    request_keyframe(&mut reassembler, &mut last_request, undecodable.is_some());
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
        if let Some(probe) = shared.frame_probe.lock().unwrap().as_ref() {
            probe(&decoded.pixel_buffer, timing);
        }
        shared.slot.publish(ReadyFrame { pixel_buffer: decoded.pixel_buffer, timing });
    })
}
