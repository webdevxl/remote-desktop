//! Host role: accept viewers from the local network and stream this Mac's screen to them.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use platform_mac::capture::{CaptureConfig, CapturedFrame, Capturer, main_display};
use platform_mac::cursor::{CursorMonitor, CursorUpdate};
use platform_mac::encoder::{EncodedFrame, Encoder, EncoderConfig};
use platform_mac::{clock, permissions, system};
use protocol::{ClientMsg, Codec, ControlReason, ControlState, CursorState, HostMsg, PROTOCOL_VERSION, VideoFrame};
use quinn::{Connection, Endpoint, Incoming, RecvStream, SendStream};
use tokio::sync::mpsc;
use transport::endpoint::peer_fingerprint;
use transport::framing::{read_msg, write_msg};
use transport::identity::{Fingerprint, short_hex};
use transport::net::is_local_network;
use transport::pairing::{generate_pin, host_respond};
use serde::Serialize;
use transport::video::Packetizer;

use crate::control::{Backend, HostSettings, InputCmdSender, InputConfig, InputFailure, InputShared, InputThread};
use crate::{Event, EventSink, Trust};

/// Minimum spacing between keyframes produced on request; a client that keeps losing packets
/// shouldn't turn the stream into all-keyframes.
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// How long the PIN stays valid while someone walks over to the other Mac.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);
/// A frame should leave the encoder within milliseconds; past this, move on to the next one.
const ENCODE_TIMEOUT: Duration = Duration::from_millis(500);
/// Connections from devices not (yet) trusted that are still saying Hello, pairing or starting
/// their stream. More are turned away, so peers that never finish can't pile up. Trusted devices
/// don't count and are never turned away.
const MAX_PENDING: usize = 16;
/// Time a new connection gets to open its control stream and say Hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Messages waiting for a viewer that reads its control stream. A viewer that lets this many
/// pile up isn't reading: it is disconnected rather than buffered for without end.
const OUT_QUEUE: usize = 1024;
/// Messages from a viewer read ahead of the session handling them; beyond this, QUIC flow
/// control makes the viewer wait.
const CLIENT_QUEUE: usize = 64;
/// Control requests and focus changes a viewer may send per second. People make a few; more
/// means a broken or hostile viewer, and each one costs the host real work.
const MAX_CONTROL_REQUESTS_PER_SEC: u32 = 50;
/// Input threads a session may start per second. A viewer gets one per grant at most (another
/// only after the host gave up on its stream, which also ends control).
const MAX_INPUT_THREADS_PER_SEC: u32 = 5;

#[derive(Clone)]
pub struct Viewer {
    pub id: u64,
    pub name: String,
    pub addr: SocketAddr,
    pub fingerprint: Fingerprint,
    pub conn: Connection,
    /// Whether this viewer controls the Mac right now (rather than only viewing it).
    pub controlling: Arc<AtomicBool>,
    /// Reaches the viewer's session, e.g. to stop its control from the host UI.
    pub(crate) session: mpsc::UnboundedSender<SessionEvt>,
}

/// A device asking to pair; the UI shows its PIN until the attempt ends.
#[derive(Clone)]
pub struct PairPrompt {
    pub id: u64,
    pub name: String,
    pub addr: SocketAddr,
    pub pin: String,
    pub conn: Connection,
}

/// What the UI shows about this Mac's host role.
#[derive(Default)]
pub struct HostStatus {
    pub viewers: Vec<Viewer>,
    pub pairing: Vec<PairPrompt>,
}

pub struct HostCtx {
    pub status: Mutex<HostStatus>,
    pub events: EventSink,
    pub trust: Arc<Trust>,
    pub settings: Mutex<HostSettings>,
    pub settings_path: PathBuf,
    /// Where injected input goes (real events unless a test asks to record them).
    pub backend: Backend,
    /// Stamped on every injected event; viewers on this Mac drop events carrying it.
    pub injected_tag: i64,
    /// Runs input threads' async reads.
    pub rt: tokio::runtime::Handle,
    /// Stream the screen (false only in tests without Screen Recording).
    pub video: bool,
    /// Input threads by session, to release everything when the app quits.
    pub inputs: Mutex<Vec<(u64, InputCmdSender)>>,
    pub allow_same_mac_control: bool,
    /// The session allowed to inject right now. Claimed and released only under this lock,
    /// never while holding `status`.
    pub controller: Mutex<Option<ControllerSlot>>,
    /// Tests: inject only into this process's windows.
    pub guard_pid: Option<i32>,
    /// Tests: end control this long after it was granted.
    pub control_ttl: Option<Duration>,
    /// Limits PIN guessing (see [`PairingThrottle`]).
    pub(crate) pairing_throttle: Mutex<PairingThrottle>,
    /// Connections being set up (see [`MAX_PENDING`]).
    pub(crate) pending: AtomicUsize,
}

