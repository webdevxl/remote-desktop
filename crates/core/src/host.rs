//! Host role: accept viewers from the local network and stream this Mac's screen to them.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use platform_mac::capture::{CaptureConfig, CapturedFrame, Capturer, main_display};
use platform_mac::encoder::{EncodedFrame, Encoder, EncoderConfig};
use platform_mac::{clock, permissions, system};
use protocol::{ClientMsg, Codec, HostMsg, PROTOCOL_VERSION, VideoFrame};
use quinn::{Connection, Endpoint, Incoming, RecvStream, SendStream};
use transport::endpoint::peer_fingerprint;
use transport::framing::{read_msg, write_msg};
use transport::identity::{Fingerprint, short_hex};
use transport::net::is_local_network;
use transport::pairing::{generate_pin, host_respond};
use serde::Serialize;
use transport::video::Packetizer;

use crate::{Event, EventSink, Trust};

/// Minimum spacing between keyframes produced on request; a client that keeps losing packets
/// shouldn't turn the stream into all-keyframes.
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// How long the PIN stays valid while someone walks over to the other Mac.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);
/// A frame should leave the encoder within milliseconds; past this, move on to the next one.
const ENCODE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub struct Viewer {
    pub id: u64,
    pub name: String,
    pub addr: SocketAddr,
    pub fingerprint: Fingerprint,
    pub conn: Connection,
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
}

/// Host status as the UI sees it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatusView {
    pub viewers: Vec<ViewerView>,
    pub pairing: Vec<PairPromptView>,
    pub screen_capture_allowed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerView {
    pub id: u64,
    pub name: String,
    pub address: String,
    pub device_id: String,
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
                })
                .collect(),
            pairing: status
                .pairing
                .iter()
                .map(|p| PairPromptView { id: p.id, name: p.name.clone(), address: p.addr.ip().to_string(), pin: p.pin.clone() })
                .collect(),
            screen_capture_allowed: permissions::screen_capture_allowed(),
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
    let (mut send, mut recv) = conn.accept_bi().await.context("open control stream")?;
    let Some(ClientMsg::Hello { version, device_name, max_width, max_height, fps, trusts_host }) = read_msg(&mut recv).await? else {
        bail!("expected Hello");
    };
    if version != PROTOCOL_VERSION {
        return reject(&conn, &mut send, &version_mismatch(version, &system::device_name())).await;
    }
    if !tokio::task::spawn_blocking(can_capture).await? {
        return reject(&conn, &mut send, "That Mac hasn't allowed Screen Recording for LanKVM yet (System Settings → Privacy & Security), or LanKVM needs a restart there after allowing it.").await;
    }
    let known = ctx.trust.viewers.lock().unwrap().contains(&client_fp);
    if !known || !trusts_host {
        pair(&conn, &mut send, &mut recv, &ctx, client_fp, &device_name).await?;
    }

    let display = tokio::task::spawn_blocking(main_display).await??;
    let (width, height) = fit_within(display.width, display.height, max_width, max_height);
    let fps = fps.clamp(15, 120);
    let bitrate_bps = bitrate_for(width, height, fps);
    let capture = CaptureConfig { display_id: display.id, width, height, fps, show_cursor: true };
    let encoder = EncoderConfig { width, height, fps, bitrate_bps, codec: Codec::Hevc };
    let stream_conn = conn.clone();
    let stream = tokio::task::spawn_blocking(move || StreamSession::start(stream_conn, &capture, &encoder))
        .await?
        .context("start screen stream")?;
    let stream = SessionGuard(Some(stream));

    write_msg(
        &mut send,
        &HostMsg::Welcome { device_name: system::device_name(), width, height, fps, codec: stream.codec() },
    )
    .await?;

    tracing::info!(viewer = %device_name, addr = %conn.remote_address(), fingerprint = %short_hex(&client_fp), width, height, fps, bitrate_bps, "streaming");
    let _registration = Registration::new(
        Viewer { id: conn.stable_id() as u64, name: device_name, addr: conn.remote_address(), fingerprint: client_fp, conn: conn.clone() },
        ctx.clone(),
    );

    while let Some(msg) = read_msg::<ClientMsg>(&mut recv).await? {
        match msg {
            ClientMsg::RequestKeyframe => stream.request_keyframe(),
            ClientMsg::Ping { client_time_us } => {
                write_msg(&mut send, &HostMsg::Pong { client_time_us, host_time_us: clock::now_us() }).await?;
            }
            other => bail!("unexpected message {other:?}"),
        }
    }
    Ok(())
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

/// Generous LAN bitrate: about 0.12 bits per pixel per frame, so text stays sharp.
fn bitrate_for(width: u32, height: u32, fps: u32) -> u32 {
    let bps = width as f64 * height as f64 * fps as f64 * 0.12;
    bps.clamp(8e6, 150e6) as u32
}

/// Captures the display and streams encoded frames to one viewer.
struct StreamSession {
    capturer: Option<Capturer>,
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
        session.capturer = Some(Capturer::start(capture, move |frame| on_frame.on_captured(frame))?);
        Ok(session)
    }

    fn codec(&self) -> Codec {
        self.shared.encoder.codec()
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
    fn bitrate_is_clamped() {
        assert_eq!(bitrate_for(640, 480, 30), 8_000_000);
        assert_eq!(bitrate_for(7680, 4320, 120), 150_000_000);
    }
}