/// Host status as the UI sees it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatusView {
    pub viewers: Vec<ViewerView>,
    pub pairing: Vec<PairPromptView>,
    pub screen_capture_allowed: bool,
    /// Whether paired Macs may control this one (a setting).
    pub allow_control: bool,
    /// Whether macOS lets LanKVM post input here (Accessibility).
    pub control_permission: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerView {
    pub id: u64,
    pub name: String,
    pub address: String,
    pub device_id: String,
    pub controlling: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairPromptView {
    pub id: u64,
    pub name: String,
    pub address: String,
    pub pin: String,
}

impl HostCtx {
    fn changed(&self) {
        (self.events)(Event::HostChanged);
    }

    pub fn view(&self) -> HostStatusView {
        // Permission checks ask another process; not while holding `status`.
        let screen_capture_allowed = permissions::screen_capture_allowed();
        let control_permission = self.backend.permitted();
        let allow_control = self.settings.lock().unwrap().allow_control;
        let status = self.status.lock().unwrap();
        HostStatusView {
            viewers: status
                .viewers
                .iter()
                .map(|v| ViewerView {
                    id: v.id,
                    name: v.name.clone(),
                    address: v.addr.ip().to_string(),
                    device_id: short_hex(&v.fingerprint),
                    controlling: v.controlling.load(Ordering::Acquire),
                })
                .collect(),
            pairing: status
                .pairing
                .iter()
                .map(|p| PairPromptView { id: p.id, name: p.name.clone(), address: p.addr.ip().to_string(), pin: p.pin.clone() })
                .collect(),
            screen_capture_allowed,
            allow_control,
            control_permission,
        }
    }

    /// Allows or forbids control by paired Macs. Forbidding it ends any control in progress.
    pub fn set_allow_control(&self, allow: bool) {
        {
            let mut settings = self.settings.lock().unwrap();
            settings.allow_control = allow;
            if let Err(e) = settings.save(&self.settings_path) {
                tracing::warn!("save host settings: {e:#}");
            }
        }
        if !allow {
            let message = format!("Remote control was turned off on {}.", system::device_name());
            for v in self.status.lock().unwrap().viewers.iter() {
                let _ = v.session.send(SessionEvt::Revoke(ControlReason::TURNED_OFF, message.clone()));
            }
        }
        self.changed();
    }

    /// Releases every key and button held for remote viewers and waits (up to `timeout`).
    pub fn release_all_input(&self, timeout: Duration) {
        let cmds: Vec<_> = self.inputs.lock().unwrap().iter().map(|(_, c)| c.clone()).collect();
        crate::control::release_all_and_wait(&cmds, timeout);
    }

    /// Takes control back from a viewer; it keeps viewing and may ask again.
    pub fn stop_control(&self, id: u64) {
        let message = format!("Control was stopped on {}.", system::device_name());
        for v in self.status.lock().unwrap().viewers.iter().filter(|v| v.id == id) {
            let _ = v.session.send(SessionEvt::Revoke(ControlReason::STOPPED_BY_HOST, message.clone()));
        }
    }

    /// Takes control back from whoever has it (menu, stop hotkey).
    pub fn stop_all_control(&self) {
        let holder = self.controller.lock().unwrap().as_ref().map(|s| s.session_id);
        if let Some(id) = holder {
            self.stop_control(id);
        }
    }

    pub fn close_viewer(&self, id: u64) {
        for v in self.status.lock().unwrap().viewers.iter().filter(|v| v.id == id) {
            v.conn.close(1u32.into(), b"disconnected by host");
        }
    }

    pub fn deny_pairing(&self, id: u64) {
        for p in self.status.lock().unwrap().pairing.iter().filter(|p| p.id == id) {
            p.conn.close(2u32.into(), b"pairing denied");
        }
    }

    pub fn close_viewers_with(&self, fp: &Fingerprint) {
        for v in self.status.lock().unwrap().viewers.iter().filter(|v| v.fingerprint == *fp) {
            v.conn.close(3u32.into(), b"device forgotten");
        }
    }
}

/// Whether this Mac can actually capture its screen. The quick TCC preflight can be stale (it
/// only updates on restart, and is unreliable for ad-hoc signed builds), so fall back to asking
/// ScreenCaptureKit, which fails without permission.
pub fn can_capture() -> bool {
    permissions::screen_capture_allowed() || main_display().is_ok()
}

pub async fn run(endpoint: Endpoint, ctx: Arc<HostCtx>) {
    while let Some(incoming) = endpoint.accept().await {
        let remote = incoming.remote_address();
        if !is_local_network(remote.ip()) {
            tracing::warn!(%remote, "refused connection from outside the local network");
            incoming.refuse();
            continue;
        }
        let ctx = ctx.clone();
        tokio::spawn(async move {
            match serve(incoming, ctx).await {
                Ok(()) => tracing::info!(%remote, "viewer disconnected"),
                Err(e) => tracing::info!(%remote, "viewer session ended: {e:#}"),
            }
        });
    }
}

async fn serve(incoming: Incoming, ctx: Arc<HostCtx>) -> Result<()> {
    let conn = incoming.await.context("handshake")?;
    let client_fp = peer_fingerprint(&conn).context("viewer sent no certificate")?;
    // The handshake proved the device holds its key, so trust can't be faked here.
    let pending = if ctx.trust.viewers.lock().unwrap().contains(&client_fp) {
        None
    } else {
        let Some(pending) = Pending::enter(&ctx) else {
            conn.close(5u32.into(), b"busy");
            bail!("turned away: too many unknown devices connecting");
        };
        Some(pending)
    };
    let hello = tokio::time::timeout(HELLO_TIMEOUT, async {
        let (send, mut recv) = conn.accept_bi().await.context("open control stream")?;
        let hello = read_msg::<ClientMsg>(&mut recv).await?;
        anyhow::Ok((send, recv, hello))
    });
    let (mut send, mut recv, hello) = hello.await.context("no Hello in time")??;
    let Some(ClientMsg::Hello { version, device_name, max_width, max_height, fps, trusts_host }) = hello else {
        bail!("expected Hello");
    };
    if version != PROTOCOL_VERSION {
        return reject(&conn, &mut send, &version_mismatch(version, &system::device_name())).await;
    }
    if ctx.video && !tokio::task::spawn_blocking(can_capture).await? {
        return reject(&conn, &mut send, "That Mac hasn't allowed Screen Recording for LanKVM yet (System Settings → Privacy & Security), or LanKVM needs a restart there after allowing it.").await;
    }
    let known = ctx.trust.viewers.lock().unwrap().contains(&client_fp);
    if !known || !trusts_host {
        let _slot = match PairingSlot::claim(&ctx) {
            Ok(slot) => slot,
            Err(why) => return reject(&conn, &mut send, &why).await,
        };
        pair(&conn, &mut send, &mut recv, &ctx, client_fp, &device_name).await?;
    }

    let display = if ctx.video { tokio::task::spawn_blocking(main_display).await?? } else { platform_mac::capture::main_display_bounds() };
    let (width, height) = fit_within(display.width, display.height, max_width, max_height);
    let fps = choose_fps(fps, width, height);
    // The bit budget stays at 60 fps worth, so bandwidth doesn't double at 120.
    let bitrate_bps = bitrate_for(width, height, fps.min(60));
    let stream = if ctx.video {
        let capture = CaptureConfig { display_id: display.id, width, height, fps, show_cursor: true };
        let encoder = EncoderConfig { width, height, fps, bitrate_bps, codec: Codec::Hevc };
        let stream_conn = conn.clone();
        let stream = tokio::task::spawn_blocking(move || StreamSession::start(stream_conn, &capture, &encoder))
            .await?
            .context("start screen stream")?;
        Some(SessionGuard(Some(stream)))
    } else {
        None
    };
    let codec = stream.as_ref().map_or(Codec::Hevc, |s| s.codec());

    write_msg(&mut send, &HostMsg::Welcome { device_name: system::device_name(), width, height, fps, codec }).await?;

    tracing::info!(viewer = %device_name, addr = %conn.remote_address(), fingerprint = %short_hex(&client_fp), width, height, fps, bitrate_bps, video = ctx.video, "streaming");

    // From here on several parties talk to the viewer (pongs, control state, cursor shapes,
    // input acks), so one task owns the send side of the control stream.
    let (out, mut out_rx) = mpsc::channel::<HostMsg>(OUT_QUEUE);
    let _writer = AbortOnDrop(tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
    }));
    // What the viewer sends is read only as fast as the session handles it.
    let (client_tx, mut client_rx) = mpsc::channel::<SessionEvt>(CLIENT_QUEUE);
    let _reader = AbortOnDrop(tokio::spawn(async move {
        loop {
            let evt = match read_msg::<ClientMsg>(&mut recv).await {
                Ok(Some(msg)) => SessionEvt::Client(msg),
                Ok(None) => SessionEvt::Closed(None),
                Err(e) => SessionEvt::Closed(Some(e)),
            };
            let closed = matches!(evt, SessionEvt::Closed(_));
            if client_tx.send(evt).await.is_err() || closed {
                break;
            }
        }
    }));
    // Everything else about the session (this Mac's user, the input thread, the cursor monitor)
    // comes here and goes first, so stopping control never waits behind the viewer's traffic.
    let (evt_tx, mut events) = mpsc::unbounded_channel::<SessionEvt>();
    // The viewer opens its input stream with its first input, and a new one if the host gave up
    // on the old one.
    let _acceptor = AbortOnDrop(tokio::spawn({
        let (evt_tx, conn) = (evt_tx.clone(), conn.clone());
        async move {
            while let Ok(recv) = conn.accept_uni().await {
                if evt_tx.send(SessionEvt::InputStream(recv)).is_err() {
                    break;
                }
            }
        }
    }));

    let controlling = Arc::new(AtomicBool::new(false));
    let session_id = conn.stable_id() as u64;
    let _registration = Registration::new(
        Viewer {
            id: session_id,
            name: device_name.clone(),
            addr: conn.remote_address(),
            fingerprint: client_fp,
            conn: conn.clone(),
            controlling: controlling.clone(),
            session: evt_tx.clone(),
        },
        ctx.clone(),
    );
    // Forget Device may have removed this device while its stream started. It closes only the
    // viewers listed, and this one is listed from here on.
    if !ctx.trust.viewers.lock().unwrap().contains(&client_fp) {
        bail!("device was forgotten while connecting");
    }
    drop(pending);
    let mut control = SessionControl {
        ctx: ctx.clone(),
        conn: conn.clone(),
        session_id,
        viewer_name: device_name,
        client_fp,
        same_mac: crate::is_this_mac(conn.remote_address().ip()),
        display_id: display.id,
        shared: Arc::new(InputShared::default()),
        input: None,
        input_starts: RateLimit::default(),
        controlling,
        cursor: None,
        cursor_unknown: true,
        cursor_in_video: Arc::new(AtomicBool::new(true)),
        cursor_apply: Arc::new(Mutex::new(())),
        forwarding: false,
        capturer: stream.as_ref().and_then(|s| s.capturer()),
        out: out.clone(),
        events: evt_tx,
        ttl: None,
    };

    // While controlled, notice within a second if the Accessibility permission is withdrawn.
    let mut permission_check = tokio::time::interval(PERMISSION_CHECK);
    let mut requests = RateLimit::default();
    loop {
        let evt = tokio::select! {
            biased;
            evt = events.recv() => evt,
            evt = client_rx.recv() => evt,
            _ = permission_check.tick() => {
                control.check_permission();
                continue;
            }
        };
        let Some(evt) = evt else { break };
        if let SessionEvt::Client(ClientMsg::SetControl { .. } | ClientMsg::Focus { .. }) = evt
            && !requests.allow(Instant::now(), MAX_CONTROL_REQUESTS_PER_SEC)
        {
            bail!("more than {MAX_CONTROL_REQUESTS_PER_SEC} control requests a second");
        }
        match evt {
            SessionEvt::Client(ClientMsg::RequestKeyframe) => {
                if let Some(stream) = &stream {
                    stream.request_keyframe();
                }
            }
            SessionEvt::Client(ClientMsg::Ping { client_time_us }) => {
                let now = clock::now_us();
                control.shared.last_ping_us.store(now, Ordering::Release);
                // Only for clock sync: skipped if the viewer isn't reading.
                let _ = out.try_send(HostMsg::Pong { client_time_us, host_time_us: now });
            }
            SessionEvt::Client(ClientMsg::SetControl { on, request, take_over }) => control.set(on, take_over, request),
            SessionEvt::Client(ClientMsg::Focus { forwarding }) => control.focus(forwarding),
            SessionEvt::Client(other) => bail!("unexpected message {other:?}"),
            SessionEvt::Closed(None) => break,
            SessionEvt::Closed(Some(e)) => return Err(e),
            SessionEvt::InputStream(recv) => control.attach_input(recv),
            SessionEvt::Revoke(reason, message) => control.revoke(reason, message),
            SessionEvt::Cursor(update) => control.cursor(update),
        }
    }
    Ok(())
}

const PERMISSION_CHECK: Duration = Duration::from_secs(1);

/// Counts events in one-second windows.
#[derive(Default)]
struct RateLimit {
    window_start: Option<Instant>,
    count: u32,
}

impl RateLimit {
    /// Counts one event at `now`; false if that makes more than `max` in the current second.
    fn allow(&mut self, now: Instant, max: u32) -> bool {
        if self.window_start.is_none_or(|start| now.duration_since(start) >= Duration::from_secs(1)) {
            self.window_start = Some(now);
            self.count = 0;
        }
        self.count += 1;
        self.count <= max
    }
}

/// Holds one of the [`MAX_PENDING`] places for connections being set up.
struct Pending(Arc<HostCtx>);

impl Pending {
    fn enter(ctx: &Arc<HostCtx>) -> Option<Self> {
        let before = ctx.pending.fetch_add(1, Ordering::AcqRel);
        let pending = Self(ctx.clone());
        (before < MAX_PENDING).then_some(pending)
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Each pairing attempt is one guess at a 6-digit PIN, and anyone on the network could make
/// them without end. So pairing runs one at a time, and after a few attempts without success it
/// pauses, for twice as long each time.
#[derive(Default)]
pub(crate) struct PairingThrottle {
    /// PINs tried since the last successful pairing.
    attempts: u32,
    paused_until: Option<Instant>,
    busy: bool,
}

const FREE_PAIRING_ATTEMPTS: u32 = 5;
const PAIRING_PAUSE: Duration = Duration::from_secs(30);
const MAX_PAIRING_PAUSE: Duration = Duration::from_secs(3600);

impl PairingThrottle {
    /// Claims the pairing slot, or says (to the viewer) why pairing isn't possible now.
    fn begin(&mut self, now: Instant, host: &str) -> Result<(), String> {
        if self.busy {
            return Err(format!("Another Mac is pairing with {host} right now. Try again in a minute."));
        }
        if let Some(until) = self.paused_until.filter(|until| *until > now) {
            let secs = until.duration_since(now).as_secs_f64().ceil();
            return Err(format!("{host} paused pairing for {secs} s after several wrong codes. Try again then."));
        }
        self.busy = true;
        Ok(())
    }

    fn end(&mut self) {
        self.busy = false;
    }

    /// A PIN is being tried.
    fn attempt(&mut self, now: Instant) {
        self.attempts += 1;
        if self.attempts >= FREE_PAIRING_ATTEMPTS {
            let doublings = (self.attempts - FREE_PAIRING_ATTEMPTS).min(10);
            self.paused_until = Some(now + (PAIRING_PAUSE * (1 << doublings)).min(MAX_PAIRING_PAUSE));
        }
    }

    fn succeeded(&mut self) {
        self.attempts = 0;
        self.paused_until = None;
    }
}

/// The one pairing in progress; frees the slot when dropped.
struct PairingSlot(Arc<HostCtx>);

impl PairingSlot {
    fn claim(ctx: &Arc<HostCtx>) -> Result<Self, String> {
        ctx.pairing_throttle.lock().unwrap().begin(Instant::now(), &system::device_name())?;
        Ok(Self(ctx.clone()))
    }
}

impl Drop for PairingSlot {
    fn drop(&mut self) {
        self.0.pairing_throttle.lock().unwrap().end();
    }
}

/// What a viewer's session reacts to.
pub(crate) enum SessionEvt {
    Client(ClientMsg),
    /// The control stream ended, cleanly or not.
    Closed(Option<anyhow::Error>),
    InputStream(RecvStream),
    /// Control ends on the host's side (its user, a policy change, bad input...), with the
    /// message to show on the viewer.
    Revoke(ControlReason, String),
    Cursor(CursorUpdate),
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The one session allowed to inject input (one controller per Mac).
pub struct ControllerSlot {
    pub session_id: u64,
    pub fingerprint: Fingerprint,
    pub name: String,
    pub(crate) events: mpsc::UnboundedSender<SessionEvt>,
}

/// One viewer's control of this Mac: the policy deciding whether it may, its input thread, and
/// the cursor it draws locally while it controls.
struct SessionControl {
    ctx: Arc<HostCtx>,
    conn: Connection,
    session_id: u64,
    viewer_name: String,
    client_fp: Fingerprint,
    /// The viewer runs on this Mac (a second copy of LanKVM).
    same_mac: bool,
    display_id: u32,
    shared: Arc<InputShared>,
    input: Option<InputThread>,
    input_starts: RateLimit,
    controlling: Arc<AtomicBool>,
    cursor: Option<CursorMonitor>,
    /// The monitor hasn't reported the cursor yet, or can't read its shape: the viewer can't
    /// draw ours, so it stays in the video.
    cursor_unknown: bool,
    /// Whether the captured video should include the cursor. Applied off the runtime by one
    /// update at a time (`cursor_apply`), each applying the latest wish.
    cursor_in_video: Arc<AtomicBool>,
    cursor_apply: Arc<Mutex<()>>,
    /// The viewer's window has the focus (it draws our cursor only then). It says so after each
    /// grant; until then the cursor stays in the video.
    forwarding: bool,
    capturer: Option<Arc<Capturer>>,
    out: mpsc::Sender<HostMsg>,
    events: mpsc::UnboundedSender<SessionEvt>,
    /// Ends control after a test-chosen time (`LANKVM_TEST_CONTROL_TTL`).
    ttl: Option<AbortOnDrop>,
}

impl SessionControl {
    fn active(&self) -> bool {
        self.shared.active.load(Ordering::Acquire)
    }

    fn set(&mut self, on: bool, take_over: bool, request: u32) {
        if !on {
            self.deactivate(ControlReason::NONE, String::new(), request);
            return;
        }
        if let Err((reason, message)) = self.may_control() {
            tracing::info!(viewer = %self.viewer_name, "control refused: {message}");
            self.deactivate(reason, message, request);
            return;
        }
        // Claim the controller slot. Decide under the lock; send messages after releasing it.
        let host = system::device_name();
        let displaced = {
            let mut slot = self.ctx.controller.lock().unwrap();
            let displaced = match slot.as_ref() {
                Some(s) if s.session_id == self.session_id => None,
                // Another device has to ask to take over (two people fighting over one pointer
                // helps nobody); the same device (a second window, a reconnect) just takes over.
                Some(s) if s.fingerprint != self.client_fp && !take_over => {
                    let message = format!("{} is controlling {host} right now.", s.name);
                    drop(slot);
                    tracing::info!(viewer = %self.viewer_name, "control refused: {message}");
                    self.deactivate(ControlReason::IN_USE, message, request);
                    return;
                }
                Some(s) => Some(s.events.clone()),
                None => None,
            };
            *slot = Some(ControllerSlot {
                session_id: self.session_id,
                fingerprint: self.client_fp,
                name: self.viewer_name.clone(),
                events: self.events.clone(),
            });
            displaced
        };
        if let Some(events) = displaced {
            let _ = events.send(SessionEvt::Revoke(ControlReason::TAKEN_OVER, format!("{} took control of {host}.", self.viewer_name)));
        }
        self.activate(request);
    }

    /// Why this viewer may not control the Mac, if it may not. Shown on the viewer, so it names
    /// this Mac.
    fn may_control(&self) -> Result<(), (ControlReason, String)> {
        let host = system::device_name();
        if !self.ctx.settings.lock().unwrap().allow_control {
            return Err((
                ControlReason::TURNED_OFF,
                format!("Remote control is turned off on {host}. It can be turned on under This Mac in LanKVM there."),
            ));
        }
        if !self.ctx.trust.viewers.lock().unwrap().contains(&self.client_fp) {
            return Err((ControlReason::TURNED_OFF, format!("{host} no longer trusts this Mac. Connect again to pair.")));
        }
        if self.client_fp == self.ctx.trust.fingerprint {
            return Err((ControlReason::SELF_CONNECTION, "This LanKVM is connected to itself. Connect from another Mac to control this one.".into()));
        }
        if self.same_mac && !self.ctx.allow_same_mac_control {
            return Err((
                ControlReason::SAME_MAC,
                format!("That's this Mac ({host}). Controlling it from itself would loop, so control is only possible from another Mac."),
            ));
        }
        if !self.ctx.backend.permitted() {
            return Err((ControlReason::NEEDS_PERMISSION, permission_reason(&host)));
        }
        Ok(())
    }

    fn activate(&mut self, request: u32) {
        let was_active = self.shared.active.swap(true, Ordering::AcqRel);
        self.controlling.store(true, Ordering::Release);
        if !was_active {
            tracing::info!(viewer = %self.viewer_name, "controlling this Mac");
            // The viewer tells us when its window has the focus and draws our cursor.
            self.forwarding = false;
        }
        // Each grant starts the cursor over: the viewer forgets the shapes it had, and a fresh
        // monitor sends the current one (its updates queue behind the state sent below).
        if let Some(old) = self.cursor.take() {
            tokio::task::spawn_blocking(move || drop(old));
        }
        self.cursor_unknown = true;
        let events = self.events.clone();
        match CursorMonitor::start(move |u| drop(events.send(SessionEvt::Cursor(u)))) {
            Ok(m) => self.cursor = Some(m),
            Err(e) => tracing::warn!("cursor monitor: {e}"),
        }
        if let Some(ttl) = self.ctx.control_ttl {
            let events = self.events.clone();
            self.ttl = Some(AbortOnDrop(tokio::spawn(async move {
                tokio::time::sleep(ttl).await;
                let _ = events.send(SessionEvt::Revoke(ControlReason::TEST_TTL, "The test's time for control ran out.".into()));
            })));
        }
        self.send_state(request, true, ControlReason::NONE, String::new());
        if !was_active {
            self.ctx.changed();
        }
    }

    fn deactivate(&mut self, reason: ControlReason, message: String, request: u32) {
        let was_active = self.shared.active.swap(false, Ordering::AcqRel);
        if was_active {
            tracing::info!(viewer = %self.viewer_name, ?reason, "stopped controlling this Mac");
        }
        {
            let mut slot = self.ctx.controller.lock().unwrap();
            if slot.as_ref().is_some_and(|s| s.session_id == self.session_id) {
                *slot = None;
            }
        }
        if let Some(input) = &self.input {
            input.release_all();
        }
        self.controlling.store(false, Ordering::Release);
        self.ttl = None;
        if let Some(monitor) = self.cursor.take() {
            // Joining the monitor thread can take a poll interval.
            tokio::task::spawn_blocking(move || drop(monitor));
        }
        self.set_cursor_in_video(true);
        self.send_state(request, false, reason, message);
        if was_active {
            self.ctx.changed();
        }
    }

    fn revoke(&mut self, reason: ControlReason, message: String) {
        if self.active() {
            self.deactivate(reason, message, 0);
        }
    }

    fn check_permission(&mut self) {
        if self.active() && !self.ctx.backend.permitted() {
            tracing::warn!("Accessibility permission was withdrawn; ending control");
            let message = format!("{} turned off LanKVM's Accessibility permission, so it can't be controlled now.", system::device_name());
            self.deactivate(ControlReason::PERMISSION_LOST, message, 0);
        }
    }

    /// The viewer's window gained or lost the focus. While it's elsewhere the viewer shows a
    /// plain arrow, so the remote cursor goes back into the video.
    fn focus(&mut self, forwarding: bool) {
        self.forwarding = forwarding;
        if self.active() {
            let local_cursor = forwarding && self.cursor_shown_locally();
            self.set_cursor_in_video(!local_cursor);
        }
    }

    fn cursor_shown_locally(&self) -> bool {
        self.cursor.is_some() && !self.cursor_unknown
    }

    /// Sends a message the viewer needs to stay in sync. A viewer that doesn't read them (its
    /// queue is full) is broken or hostile: hang up rather than buffer without end.
    fn send(&self, msg: HostMsg) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.out.try_send(msg) {
            tracing::warn!(viewer = %self.viewer_name, "viewer isn't reading its control stream; disconnecting");
            self.conn.close(4u32.into(), b"viewer not reading");
        }
    }

    fn send_state(&self, request: u32, active: bool, reason: ControlReason, message: String) {
        let state = ControlState { request, active, reason, message, injected_tag: self.ctx.injected_tag, host_pid: std::process::id() };
        self.send(HostMsg::Control(state));
    }

    fn attach_input(&mut self, recv: RecvStream) {
        // One stream at a time, and only while controlling (the viewer sends input only then). A
        // newer one replaces a stream the input thread gave up on; otherwise dropping it tells
        // the viewer to stop sending there.
        if !self.active() || self.input.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        if !self.input_starts.allow(Instant::now(), MAX_INPUT_THREADS_PER_SEC) {
            tracing::warn!(viewer = %self.viewer_name, "too many input streams; disconnecting");
            self.conn.close(6u32.into(), b"too many input streams");
            return;
        }
        if self.input.take().is_some() {
            self.ctx.inputs.lock().unwrap().retain(|(id, _)| *id != self.session_id);
        }
        let events = self.events.clone();
        let viewer = self.viewer_name.clone();
        let config = InputConfig {
            display_id: self.display_id,
            backend: self.ctx.backend.clone(),
            tag: self.ctx.injected_tag,
            guard_pid: self.ctx.guard_pid,
            on_failure: Box::new(move |failure| {
                let message = match failure {
                    InputFailure::BadInput(why) => format!("{viewer} sent input LanKVM couldn't accept ({why}), so control was stopped."),
                    InputFailure::Unavailable(why) => injection_failed(&why),
                };
                let _ = events.send(SessionEvt::Revoke(ControlReason::BAD_INPUT, message));
            }),
        };
        match InputThread::spawn(self.ctx.rt.clone(), recv, config, self.shared.clone(), self.out.clone()) {
            Ok(t) => {
                self.ctx.inputs.lock().unwrap().push((self.session_id, t.commands()));
                self.input = Some(t);
            }
            Err(e) => {
                tracing::warn!("start input thread: {e:#}");
                self.revoke(ControlReason::BAD_INPUT, injection_failed(&format!("{e:#}")));
            }
        }
    }

    fn cursor(&mut self, update: CursorUpdate) {
        if !self.active() {
            return;
        }
        self.cursor_unknown = matches!(update, CursorUpdate::Unavailable);
        // While the viewer's window isn't focused it shows a plain arrow: keep the real cursor
        // in the video then.
        let local = self.forwarding;
        match update {
            CursorUpdate::Shape { id, image } => {
                self.send(HostMsg::CursorShape {
                    id,
                    png: image.png.clone(),
                    width: image.width as f32,
                    height: image.height as f32,
                    hot_x: image.hot_x as f32,
                    hot_y: image.hot_y as f32,
                });
            }
            CursorUpdate::Show(id) => {
                self.send(HostMsg::Cursor(CursorState::Shape(id)));
                self.set_cursor_in_video(!local);
            }
            CursorUpdate::Hide => {
                self.send(HostMsg::Cursor(CursorState::Hidden));
                self.set_cursor_in_video(!local);
            }
            CursorUpdate::Unavailable => {
                self.send(HostMsg::Cursor(CursorState::InVideo));
                self.set_cursor_in_video(true);
            }
        }
    }

    /// While the viewer draws the cursor itself, the video leaves it out (or it would show twice,
    /// the video one trailing behind).
    fn set_cursor_in_video(&self, show: bool) {
        let Some(capturer) = self.capturer.clone() else { return };
        if self.cursor_in_video.swap(show, Ordering::AcqRel) == show {
            return;
        }
        let (wanted, apply) = (self.cursor_in_video.clone(), self.cursor_apply.clone());
        // ScreenCaptureKit can take a while to apply it: never wait for that on the runtime.
        tokio::task::spawn_blocking(move || {
            let _one_at_a_time = apply.lock().unwrap();
            let show = wanted.load(Ordering::Acquire);
            let started = std::time::Instant::now();
            match capturer.set_shows_cursor(show) {
                Ok(()) => tracing::debug!(show, ms = started.elapsed().as_secs_f64() * 1000.0, "cursor in video"),
                Err(e) => tracing::warn!("show cursor in video: {e:#}"),
            }
        });
    }
}

fn injection_failed(why: &str) -> String {
    format!("LanKVM on {} couldn't start posting input ({why}), so control was stopped.", system::device_name())
}

fn permission_reason(host: &str) -> String {
    format!("{host} hasn't allowed LanKVM to control it yet. On that Mac, turn on LanKVM in System Settings → Privacy & Security → Accessibility.")
}

impl Drop for SessionControl {
    fn drop(&mut self) {
        self.ctx.inputs.lock().unwrap().retain(|(id, _)| *id != self.session_id);
        {
            let mut slot = self.ctx.controller.lock().unwrap();
            if slot.as_ref().is_some_and(|s| s.session_id == self.session_id) {
                *slot = None;
            }
        }
        self.shared.active.store(false, Ordering::Release);
        self.controlling.store(false, Ordering::Release);
        // Dropping the input thread handle makes it release everything and exit.
        self.input.take();
        if let Some(monitor) = self.cursor.take() {
            tokio::task::spawn_blocking(move || drop(monitor));
        }
    }
}

/// Shows a PIN on this Mac and runs SPAKE2 with the viewer. On success both sides remember
/// each other; the viewer's certificate is added to the trusted list.
async fn pair(
    conn: &Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    ctx: &Arc<HostCtx>,
    client_fp: Fingerprint,
    device_name: &str,
) -> Result<()> {
    let pin = generate_pin();
    let _prompt = PromptGuard::new(
        PairPrompt { id: conn.stable_id() as u64, name: device_name.to_string(), addr: conn.remote_address(), pin: pin.clone(), conn: conn.clone() },
        ctx.clone(),
    );
    tracing::info!(viewer = %device_name, addr = %conn.remote_address(), "pairing requested");
    write_msg(send, &HostMsg::PairingRequired).await?;

    let start = tokio::time::timeout(PAIRING_TIMEOUT, read_msg::<ClientMsg>(recv))
        .await
        .context("pairing timed out")??;
    let Some(ClientMsg::PairStart { spake }) = start else { bail!("viewer cancelled pairing") };
    // The reply lets the viewer check its guess (and hang up if wrong), so this is the attempt.
    ctx.pairing_throttle.lock().unwrap().attempt(Instant::now());
    let (pairing, spake, mac) = host_respond(&pin, &client_fp, &ctx.trust.fingerprint, &spake)?;
    write_msg(send, &HostMsg::PairReply { spake, mac }).await?;

    let confirm = tokio::time::timeout(Duration::from_secs(10), read_msg::<ClientMsg>(recv))
        .await
        .context("pairing timed out")??;
    match confirm {
        Some(ClientMsg::PairConfirm { mac }) if pairing.verify(&mac) => {}
        // The viewer detects a wrong PIN first and hangs up.
        _ => bail!("pairing failed (wrong code)"),
    }
    ctx.trust.viewers.lock().unwrap().add(client_fp, device_name)?;
    ctx.pairing_throttle.lock().unwrap().succeeded();
    tracing::info!(viewer = %device_name, "paired");
    (ctx.events)(Event::TrustChanged);
    Ok(())
}

/// Tells the viewer why it can't connect, then waits briefly for it to hang up so the message
/// isn't lost when the connection is torn down.
async fn reject(conn: &Connection, send: &mut SendStream, reason: &str) -> Result<()> {
    write_msg(send, &HostMsg::Rejected { reason: reason.to_string() }).await?;
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    bail!("rejected: {reason}")
}

/// Shown on the viewer, so "this Mac" is the viewer. Names the Mac that needs the update.
fn version_mismatch(viewer_version: u32, host_name: &str) -> String {
    let outdated = if viewer_version < PROTOCOL_VERSION { "this Mac" } else { host_name };
    format!(
        "LanKVM on {host_name} doesn't match this Mac (protocol {PROTOCOL_VERSION} there, {viewer_version} here). \
         Update LanKVM on {outdated}, then quit and reopen it."
    )
}

/// Largest size with the display's aspect ratio that fits the client's limit (even dimensions,
/// as the encoder requires).
fn fit_within(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let scale = (max_w as f64 / width as f64).min(max_h as f64 / height as f64).min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(width as f64 * scale), even(height as f64 * scale))
}

/// Up to the viewer's refresh rate, but 120 fps only for streams the encoder can keep up with:
/// encoding takes about 1.1 ms per megapixel on Apple silicon, so above ~5.6 Mpx a frame takes
/// longer than 1/120 s and the extra frames only cost power (measured: no latency gain at
/// 4112×2658; 4 ms less click-to-photon at 2560×1654).
fn choose_fps(requested: u32, width: u32, height: u32) -> u32 {
    let max = if u64::from(width) * u64::from(height) <= 5_600_000 { 120 } else { 60 };
    requested.clamp(15, max)
}

/// Generous LAN bitrate: about 0.12 bits per pixel per frame, so text stays sharp.
fn bitrate_for(width: u32, height: u32, fps: u32) -> u32 {
    let bps = width as f64 * height as f64 * fps as f64 * 0.12;
    bps.clamp(8e6, 150e6) as u32
}

/// Captures the display and streams encoded frames to one viewer.
struct StreamSession {
    capturer: Option<Arc<Capturer>>,
    shared: Arc<Shared>,
    encode_thread: Option<JoinHandle<()>>,
}

/// Capture runs ahead of the encoder, which takes one frame at a time and always the newest. A
/// frame that would have to queue behind another is replaced by a newer one instead, so a slow
/// encode costs frame rate, never latency.
struct Shared {
    encoder: Encoder,
    state: Mutex<EncodeState>,
    wake: Condvar,
    last_forced_us: AtomicU64,
}

#[derive(Default)]
struct EncodeState {
    /// Captured and waiting for the encoder; a newer capture replaces it.
    next: Option<CapturedFrame>,
    force_keyframe: bool,
    stop: bool,
}

impl StreamSession {
    fn start(conn: Connection, capture: &CaptureConfig, encoder: &EncoderConfig) -> Result<Self> {
        let packetizer = Mutex::new(Packetizer::new());
        let (width, height) = (encoder.width, encoder.height);
        let encoder = Encoder::new(encoder, move |frame| send_frame(&conn, &packetizer, frame, width, height))?;
        let shared = Arc::new(Shared {
            encoder,
            state: Mutex::new(EncodeState::default()),
            wake: Condvar::new(),
            last_forced_us: AtomicU64::new(0),
        });
        let encode_thread = std::thread::Builder::new().name("lankvm-encode".into()).spawn({
            let shared = shared.clone();
            move || shared.encode_loop()
        })?;
        let mut session = Self { capturer: None, shared, encode_thread: Some(encode_thread) };
        let on_frame = session.shared.clone();
        session.capturer = Some(Arc::new(Capturer::start(capture, move |frame| on_frame.on_captured(frame))?));
        Ok(session)
    }

    fn codec(&self) -> Codec {
        self.shared.encoder.codec()
    }

    fn capturer(&self) -> Option<Arc<Capturer>> {
        self.capturer.clone()
    }

    fn request_keyframe(&self) {
        self.shared.request_keyframe();
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        // Stop capture first so no new frames reach the encoder while it shuts down.
        self.capturer.take();
        self.shared.state.lock().unwrap().stop = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.encode_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Shared {
    fn on_captured(&self, frame: CapturedFrame) {
        self.state.lock().unwrap().next = Some(frame);
        self.wake.notify_one();
    }

    fn request_keyframe(&self) {
        let now = clock::now_us();
        let prev = self.last_forced_us.load(Ordering::Acquire);
        if now.saturating_sub(prev) < KEYFRAME_MIN_INTERVAL.as_micros() as u64 {
            return;
        }
        self.last_forced_us.store(now, Ordering::Release);
        self.state.lock().unwrap().force_keyframe = true;
        self.wake.notify_one();
    }

    fn encode_loop(&self) {
        // The newest frame handed to the encoder, kept so a keyframe can be produced right away
        // even when the screen is static (ScreenCaptureKit only delivers frames on change).
        let mut last: Option<CapturedFrame> = None;
        loop {
            let (next, force) = {
                let mut state = self.state.lock().unwrap();
                while !state.stop && state.next.is_none() && !(state.force_keyframe && last.is_some()) {
                    state = self.wake.wait(state).unwrap();
                }
                if state.stop {
                    return;
                }
                (state.next.take(), std::mem::take(&mut state.force_keyframe))
            };
            let time_us = match next {
                Some(frame) => {
                    let time_us = frame.capture_time_us;
                    last = Some(frame);
                    time_us
                }
                // Nothing new on screen: re-encode what's showing now as a keyframe.
                None => clock::now_us(),
            };
            let Some(frame) = &last else { continue };
            if let Err(e) = self.encoder.encode(&frame.pixel_buffer, time_us, force) {
                tracing::warn!("encode: {e:#}");
                continue;
            }
            if !self.encoder.wait_idle(ENCODE_TIMEOUT) {
                tracing::warn!("encoder took over {ENCODE_TIMEOUT:?} for a frame");
            }
        }
    }
}

fn send_frame(conn: &Connection, packetizer: &Mutex<Packetizer>, frame: EncodedFrame, width: u32, height: u32) {
    let video = VideoFrame {
        codec: frame.codec,
        keyframe: frame.keyframe,
        width,
        height,
        capture_time_us: frame.capture_time_us,
        encode_start_us: frame.encode_start_us,
        encoded_time_us: clock::now_us(),
        param_sets: frame.param_sets,
        nal_length_size: frame.nal_length_size,
        data: frame.data,
    };
    let Some(max) = conn.max_datagram_size() else { return };
    let packets = protocol::encode(&video)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| packetizer.lock().unwrap().packetize(&bytes, max));
    match packets {
        Ok(packets) => {
            for packet in packets {
                if conn.send_datagram(packet).is_err() {
                    break; // connection closing
                }
            }
        }
        Err(e) => tracing::warn!("packetize: {e:#}"),
    }
}

/// Drops the stream on a blocking thread: stopping ScreenCaptureKit waits for a completion
/// handler, which must not stall the async runtime.
struct SessionGuard(Option<StreamSession>);

impl std::ops::Deref for SessionGuard {
    type Target = StreamSession;
    fn deref(&self) -> &StreamSession {
        self.0.as_ref().expect("present until drop")
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(stream) = self.0.take() {
            tokio::task::spawn_blocking(move || drop(stream));
        }
    }
}

/// Lists the viewer in the UI for as long as the session lives.
struct Registration {
    id: u64,
    ctx: Arc<HostCtx>,
}

impl Registration {
    fn new(viewer: Viewer, ctx: Arc<HostCtx>) -> Self {
        let id = viewer.id;
        ctx.status.lock().unwrap().viewers.push(viewer);
        ctx.changed();
        Self { id, ctx }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.ctx.status.lock().unwrap().viewers.retain(|v| v.id != self.id);
        self.ctx.changed();
    }
}

/// Shows the pairing PIN in the UI for as long as the pairing attempt lasts.
struct PromptGuard {
    id: u64,
    ctx: Arc<HostCtx>,
}

impl PromptGuard {
    fn new(prompt: PairPrompt, ctx: Arc<HostCtx>) -> Self {
        let id = prompt.id;
        ctx.status.lock().unwrap().pairing.push(prompt);
        ctx.changed();
        Self { id, ctx }
    }
}

impl Drop for PromptGuard {
    fn drop(&mut self) {
        self.ctx.status.lock().unwrap().pairing.retain(|p| p.id != self.id);
        self.ctx.changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_and_even() {
        assert_eq!(fit_within(3456, 2234, 10_000, 10_000), (3456, 2234));
        assert_eq!(fit_within(5120, 2880, 2560, 1600), (2560, 1440));
        let (w, h) = fit_within(3024, 1964, 1920, 1080);
        assert!(w <= 1920 && h <= 1080 && w % 2 == 0 && h % 2 == 0);
    }

    #[test]
    fn version_mismatch_names_the_outdated_mac() {
        assert!(version_mismatch(PROTOCOL_VERSION - 1, "Studio").contains("Update LanKVM on this Mac"));
        assert!(version_mismatch(PROTOCOL_VERSION + 1, "Studio").contains("Update LanKVM on Studio"));
    }

    #[test]
    fn fps_follows_the_viewer_within_what_encodes_in_time() {
        assert_eq!(choose_fps(120, 2560, 1654), 120);
        assert_eq!(choose_fps(120, 4112, 2658), 60);
        assert_eq!(choose_fps(60, 2560, 1654), 60);
        assert_eq!(choose_fps(1000, 1920, 1080), 120);
        assert_eq!(choose_fps(0, 1920, 1080), 15);
    }

    #[test]
    fn pairing_pauses_after_a_few_attempts_for_longer_each_time() {
        let mut t = PairingThrottle::default();
        let t0 = Instant::now();
        for _ in 0..FREE_PAIRING_ATTEMPTS - 1 {
            t.begin(t0, "Studio").unwrap();
            t.attempt(t0);
            t.end();
        }
        t.begin(t0, "Studio").unwrap();
        // Only one pairing at a time.
        assert!(t.begin(t0, "Studio").unwrap_err().contains("Another Mac"));
        t.attempt(t0);
        t.end();
        let err = t.begin(t0, "Studio").unwrap_err();
        assert!(err.contains("paused pairing for 30 s"), "{err}");
        let t1 = t0 + PAIRING_PAUSE;
        t.begin(t1, "Studio").unwrap();
        t.attempt(t1);
        t.end();
        assert!(t.begin(t1 + PAIRING_PAUSE, "Studio").is_err());
        assert!(t.begin(t1 + PAIRING_PAUSE * 2, "Studio").is_ok());
        t.end();
        // Far along, the pause stays at its maximum.
        for _ in 0..20 {
            t.attempt(t1);
        }
        assert!(t.begin(t1 + MAX_PAIRING_PAUSE - Duration::from_secs(1), "Studio").is_err());
        assert!(t.begin(t1 + MAX_PAIRING_PAUSE, "Studio").is_ok());
        t.end();
        t.succeeded();
        assert!(t.begin(t1, "Studio").is_ok());
    }

    #[test]
    fn rate_limit_counts_per_second() {
        let mut r = RateLimit::default();
        let t0 = Instant::now();
        assert!((0..50).all(|_| r.allow(t0, 50)));
        assert!(!r.allow(t0 + Duration::from_millis(999), 50));
        assert!(r.allow(t0 + Duration::from_secs(1), 50));
    }

    #[test]
    fn bitrate_is_clamped() {
        assert_eq!(bitrate_for(640, 480, 30), 8_000_000);
        assert_eq!(bitrate_for(7680, 4320, 120), 150_000_000);
    }
}
