//! Host role: accept viewers (from the local network, and from the internet when that is on)
//! and stream this Mac's screen to them.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use platform_mac::capture::{CaptureConfig, CapturedFrame, Capturer, DisplayInfo, main_display};
use platform_mac::cursor::{CursorMonitor, CursorUpdate};
use platform_mac::encoder::{EncodedFrame, Encoder, EncoderConfig};
use platform_mac::tiler::{TileCopy, Tiler, tiles_touched};
use platform_mac::{CVPixelBuffer, clock, permissions, system, virtual_display};
use protocol::{
    Arrangement, ClientMsg, Codec, ControlReason, ControlState, CursorState, DisplayChoice, DisplayReason, DisplayState, HostMsg,
    FULL_FRAME_TILE, MAX_TILES, PROTOCOL_VERSION, TILE_MAX_WIDTH, TileRect, VideoFrame, VirtualDisplaySpec, tile_layout,
};
use quinn::{Connection, ConnectionError, Incoming, RecvStream, SendStream};
use tokio::sync::mpsc;
use transport::cc::Pace;
use transport::endpoint::{Network, peer_fingerprint};
use transport::framing::{read_msg, write_msg};
use transport::identity::{Fingerprint, short_hex};
use transport::pairing::{generate_pin, host_respond};
use serde::Serialize;
use transport::video::Packetizer;

use crate::control::{Backend, HostSettings, InputCmdSender, InputConfig, InputFailure, InputShared, InputThread, RateLimit};
use crate::displays::{Acquired, DisplayNotice, Displays, VirtualDisplayView};
use crate::internet::{HostInternet, InternetView};
use crate::rate::{self, RateControl, Sample};
use crate::{Event, EventSink, Trust};

/// Minimum spacing between keyframes (and re-sent tiles) produced on request or after a loss; a
/// client that keeps losing packets shouldn't turn the stream into all-keyframes. Short, as a
/// keyframe is only of the tiles that need one.
const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(50);
/// Each tile's encoder gets at least this much of the stream's bitrate.
const MIN_TILE_BITRATE: u32 = 1_000_000;
/// A tile counts as moving, for its bitrate, this long after it last changed (see [`Motion`]).
const MOTION_WINDOW: Duration = Duration::from_secs(1);
/// How often the tiles' bitrates follow the motion.
const RETARGET_INTERVAL: Duration = Duration::from_millis(250);
/// A tile's encoder is told a new bitrate only when it is off by more than this fraction.
const RETARGET_MIN_CHANGE: f64 = 0.25;
/// How long the PIN stays valid while someone walks over to the other Mac.
const PAIRING_TIMEOUT: Duration = Duration::from_secs(120);
/// A frame should leave the encoders within milliseconds; past this, move on to the next one.
const ENCODE_TIMEOUT: Duration = Duration::from_millis(500);
/// The screen counts as still this long after an update: the viewer is then told which update
/// was the last ([`HostMsg::VideoIdle`]), in case it lost all of it. Longer than
/// [`KEYFRAME_MIN_INTERVAL`], so a tile resent after its encoder dropped it goes out first (the
/// viewer would otherwise ask for a keyframe it doesn't need).
const IDLE_NOTE_AFTER: Duration = Duration::from_millis(60);
/// Even with ScreenCaptureKit's dirty rectangles to go by, every tile is compared this often, in
/// case they missed a change.
const FULL_COMPARE_INTERVAL: Duration = Duration::from_millis(250);
/// After the dirty rectangles missed a change, they aren't used for this long.
const DIRTY_RECTS_DISTRUST: Duration = Duration::from_secs(10);
/// QUIC handshakes in progress at once, in all and from one address. Honest ones take a round
/// trip on a LAN; a peer that starts handshakes and never finishes them can't pile them up.
const MAX_HANDSHAKES: usize = 64;
const MAX_HANDSHAKES_PER_ADDRESS: usize = 8;
/// Handshakes from the internet count apart, and fewer may run: whoever has a knock (a paired
/// viewer, or someone with its key) can't take the places viewers on the local network need.
const MAX_HANDSHAKES_INTERNET: usize = 16;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// The same over the internet: a few round trips of a slow path, with a lost packet or two.
const HANDSHAKE_TIMEOUT_INTERNET: Duration = Duration::from_secs(8);
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
/// Display requests a viewer may send per second. Each can rearrange the Mac's displays; people
/// pick one from a menu now and then. (Changes are spaced out anyway, see `displays`.)
const MAX_DISPLAY_REQUESTS_PER_SEC: u32 = 10;
/// The longest a session waits before restarting a capture that stopped by itself; it waits
/// longer each time in a row, starting from a quarter second.
const MAX_CAPTURE_RESTART_DELAY: Duration = Duration::from_secs(8);

#[derive(Clone)]
pub struct Viewer {
    pub id: u64,
    pub name: String,
    pub fingerprint: Fingerprint,
    pub conn: Connection,
    /// It connected over the internet, not from the local network.
    pub internet: bool,
    /// Whether this viewer controls the Mac right now (rather than only viewing it).
    pub controlling: Arc<AtomicBool>,
    /// The display it watches.
    pub watching: Arc<AtomicU32>,
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
    /// Virtual displays made for viewers.
    pub(crate) displays: Displays,
    /// This user's session has the screen (not switched out by fast user switching).
    pub(crate) console_active: AtomicBool,
    pub status: Mutex<HostStatus>,
    pub events: EventSink,
    pub trust: Arc<Trust>,
    pub settings: Mutex<HostSettings>,
    pub settings_path: PathBuf,
    /// Internet access: the gate's keys and the port mapping.
    pub(crate) internet: Arc<HostInternet>,
    /// Where injected input goes (real events unless a test asks to record them).
    pub backend: Backend,
    /// Stamped on every injected event, with bits 24..31 holding the relay depth; viewers on
    /// this Mac drop events matching it with those bits masked.
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
    /// Handshakes in progress by whether they came from the internet, and address (see
    /// [`MAX_HANDSHAKES`]).
    pub(crate) handshakes: Mutex<HashMap<(bool, IpAddr), usize>>,
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
    /// Displays made for viewers, watched or waiting for their viewer to come back.
    pub virtual_displays: Vec<VirtualDisplayView>,
    /// Whether paired Macs can reach this one over the internet.
    pub internet: InternetView,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerView {
    pub id: u64,
    pub name: String,
    pub address: String,
    pub device_id: String,
    pub controlling: bool,
    /// The display it watches, and whether that is one made for a viewer.
    pub display_id: u32,
    pub virtual_display: bool,
    /// It connected over the internet.
    pub internet: bool,
    /// Its traffic goes through a LanKVM server's relay (`address` then stands for the relay
    /// session, in 240.0.0.0/4).
    pub relayed: bool,
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
        let virtual_displays = self.displays.summary();
        let internet = self.internet.view();
        let status = self.status.lock().unwrap();
        HostStatusView {
            viewers: status
                .viewers
                .iter()
                .map(|v| {
                    let display_id = v.watching.load(Ordering::Acquire);
                    // Where it is now: QUIC follows a viewer that moves (off a relay, say).
                    let addr = v.conn.remote_address();
                    ViewerView {
                        id: v.id,
                        name: v.name.clone(),
                        address: addr.ip().to_string(),
                        device_id: short_hex(&v.fingerprint),
                        controlling: v.controlling.load(Ordering::Acquire),
                        display_id,
                        virtual_display: virtual_displays.iter().any(|d| d.display_id == display_id),
                        internet: v.internet,
                        relayed: self.internet.is_relayed(addr),
                    }
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
            virtual_displays,
            internet,
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
            // Virtual displays come with control: they move this Mac's windows around.
            self.displays.remove(None, format!("Remote control was turned off on {}, so its virtual display was removed.", system::device_name()));
        }
        for v in self.status.lock().unwrap().viewers.iter() {
            let _ = v.session.send(SessionEvt::Availability);
        }
        self.changed();
    }

    /// Lets paired Macs connect over the internet, or stops that. Turning it off ends the
    /// sessions that came over the internet; turning it on asks the router to forward the port.
    pub fn set_internet_access(&self, on: bool) {
        {
            let mut settings = self.settings.lock().unwrap();
            settings.internet_access = on;
            if let Err(e) = settings.save(&self.settings_path) {
                tracing::warn!("save host settings: {e:#}");
            }
        }
        // First, so no new session gets in once the old ones are closed below.
        self.internet.set_enabled(on);
        tracing::info!(on, "internet access");
        if !on {
            for v in self.status.lock().unwrap().viewers.iter().filter(|v| v.internet) {
                v.conn.close(8u32.into(), b"internet access turned off");
            }
        }
        self.changed();
    }

    /// The address paired Macs are told to use over the internet, as the user typed it (a
    /// dynamic DNS name or an IP, with or without a port), or "".
    pub fn set_public_address(&self, address: &str) {
        let address = address.trim().to_string();
        {
            let mut settings = self.settings.lock().unwrap();
            settings.public_address = address.clone();
            if let Err(e) = settings.save(&self.settings_path) {
                tracing::warn!("save host settings: {e:#}");
            }
        }
        self.internet.set_public_address(address);
        self.changed();
    }

    /// The LanKVM server ("host:port") this Mac registers with while internet access is on, as
    /// the user typed it, or "" for none.
    pub fn set_rendezvous_server(&self, address: &str) {
        let address = address.trim().to_string();
        {
            let mut settings = self.settings.lock().unwrap();
            settings.rendezvous_server = address.clone();
            if let Err(e) = settings.save(&self.settings_path) {
                tracing::warn!("save host settings: {e:#}");
            }
        }
        tracing::info!(server = address, "LanKVM server");
        // Sessions through the old server's relay end: this Mac stops listening to it in a
        // moment (see `Rendezvous::set_host_addr`).
        if self.internet.set_rendezvous_server(&address) {
            for v in self.status.lock().unwrap().viewers.iter().filter(|v| self.internet.is_relayed(v.conn.remote_address())) {
                v.conn.close(8u32.into(), b"LanKVM server changed");
            }
        }
        self.changed();
    }

    /// Whether this user's session has the screen. While another user has it, the displays made
    /// for viewers (holding this user's windows) go, and viewers can't add one.
    pub fn set_console_active(&self, active: bool) {
        if self.console_active.swap(active, Ordering::AcqRel) == active {
            return;
        }
        if !active {
            let host = system::device_name();
            self.displays.remove(None, format!("{host} switched to another user, so its virtual display was removed."));
        }
        for v in self.status.lock().unwrap().viewers.iter() {
            let _ = v.session.send(SessionEvt::Availability);
        }
    }

    /// The virtual displays changed: the UI shows them.
    pub(crate) fn displays_changed(&self) {
        self.changed();
    }

    /// Removes a virtual display made for a viewer (`None`: all of them); its viewers go back to
    /// this Mac's own screen. Never waits.
    pub fn remove_virtual_display(&self, display_id: Option<u32>) {
        let host = system::device_name();
        self.displays.remove(display_id, format!("{host} went back to its own screen: its user removed the virtual display."));
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

pub async fn run(network: Network, ctx: Arc<HostCtx>) {
    while let Some(incoming) = network.endpoint.accept().await {
        let remote = incoming.remote_address();
        // From the internet, only a paired viewer that knocks gets any answer, and only while
        // internet access is on. Everyone else is ignored, never refused: a refusal would show
        // the port is open. (The gate in front of the socket already dropped most of them.)
        let internet = if network.gate.is_internet(remote) {
            let Some(viewer) = network.gate.admit(&incoming.orig_dst_cid(), remote) else {
                tracing::debug!(%remote, "ignored a connection attempt from the internet");
                incoming.ignore();
                continue;
            };
            let pace = Pace::new();
            Some(Internet { viewer, config: network.internet_server_config(&pace), pace })
        } else {
            None
        };
        // The peer proves it receives at its address (a stateless retry: one more round trip)
        // before the host keeps any state for it, so floods from made-up addresses cost nothing.
        if !incoming.remote_address_validated() {
            if let Err(e) = incoming.retry() {
                e.into_incoming().ignore();
            }
            continue;
        }
        let Some(handshake) = Handshake::enter(&ctx, remote.ip(), internet.is_some()) else {
            if internet.is_some() {
                tracing::debug!(%remote, "ignored a connection from the internet: too many handshakes in progress");
                incoming.ignore();
            } else {
                tracing::warn!(%remote, "refused connection: too many handshakes in progress");
                incoming.refuse();
            }
            continue;
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            match serve(incoming, ctx, handshake, internet).await {
                Ok(()) => tracing::info!(%remote, "viewer disconnected"),
                Err(e) => tracing::info!(%remote, "viewer session ended: {e:#}"),
            }
        });
    }
}

/// A connection attempt from the internet that carried a valid knock.
struct Internet {
    /// The viewer whose key made the knock.
    viewer: Fingerprint,
    config: Arc<quinn::ServerConfig>,
    /// How fast the connection sends, set from the video's bitrate ceiling.
    pace: Pace,
}

async fn serve(incoming: Incoming, ctx: Arc<HostCtx>, handshake: Handshake, internet: Option<Internet>) -> Result<()> {
    let conn = match &internet {
        None => tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await.context("handshake timed out")?.context("handshake")?,
        Some(internet) => {
            let connecting = incoming.accept_with(internet.config.clone()).context("handshake")?;
            tokio::time::timeout(HANDSHAKE_TIMEOUT_INTERNET, connecting).await.context("handshake timed out")?.context("handshake")?
        }
    };
    drop(handshake);
    let client_fp = peer_fingerprint(&conn).context("viewer sent no certificate")?;
    // Over the internet, the device must be the paired viewer whose key knocked. Anyone else is
    // told nothing.
    if let Some(internet) = &internet
        && (client_fp != internet.viewer || !ctx.trust.viewers.lock().unwrap().contains(&client_fp))
    {
        conn.close(0u32.into(), b"");
        bail!("turned away a device from the internet: not the paired viewer that knocked");
    }
    let pace = internet.map(|internet| internet.pace);
    let internet = pace.is_some();
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
        // A PIN shown here could be guessed at from anywhere: pairing needs the local network.
        if internet {
            let host = system::device_name();
            return reject(&conn, &mut send, &format!(
                "{host} only pairs with Macs on its own network. Connect to it on the same network once (enter its code there), then connect over the internet."
            ))
            .await;
        }
        let _slot = match PairingSlot::claim(&ctx) {
            Ok(slot) => slot,
            Err(why) => return reject(&conn, &mut send, &why).await,
        };
        pair(&conn, &mut send, &mut recv, &ctx, client_fp, &device_name).await?;
    }

    let session_id = conn.stable_id() as u64;
    let same_mac = crate::is_this_mac(conn.remote_address().ip());
    // Everything else about the session (this Mac's user, the input thread, the cursor monitor,
    // displays) comes here and goes first, so stopping control never waits behind the viewer's
    // traffic.
    let (evt_tx, mut events) = mpsc::unbounded_channel::<SessionEvt>();
    // The session counts among the virtual displays' viewers from here on, until this drops (last).
    let _lease = Lease { ctx: ctx.clone(), session_id, conn: conn.clone() };
    // A device that comes back finds its virtual display (kept a while after a lost connection).
    let adopted = ctx.displays.join(session_id, client_fp, &device_name, evt_tx.clone()).await;
    let adopted_seq = adopted.map_or(0, |a| a.seq);
    let streamer = Streamer {
        ctx: ctx.clone(),
        conn: conn.clone(),
        session_id,
        viewer_max: (max_width, max_height),
        viewer_fps: fps,
        video: ctx.video,
        internet,
        stream: Arc::default(),
        video_out: Arc::new(match pace {
            Some(pace) => VideoOut::over_the_internet(conn.clone(), pace),
            None => VideoOut::new(conn.clone(), false),
        }),
        input: Arc::new(InputShared::default()),
        cursor: CursorWish::default(),
        generation: Arc::default(),
        events: evt_tx.clone(),
    };
    let showing = match adopted {
        Some(acquired) => match streamer.show_virtual(acquired).await {
            Ok(showing) => showing,
            Err(e) => {
                tracing::warn!("show the virtual display: {e:#}");
                ctx.displays.release(session_id).await;
                streamer.show_main().await.context("start screen stream")?
            }
        },
        None => streamer.show_main().await.context("start screen stream")?,
    };
    let codec = streamer.stream.codec().unwrap_or(Codec::Hevc);
    let Showing { width, height, fps, .. } = showing;

    write_msg(&mut send, &HostMsg::Welcome { device_name: system::device_name(), width, height, fps, codec }).await?;

    tracing::info!(viewer = %device_name, addr = %conn.remote_address(), fingerprint = %short_hex(&client_fp), width, height, fps, display = showing.display_id, video = ctx.video, internet, "streaming");

    // From here on several parties talk to the viewer (pongs, control state, cursor shapes,
    // input acks), so one task owns the send side of the control stream.
    let (out, mut out_rx) = mpsc::channel::<HostMsg>(OUT_QUEUE);
    streamer.video_out.set_notices(out.clone());
    let _writer = AbortOnDrop(tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
    }));
    // Over the internet, the video follows what the connection carries.
    let _rate = internet.then(|| AbortOnDrop(tokio::spawn(follow_the_connection(conn.clone(), streamer.video_out.clone(), ctx.internet.clone()))));
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
    let watching = Arc::new(AtomicU32::new(showing.display_id));
    let _registration = Registration::new(
        Viewer {
            id: session_id,
            name: device_name.clone(),
            fingerprint: client_fp,
            conn: conn.clone(),
            internet,
            controlling: controlling.clone(),
            watching: watching.clone(),
            session: evt_tx.clone(),
        },
        ctx.clone(),
    );
    // Forget Device may have removed this device while its stream started. It closes only the
    // viewers listed, and this one is listed from here on.
    if !ctx.trust.viewers.lock().unwrap().contains(&client_fp) {
        bail!("device was forgotten while connecting");
    }
    // Likewise turning internet access off.
    if internet && !ctx.internet.enabled() {
        conn.close(8u32.into(), b"internet access turned off");
        bail!("internet access was turned off while connecting");
    }
    drop(pending);
    let mut control = SessionControl {
        ctx: ctx.clone(),
        conn: conn.clone(),
        session_id,
        viewer_name: device_name,
        client_fp,
        same_mac,
        shared: streamer.input.clone(),
        input: None,
        input_starts: RateLimit::default(),
        controlling,
        cursor: None,
        cursor_unknown: true,
        cursor_wish: streamer.cursor.clone(),
        forwarding: false,
        stream: streamer.stream.clone(),
        out: out.clone(),
        events: evt_tx,
        ttl: None,
    };
    let mut screen = Screen::new(streamer, showing, out.clone(), client_fp, same_mac, watching);
    screen.seq_seen = adopted_seq;
    // What it shows, and whether it may ask for a virtual display.
    screen.announce(0, DisplayReason::NONE, String::new());
    // How to reach this Mac over the internet, now or once that is turned on (after Display:
    // the viewer reads that first).
    let (rendezvous_server, rendezvous_id) = ctx.internet.rendezvous_announced();
    let _ = out.try_send(HostMsg::InternetAccess {
        key: ctx.internet.access_key(&client_fp).to_vec(),
        addresses: ctx.internet.announced_addresses(),
        rendezvous_server,
        rendezvous_id,
    });

    // While controlled, notice within a second if the Accessibility permission is withdrawn.
    let mut permission_check = tokio::time::interval(PERMISSION_CHECK);
    // Where the viewer is, as This Mac lists it: it can move (off a relay, say).
    let mut listed_at = conn.remote_address();
    let mut requests = RateLimit::default();
    let mut display_requests = RateLimit::default();
    loop {
        let evt = tokio::select! {
            biased;
            evt = events.recv() => evt,
            evt = client_rx.recv() => evt,
            _ = permission_check.tick() => {
                control.check_permission();
                // QUIC follows a viewer to a new network. A session from the local network
                // doesn't follow it onto the internet: that takes a knock (and internet access
                // on), and its settings are the local network's.
                if !internet && ctx.internet.is_internet(conn.remote_address()) {
                    conn.close(9u32.into(), b"left the local network");
                    bail!("the viewer left the local network (now at {})", conn.remote_address());
                }
                if conn.remote_address() != listed_at {
                    listed_at = conn.remote_address();
                    tracing::info!(addr = %listed_at, relayed = ctx.internet.is_relayed(listed_at), "the viewer moved");
                    ctx.changed();
                }
                continue;
            }
        };
        let Some(evt) = evt else { break };
        if let SessionEvt::Client(ClientMsg::SetControl { .. } | ClientMsg::Focus { .. } | ClientMsg::SetDisplay { .. }) = evt
            && !requests.allow(Instant::now(), MAX_CONTROL_REQUESTS_PER_SEC)
        {
            bail!("more than {MAX_CONTROL_REQUESTS_PER_SEC} control requests a second");
        }
        if let SessionEvt::Client(ClientMsg::SetDisplay { .. }) = evt
            && !display_requests.allow(Instant::now(), MAX_DISPLAY_REQUESTS_PER_SEC)
        {
            bail!("more than {MAX_DISPLAY_REQUESTS_PER_SEC} display requests a second");
        }
        match evt {
            SessionEvt::Client(ClientMsg::RequestKeyframe) => control.stream.request_keyframes(u64::MAX),
            SessionEvt::Client(ClientMsg::RequestKeyframes { tiles }) => control.stream.request_keyframes(tiles),
            SessionEvt::Client(ClientMsg::NoFullFrame) => {
                // For good, and first: streams started from now on (a switch may be under way)
                // make no full-frame encoder; then the one running stops using its own.
                screen.streamer.video_out.no_full_frame.store(true, Ordering::Release);
                control.stream.no_full_frame();
            }
            SessionEvt::Client(ClientMsg::Ping { client_time_us }) => {
                let now = clock::now_us();
                control.shared.last_ping_us.store(now, Ordering::Release);
                // Only for clock sync: skipped if the viewer isn't reading.
                let _ = out.try_send(HostMsg::Pong { client_time_us, host_time_us: now });
            }
            SessionEvt::Client(ClientMsg::SetControl { on, request, take_over }) => control.set(on, take_over, request),
            SessionEvt::Client(ClientMsg::Focus { forwarding }) => control.focus(forwarding),
            SessionEvt::Client(ClientMsg::SetDisplay { request, display }) => screen.request(request, display),
            SessionEvt::Client(other) => bail!("unexpected message {other:?}"),
            SessionEvt::Closed(None) => break,
            SessionEvt::Closed(Some(e)) => return Err(e),
            SessionEvt::InputStream(recv) => control.attach_input(recv),
            SessionEvt::Revoke(reason, message) => control.revoke(reason, message),
            SessionEvt::Cursor(update) => control.cursor(update),
            SessionEvt::Display(notice) => screen.notice(notice),
            SessionEvt::Switched(done) => {
                if let Err(e) = screen.switched(*done) {
                    conn.close(7u32.into(), b"no display to show");
                    return Err(e);
                }
            }
            SessionEvt::Availability => screen.availability_changed(),
            SessionEvt::CaptureStopped(generation) => screen.capture_stopped(generation),
        }
    }
    Ok(())
}

const PERMISSION_CHECK: Duration = Duration::from_secs(1);

/// Holds a place among the handshakes in progress (see [`MAX_HANDSHAKES`]).
struct Handshake {
    ctx: Arc<HostCtx>,
    /// Whether it came from the internet, and from where.
    key: (bool, IpAddr),
}

impl Handshake {
    fn enter(ctx: &Arc<HostCtx>, ip: IpAddr, internet: bool) -> Option<Self> {
        let max = if internet { MAX_HANDSHAKES_INTERNET } else { MAX_HANDSHAKES };
        let key = (internet, ip);
        let mut handshakes = ctx.handshakes.lock().unwrap();
        let total: usize = handshakes.iter().filter(|((from_internet, _), _)| *from_internet == internet).map(|(_, n)| n).sum();
        let from_ip = handshakes.get(&key).copied().unwrap_or(0);
        if total >= max || from_ip >= MAX_HANDSHAKES_PER_ADDRESS {
            return None;
        }
        handshakes.insert(key, from_ip + 1);
        Some(Self { ctx: ctx.clone(), key })
    }
}

impl Drop for Handshake {
    fn drop(&mut self) {
        let mut handshakes = self.ctx.handshakes.lock().unwrap();
        if let Some(n) = handshakes.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                handshakes.remove(&self.key);
            }
        }
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
    /// News about the virtual display the session watches, or about the Mac's displays.
    Display(DisplayNotice),
    /// A display switch is done (see [`Screen`]).
    Switched(Box<Switched>),
    /// Whether the viewer may have a virtual display may have changed (a setting).
    Availability,
    /// ScreenCaptureKit stopped the capture of this generation of the stream by itself.
    CaptureStopped(u64),
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
    /// Shared with the input thread, which also reads which display pointer input goes to.
    shared: Arc<InputShared>,
    input: Option<InputThread>,
    input_starts: RateLimit,
    controlling: Arc<AtomicBool>,
    cursor: Option<CursorMonitor>,
    /// The monitor hasn't reported the cursor yet, or can't read its shape: the viewer can't
    /// draw ours, so it stays in the video.
    cursor_unknown: bool,
    /// Whether the captured video should include the cursor.
    cursor_wish: CursorWish,
    /// The viewer's window has the focus (it draws our cursor only then). It says so after each
    /// grant; until then the cursor stays in the video.
    forwarding: bool,
    /// The stream sent now (replaced when the viewer switches displays).
    stream: Arc<StreamSlot>,
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
        // Until the displaced session handles this, its input thread may still inject a batch,
        // so for a moment two viewers' input can mix (two Dock swipes confuse the Dock until both
        // end; the revoke cancels its one). Rare and brief, so accepted.
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
        // The wish is kept even while no stream runs: the next stream starts with it.
        if self.cursor_wish.wanted.swap(show, Ordering::AcqRel) == show || !self.ctx.video {
            return;
        }
        let (wish, stream) = (self.cursor_wish.clone(), self.stream.clone());
        // ScreenCaptureKit can take a while to apply it: never wait for that on the runtime. One
        // update at a time, each applying the latest wish to the stream there is then (a display
        // switch holds the same lock while it replaces the stream).
        tokio::task::spawn_blocking(move || {
            let _one_at_a_time = wish.apply.lock().unwrap();
            let Some(capturer) = stream.capturer() else { return };
            let show = wish.wanted.load(Ordering::Acquire);
            let started = std::time::Instant::now();
            match capturer.set_shows_cursor(show) {
                Ok(()) => tracing::debug!(show, ms = started.elapsed().as_secs_f64() * 1000.0, "cursor in video"),
                Err(e) => tracing::warn!("show cursor in video: {e:#}"),
            }
        });
    }
}

/// Whether the video should show the cursor, and the lock that applies it one change at a time.
#[derive(Clone)]
struct CursorWish {
    wanted: Arc<AtomicBool>,
    apply: Arc<Mutex<()>>,
}

impl Default for CursorWish {
    fn default() -> Self {
        Self { wanted: Arc::new(AtomicBool::new(true)), apply: Arc::default() }
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
    ctx.internet.sync_keys();
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

/// Up to the viewer's refresh rate, whatever the size: only the tiles that changed are encoded,
/// all at once, so typing or a moving window on a 6K display keeps up with 120 Hz. A change of
/// the whole screen still costs about a whole-frame encode (~16 ms at 6144×2560); newest-frame-wins
/// then lowers the frame rate, never adds latency.
fn choose_fps(requested: u32) -> u32 {
    requested.clamp(15, 120)
}

/// Over the internet a display streams at most this often: frames past it would only split the
/// same bitrate thinner.
const INTERNET_MAX_FPS: u32 = 60;

/// The frame rate to stream a display of `refresh_hz` at, over the `internet` or not.
fn stream_fps(refresh_hz: u32, internet: bool) -> u32 {
    let fps = choose_fps(refresh_hz);
    if internet { fps.min(INTERNET_MAX_FPS) } else { fps }
}

/// Generous LAN bitrate: about 0.12 bits per pixel per frame, so text stays sharp.
fn bitrate_for(width: u32, height: u32, fps: u32) -> u32 {
    let bps = width as f64 * height as f64 * fps as f64 * 0.12;
    bps.clamp(8e6, 150e6) as u32
}

/// Each tile's bitrate. Tiles that moved lately (`active`) share the stream's budget by area, so
/// a video playing in one tile may get all of it; the others keep their area's share, enough for
/// a sharp keyframe if one is asked for. Still tiles send nothing, so the stream as a whole stays
/// near its budget. Every tile gets at least `floor` (see [`tile_floor`]).
fn tile_bitrates(stream_bps: u32, tiles: &[TileRect], active: u64, floor: u32) -> Vec<u32> {
    let area = |t: &TileRect| u64::from(t.width) * u64::from(t.height);
    let total: u64 = tiles.iter().map(area).sum();
    let moving: u64 = tiles.iter().enumerate().filter(|&(i, _)| active & tile_bit(i) != 0).map(|(_, t)| area(t)).sum();
    tiles
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let among = if active & tile_bit(i) != 0 { moving } else { total };
            let share = u64::from(stream_bps) * area(t) / among.max(1);
            (share.min(u64::from(u32::MAX)) as u32).max(floor)
        })
        .collect()
}

/// The least bitrate a tile of a `stream_bps` stream in `tiles` tiles gets: [`MIN_TILE_BITRATE`],
/// enough for a sharp keyframe. Over the internet (`wan`) the stream's bitrate is what the
/// connection carries, so all the tiles at their least must fit in it: when the whole picture
/// moves, a floor over its share would make the stream overshoot, and the excess would queue.
fn tile_floor(stream_bps: u32, tiles: usize, wan: bool) -> u32 {
    if wan { MIN_TILE_BITRATE.min(stream_bps / tiles.max(1) as u32) } else { MIN_TILE_BITRATE }
}

/// Where the picture moved lately, and the bitrates the tile encoders were given for it.
struct Motion {
    stream_bps: u32,
    tiles: Vec<TileRect>,
    /// Over the internet (see [`tile_floor`]).
    wan: bool,
    changed_at: Vec<Option<Instant>>,
    /// As the encoders have them.
    bitrates: Vec<u32>,
    next_check: Instant,
    /// `stream_bps` changed: the next check tells every tile its share, however small the change.
    budget_changed: bool,
}

impl Motion {
    /// `wan`: over the internet (see [`tile_floor`]).
    fn new(stream_bps: u32, tiles: Vec<TileRect>, wan: bool, now: Instant) -> Self {
        let bitrates = tile_bitrates(stream_bps, &tiles, 0, tile_floor(stream_bps, tiles.len(), wan));
        Self {
            stream_bps,
            changed_at: vec![None; tiles.len()],
            tiles,
            wan,
            bitrates,
            next_check: now + RETARGET_INTERVAL,
            budget_changed: false,
        }
    }

    /// The stream's bitrate is now `stream_bps` (over the internet it follows the connection):
    /// the tiles get their new shares at the next update, even a cut too small to be worth it
    /// otherwise, since it is what the connection carries.
    fn set_stream_bps(&mut self, stream_bps: u32, now: Instant) {
        if stream_bps != self.stream_bps {
            self.stream_bps = stream_bps;
            self.budget_changed = true;
            self.next_check = now;
        }
    }

    fn changed(&mut self, tiles: u64, now: Instant) {
        for (i, at) in self.changed_at.iter_mut().enumerate() {
            if tiles & tile_bit(i) != 0 {
                *at = Some(now);
            }
        }
    }

    /// Every [`RETARGET_INTERVAL`]: the tiles whose encoders should get another bitrate, and
    /// which. Small corrections aren't worth telling an encoder about.
    fn retarget(&mut self, now: Instant) -> Vec<(usize, u32)> {
        if now < self.next_check {
            return Vec::new();
        }
        self.next_check = now + RETARGET_INTERVAL;
        let active = self
            .changed_at
            .iter()
            .enumerate()
            .filter(|(_, at)| at.is_some_and(|at| now.saturating_duration_since(at) < MOTION_WINDOW))
            .fold(0, |mask, (i, _)| mask | tile_bit(i));
        let mut changes = Vec::new();
        let floor = tile_floor(self.stream_bps, self.tiles.len(), self.wan);
        for (i, target) in tile_bitrates(self.stream_bps, &self.tiles, active, floor).into_iter().enumerate() {
            let current = self.bitrates[i];
            if (f64::from(target) - f64::from(current)).abs() > RETARGET_MIN_CHANGE * f64::from(current)
                || (self.budget_changed && target != current)
            {
                self.bitrates[i] = target;
                changes.push((i, target));
            }
        }
        self.budget_changed = false;
        changes
    }
}

/// `LANKVM_TILES=COLSxROWS` forces the tile grid, e.g. `1x1` for one encoder for the whole picture.
fn tile_grid() -> Option<(u32, u32)> {
    let value = std::env::var("LANKVM_TILES").ok()?;
    let grid = parse_grid(&value);
    if grid.is_none() {
        tracing::warn!("ignoring LANKVM_TILES={value:?}: expected COLSxROWS, e.g. 2x8");
    }
    grid
}

fn parse_grid(value: &str) -> Option<(u32, u32)> {
    let (cols, rows) = value.trim().split_once(['x', 'X'])?;
    let (cols, rows): (u32, u32) = (cols.trim().parse().ok()?, rows.trim().parse().ok()?);
    (cols >= 1 && rows >= 1).then_some((cols, rows))
}

/// Bit `index` of a tile mask.
fn tile_bit(index: usize) -> u64 {
    1u64 << index
}

/// The full-frame stream's bit in a tile mask.
const FULL_FRAME_BIT: u64 = 1u64 << FULL_FRAME_TILE;

/// Updates whose numbers and tile masks [`VideoOut`] remembers. One update is encoded at a time,
/// so its tiles are out long before its slot comes round again.
const UPDATE_MASKS: usize = 8;

/// One update as [`VideoOut`] remembers it.
#[derive(Clone, Copy, Default)]
struct UpdateSlot {
    /// Its encoders' tag.
    tag: u64,
    /// Its number on the wire, once one of its frames went out.
    update: Option<u32>,
    /// Its tiles: those it encodes until it is done, then those that went out.
    mask: u64,
    /// It is done: a frame of it its encoder puts out only now isn't sent (see
    /// [`VideoOut::send`]).
    closed: bool,
}

/// What a connection's video keeps from one stream to the next (a display switch replaces the
/// stream): each tile's frame ids and the update numbers keep counting up, or the viewer would
/// drop the new stream's frames as late.
struct VideoOut {
    conn: Connection,
    /// By tile index: each tile is reassembled on its own.
    packetizers: Vec<Mutex<Packetizer>>,
    /// Numbers updates on the wire ([`VideoFrame::update`]); wraps. Only updates that send
    /// something take one, so to the viewer a gap is always a loss on the way.
    next_update: AtomicU32,
    /// Tags the updates being encoded (the encoders' tags).
    next_tag: AtomicU64,
    /// Update tagged `t` is in slot `t % UPDATE_MASKS`, set before it is encoded.
    slots: Mutex<[UpdateSlot; UPDATE_MASKS]>,
    /// The control stream, once the viewer has been welcomed: for [`HostMsg::VideoIdle`].
    notices: Mutex<Option<mpsc::Sender<HostMsg>>>,
    /// The viewer can't decode the full-frame stream ([`ClientMsg::NoFullFrame`]): every stream
    /// of this connection sends changes as tiles only.
    no_full_frame: AtomicBool,
    /// Over the internet: what the connection carries (None on the local network).
    wan: Option<WanLimits>,
}

/// The limits an internet connection's video keeps to, set by its rate controller every
/// [`rate::TICK`] (see [`follow_the_connection`]) and read by the encode thread.
struct WanLimits {
    /// The stream's bitrate ceiling, and whether it changed since the encode thread last looked.
    ceiling_bps: AtomicU32,
    changed: AtomicBool,
    /// The running stream's LAN bitrate: the ceiling never goes above it.
    cap_bps: AtomicU32,
    /// Datagram bytes that may wait in QUIC's buffer before the next update is held back.
    backlog_limit: AtomicUsize,
    /// Least time between keyframes served on request (µs): about a round trip.
    keyframe_gap_us: AtomicU64,
    /// Free space in the datagram send buffer while nothing waits there.
    empty_space: usize,
    /// How fast the connection sends (see [`rate::pace_for`]).
    pace: Pace,
    /// The connection goes through a LanKVM server's relay, which carries only so much (see
    /// [`rate::RELAYED_MAX_BPS`]).
    relayed: AtomicBool,
}

impl VideoOut {
    /// Over the `internet`, streams send tiles only: a full frame is hundreds of packets, and on
    /// a path that loses one now and then most would arrive incomplete.
    fn new(conn: Connection, internet: bool) -> Self {
        Self::with_pace(conn, internet.then(Pace::new))
    }

    /// Over the internet, on a connection sending at `pace` (see [`Network::internet_server_config`]).
    fn over_the_internet(conn: Connection, pace: Pace) -> Self {
        Self::with_pace(conn, Some(pace))
    }

    fn with_pace(conn: Connection, pace: Option<Pace>) -> Self {
        let packetizers = (0..MAX_TILES).map(|i| Mutex::new(Packetizer::for_tile(i as u8))).collect();
        let internet = pace.is_some();
        let wan = pace.map(|pace| WanLimits {
            ceiling_bps: AtomicU32::new(rate::START_BPS),
            changed: AtomicBool::new(false),
            cap_bps: AtomicU32::new(rate::START_BPS),
            backlog_limit: AtomicUsize::new(rate::backlog_limit(rate::START_BPS)),
            keyframe_gap_us: AtomicU64::new(rate::keyframe_gap(conn.rtt()).as_micros() as u64),
            // Nothing was sent yet: the connection's video goes out through here only.
            empty_space: conn.datagram_send_buffer_space(),
            pace,
            relayed: AtomicBool::new(false),
        });
        Self {
            conn,
            packetizers,
            next_update: AtomicU32::new(0),
            next_tag: AtomicU64::new(0),
            slots: Mutex::new([UpdateSlot { tag: NOT_SENT, ..UpdateSlot::default() }; UPDATE_MASKS]),
            notices: Mutex::default(),
            no_full_frame: AtomicBool::new(internet),
            wan,
        }
    }

    /// The bitrate a new stream of LAN bitrate `lan_bps` starts at: over the internet, no more
    /// than the connection carries.
    fn start_bitrate(&self, lan_bps: u32) -> u32 {
        match &self.wan {
            None => lan_bps,
            Some(wan) => {
                wan.cap_bps.store(lan_bps, Ordering::Release);
                self.wan_cap().min(wan.ceiling_bps.load(Ordering::Acquire))
            }
        }
    }

    /// Over the internet, the most the stream may use: its LAN bitrate, and through a relay no
    /// more than that carries.
    fn wan_cap(&self) -> u32 {
        let Some(wan) = &self.wan else { return u32::MAX };
        let cap = wan.cap_bps.load(Ordering::Acquire);
        if wan.relayed.load(Ordering::Acquire) { cap.min(rate::RELAYED_MAX_BPS) } else { cap }
    }

    /// The stream's new bitrate, if the ceiling changed since the last call (never on the local
    /// network).
    fn take_ceiling(&self) -> Option<u32> {
        let wan = self.wan.as_ref()?;
        wan.changed.swap(false, Ordering::AcqRel).then(|| wan.ceiling_bps.load(Ordering::Acquire).min(self.wan_cap()))
    }

    /// Bytes of datagrams waiting in QUIC's send buffer (over the internet only).
    fn backlog(&self) -> usize {
        self.wan.as_ref().map_or(0, |wan| wan.empty_space.saturating_sub(self.conn.datagram_send_buffer_space()))
    }

    /// Over the internet: more video waits in QUIC's send buffer than should, so the next update
    /// waits (always false on the local network).
    fn backlogged(&self) -> bool {
        self.wan.as_ref().is_some_and(|wan| self.backlog() > wan.backlog_limit.load(Ordering::Relaxed))
    }

    /// Over the internet, the least time between keyframes served on request.
    fn keyframe_gap(&self) -> Option<Duration> {
        self.wan.as_ref().map(|wan| Duration::from_micros(wan.keyframe_gap_us.load(Ordering::Relaxed)))
    }

    fn set_notices(&self, out: mpsc::Sender<HostMsg>) {
        *self.notices.lock().unwrap() = Some(out);
    }

    /// Tells the viewer the screen went still after `update`, which sent the tiles in `mask`.
    fn idle(&self, update: u32, mask: u64) {
        if let Some(out) = self.notices.lock().unwrap().as_ref() {
            // A safety net only: skipped if the viewer isn't reading its control stream.
            let _ = out.try_send(HostMsg::VideoIdle { update, mask });
        }
    }

    /// Starts an update encoding the tiles of `mask`; returns its encoders' tag.
    fn start_update(&self, mask: u64) -> u64 {
        let tag = self.next_tag.fetch_add(1, Ordering::Relaxed);
        self.slots.lock().unwrap()[tag as usize % UPDATE_MASKS] = UpdateSlot { tag, update: None, mask, closed: false };
        tag
    }

    /// The update tagged `tag` is done: only the tiles in `sent` went out. Returns its number,
    /// if it has one (something went out). The viewer hears of the others no more: the next
    /// update's `previous_mask` and [`HostMsg::VideoIdle`] name only what was sent.
    fn finish_update(&self, tag: u64, sent: u64) -> Option<u32> {
        let mut slots = self.slots.lock().unwrap();
        let slot = &mut slots[tag as usize % UPDATE_MASKS];
        if slot.tag != tag {
            return None;
        }
        slot.mask = sent;
        slot.closed = true;
        slot.update
    }

    /// Sends one encoded tile of a `stream`-sized picture, right away. False if it didn't go out,
    /// also when it is a frame of an update already done: the encoder took too long, the update
    /// counted it as lost, and newer pictures may have gone out since (a number now would make
    /// this older one look newer).
    fn send(&self, frame: EncodedFrame, tile: TileRect, stream: (u32, u32)) -> bool {
        let (update, update_mask, previous_mask) = {
            let mut slots = self.slots.lock().unwrap();
            let at = frame.tag as usize % UPDATE_MASKS;
            if slots[at].tag != frame.tag || slots[at].closed {
                return false;
            }
            let next = &self.next_update;
            let update = *slots[at].update.get_or_insert_with(|| next.fetch_add(1, Ordering::Relaxed));
            let mask = slots[at].mask;
            let previous = slots.iter().find(|s| s.update == Some(update.wrapping_sub(1))).map_or(0, |s| s.mask);
            (update, mask, previous)
        };
        let video = VideoFrame {
            codec: frame.codec,
            keyframe: frame.keyframe,
            width: stream.0,
            height: stream.1,
            tile,
            update,
            update_mask,
            previous_mask,
            capture_time_us: frame.capture_time_us,
            encode_start_us: frame.encode_start_us,
            encoded_time_us: clock::now_us(),
            param_sets: frame.param_sets,
            nal_length_size: frame.nal_length_size,
            data: frame.data,
            cover: Vec::new(),
        };
        let Some(max) = self.conn.max_datagram_size() else { return false };
        let Some(packetizer) = self.packetizers.get(usize::from(tile.index)) else { return false };
        let packets = protocol::encode(&video)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| packetizer.lock().unwrap().packetize(&bytes, max));
        match packets {
            Ok(packets) => packets.into_iter().all(|packet| self.conn.send_datagram(packet).is_ok()), // false: closing
            Err(e) => {
                tracing::warn!(tile = tile.index, "packetize: {e:#}");
                false
            }
        }
    }
}

/// Over the internet: every [`rate::TICK`], samples what the connection did and sets `video`'s
/// limits to what it carries (see [`crate::rate`]), and the connection's pace to match. Runs as
/// long as the session.
async fn follow_the_connection(conn: Connection, video: Arc<VideoOut>, internet: Arc<HostInternet>) {
    let Some(wan) = video.wan.as_ref() else { return };
    // The connection's path, and whether it goes through a relay: it can move off one (or onto
    // one) mid-session.
    let mut path = conn.remote_address();
    wan.relayed.store(internet.is_relayed(path), Ordering::Release);
    let mut control = RateControl::new(video.wan_cap());
    let publish = |ceiling: u32| {
        wan.backlog_limit.store(rate::backlog_limit(ceiling), Ordering::Relaxed);
        wan.pace.set(rate::pace_for(ceiling));
        wan.ceiling_bps.store(ceiling, Ordering::Release);
        wan.changed.store(true, Ordering::Release);
    };
    publish(control.ceiling());
    let mut tick = tokio::time::interval(rate::TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    let (mut at, mut stats) = (Instant::now(), conn.stats());
    loop {
        tick.tick().await;
        let (now, next) = (Instant::now(), conn.stats());
        let sample = Sample::between(&stats, &next, now - at, video.backlog());
        (at, stats) = (now, next);
        wan.keyframe_gap_us.store(rate::keyframe_gap(sample.rtt).as_micros() as u64, Ordering::Relaxed);
        if conn.remote_address() != path {
            path = conn.remote_address();
            wan.relayed.store(internet.is_relayed(path), Ordering::Release);
            control.path_changed();
        }
        let capped = control.set_cap(video.wan_cap());
        if let Some(ceiling) = control.on_sample(&sample).or(capped) {
            tracing::debug!(
                ceiling_mbps = f64::from(ceiling) / 1e6,
                sent_mbps = control.send_rate() / 1e6,
                rtt_ms = sample.rtt.as_secs_f64() * 1e3,
                lost = sample.lost_packets,
                backlog = sample.backlog,
                "video bitrate follows the connection"
            );
            publish(ceiling);
        }
    }
}

/// Never a real tag: tags count up from 0 and would take ages to get there.
const NOT_SENT: u64 = u64::MAX;

/// What became of one encoder's frames: written by its output callback, read once the encoder
/// is idle.
struct Track {
    /// Tag of the newest frame the encoder put out, whether it went out or not.
    emitted: AtomicU64,
    /// Tag of the newest frame that went out, and of the newest keyframe that did.
    sent: AtomicU64,
    sent_keyframe: AtomicU64,
    /// A frame came out that never went out: the encoder's references moved on, the viewer's
    /// didn't, so its next frame must be a keyframe.
    repair: AtomicBool,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            emitted: AtomicU64::new(NOT_SENT),
            sent: AtomicU64::new(NOT_SENT),
            sent_keyframe: AtomicU64::new(NOT_SENT),
            repair: AtomicBool::new(false),
        }
    }
}

/// What became of a frame submitted to an encoder.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fate {
    Sent { keyframe: bool },
    /// Encoded, but it never reached the network: the encoder's references moved on, the
    /// viewer's didn't, so that stream needs a keyframe.
    Unsent,
    /// The encoder put nothing out (dropped, failed or late): the viewer's decoder is where the
    /// encoder's is, and only the picture is missing.
    Lost,
}

impl Track {
    /// The encoder's output callback: sends the frame.
    fn deliver(&self, video: &VideoOut, frame: EncodedFrame, tile: TileRect, stream: (u32, u32)) {
        let (tag, keyframe) = (frame.tag, frame.keyframe);
        self.emitted.store(tag, Ordering::Release);
        if video.send(frame, tile, stream) {
            if keyframe {
                self.sent_keyframe.store(tag, Ordering::Release);
            }
            self.sent.store(tag, Ordering::Release);
        } else {
            self.repair.store(true, Ordering::Release);
        }
    }

    /// The frame tagged `tag`, once its encoder is idle.
    fn fate(&self, tag: u64) -> Fate {
        if self.sent.load(Ordering::Acquire) == tag {
            Fate::Sent { keyframe: self.sent_keyframe.load(Ordering::Acquire) == tag }
        } else if self.emitted.load(Ordering::Acquire) == tag {
            Fate::Unsent
        } else {
            Fate::Lost
        }
    }
}

/// A stream's encoders, one per tile and one for the full frame, each sending its frames the
/// moment they're done.
struct TileEncoders {
    stream: (u32, u32),
    /// The whole stream's.
    bitrate_bps: u32,
    tiles: Vec<TileRect>,
    encoders: Vec<Encoder>,
    tracks: Vec<Arc<Track>>,
    /// The whole picture's ([`FULL_FRAME_TILE`]), when there are several tiles.
    full: Option<(Encoder, Arc<Track>)>,
    /// Changed tiles covering at least this fraction of the picture go out as one full frame.
    full_frame_at: f64,
    /// To make a tile's encoder again (see [`TileEncoders::rebuild`]).
    video: Arc<VideoOut>,
    cfg: EncoderConfig,
}

impl TileEncoders {
    /// `cfg` is the whole stream's; each tile gets its share of the bitrate.
    fn new(video: &Arc<VideoOut>, cfg: &EncoderConfig, tiles: Vec<TileRect>, full_frame_at: f64) -> Result<Self> {
        let stream = (cfg.width, cfg.height);
        let whole = TileRect { index: FULL_FRAME_TILE, x: 0, y: 0, width: cfg.width, height: cfg.height };
        let with_full = tiles.len() > 1 && !video.no_full_frame.load(Ordering::Acquire);
        let mut rects = tiles.clone();
        if with_full {
            rects.push(whole);
        }
        let bitrates = tile_bitrates(cfg.bitrate_bps, &tiles, 0, tile_floor(cfg.bitrate_bps, tiles.len(), video.wan.is_some()));
        let mut tracks: Vec<Arc<Track>> = rects.iter().map(|_| Arc::default()).collect();
        // Making a session takes some 20 ms, mostly waiting for the media server: together, a
        // display switch waits for the slowest instead of all of them in turn.
        let mut made: Vec<Result<Encoder>> = std::thread::scope(|scope| {
            let threads: Vec<_> = rects
                .iter()
                .zip(&tracks)
                .enumerate()
                .map(|(i, (&rect, track))| {
                    let bitrate_bps = bitrates.get(i).copied().unwrap_or(cfg.bitrate_bps);
                    let tile_cfg = EncoderConfig { width: rect.width, height: rect.height, bitrate_bps, ..*cfg };
                    let (video, track) = (video.clone(), track.clone());
                    scope.spawn(move || Encoder::new(&tile_cfg, move |frame| track.deliver(&video, frame, rect, stream)))
                })
                .collect();
            threads.into_iter().map(|t| t.join().unwrap_or_else(|_| Err(anyhow::anyhow!("encoder setup panicked")))).collect()
        });
        let full = if with_full { Some((made.pop().expect("full-frame encoder"), tracks.pop().expect("its track"))) } else { None };
        let mut encoders = Vec::with_capacity(tiles.len());
        for (i, (encoder, tile)) in made.into_iter().zip(&tiles).enumerate() {
            let encoder = encoder.with_context(|| format!("encoder for tile {i} ({}×{})", tile.width, tile.height))?;
            // A software encoder per tile would be far too slow; one for the whole picture is the
            // lesser evil.
            if tiles.len() > 1 && !encoder.hardware() {
                bail!("no hardware encoder for tile {i}");
            }
            encoders.push(encoder);
        }
        let full = match full {
            Some((Ok(encoder), track)) if encoder.hardware() && encoder.codec() == encoders[0].codec() => Some((encoder, track)),
            Some((Ok(_), _)) => {
                tracing::warn!("no hardware encoder like the tiles' for the full frame: big changes go out as tiles");
                None
            }
            Some((Err(e), _)) => {
                tracing::warn!("full-frame encoder: {e:#}; big changes go out as tiles");
                None
            }
            None => None,
        };
        Ok(Self { stream, bitrate_bps: cfg.bitrate_bps, tiles, encoders, tracks, full, full_frame_at, video: video.clone(), cfg: *cfg })
    }

    /// Makes tile `i`'s encoder again (its session broke): its first frame is a keyframe.
    fn rebuild(&mut self, i: usize, bitrate_bps: u32) -> Result<()> {
        let rect = self.tiles[i];
        let (video, track, stream) = (self.video.clone(), self.tracks[i].clone(), self.stream);
        let cfg = EncoderConfig { width: rect.width, height: rect.height, bitrate_bps, ..self.cfg };
        self.encoders[i] = Encoder::new(&cfg, move |frame| track.deliver(&video, frame, rect, stream))?;
        Ok(())
    }

    fn codec(&self) -> Codec {
        self.encoders[0].codec()
    }

    /// Mask of every tile.
    fn mask(&self) -> u64 {
        u64::MAX >> (64 - self.tiles.len())
    }

    /// Whether changed tiles `changed` cover enough of the picture to go out as one full frame.
    fn full_frame(&self, changed: u64) -> bool {
        if self.full.is_none() || changed == 0 {
            return false;
        }
        let area = |t: &TileRect| t.width as f64 * t.height as f64;
        let covered: f64 = self.tiles.iter().enumerate().filter(|&(i, _)| changed & tile_bit(i) != 0).map(|(_, t)| area(t)).sum();
        covered >= self.full_frame_at * self.stream.0 as f64 * self.stream.1 as f64
    }
}

/// Changed tiles covering this fraction of the picture go out as one full frame
/// ([`FULL_FRAME_TILE`]); `LANKVM_FULL_FRAME_AT` overrides it (above 1: never). Measured at
/// 6144×2560 on an M3 Max with nothing else encoding: one tile out after ~3.5 ms, and each more
/// ~1.5 ms (the media engine takes the sessions one after another), one full frame ~17 ms; they
/// break even at about 10 of 16 tiles.
const FULL_FRAME_AT: f64 = 0.6;

fn full_frame_at() -> f64 {
    let Ok(value) = std::env::var("LANKVM_FULL_FRAME_AT") else { return FULL_FRAME_AT };
    match value.trim().parse::<f64>() {
        Ok(at) if at.is_finite() && at >= 0.0 => at,
        _ => {
            tracing::warn!("ignoring LANKVM_FULL_FRAME_AT={value:?}: expected a fraction of the picture, e.g. 0.75");
            FULL_FRAME_AT
        }
    }
}

/// Captures the display and streams encoded tiles to one viewer.
struct StreamSession {
    capturer: Option<Arc<Capturer>>,
    shared: Arc<Shared>,
    encode_thread: Option<JoinHandle<()>>,
    codec: Codec,
}

/// Capture runs ahead of the encoders, which take one frame at a time and always the newest. A
/// frame that would have to queue behind another is replaced by a newer one instead, so a slow
/// encode costs frame rate, never latency.
struct Shared {
    state: Mutex<EncodeState>,
    wake: Condvar,
    /// The tiles, and a mask of them all.
    layout: Vec<TileRect>,
    tiles: u64,
    /// Whether the stream has a full-frame encoder (until the viewer says it can't decode it).
    full: AtomicBool,
    /// Whether frames come at the display's own size, where ScreenCaptureKit's dirty
    /// rectangles are in the frame's pixels. (Scaled, they might not be: then they aren't used.)
    hints: bool,
}

#[derive(Default)]
struct EncodeState {
    /// Captured and waiting for the encoders; a newer capture replaces it.
    next: Option<CapturedFrame>,
    /// `next` waits until then: the tiler failed on a frame lately (see [`TilerRetry`]).
    retry_at: Option<Instant>,
    /// Tiles the frames since the one the tiler last took changed, by their dirty rectangles
    /// (all, when one didn't say): frames the encoders skip still changed what they changed.
    touched: u64,
    requests: Requests,
    stop: bool,
}

/// What an update sends besides the tiles that changed.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Work {
    /// Tiles to encode as keyframes (from their last content, unless they changed).
    keyframes: u64,
    /// Tiles to encode again from their last content, as ordinary frames: the viewer never got
    /// their newest picture, though its decoders are fine.
    resends: u64,
    /// The full-frame stream's next frame must be a keyframe.
    full_keyframe: bool,
    /// The viewer can't decode the full-frame stream: stop using it.
    drop_full: bool,
}

/// Keyframes and resends asked for, by the viewer or after a lost frame: they merge into masks
/// (nothing is ever dropped), served together at most every [`KEYFRAME_MIN_INTERVAL`] (over the
/// internet, about a round trip), so a viewer that keeps losing packets can't turn the stream into
/// all-keyframes.
#[derive(Default)]
struct Requests {
    keyframes: u64,
    resends: u64,
    full_keyframe: bool,
    drop_full: bool,
    /// Every tile was sent again for a lost full frame, and no full frame went out since: a viewer
    /// asking again meanwhile misses the same picture, which those tiles already carry.
    full_resent: bool,
    served: Option<Instant>,
    /// Keyframes still waiting when the update in progress started.
    held: u64,
    /// Over the internet, the spacing instead of [`KEYFRAME_MIN_INTERVAL`]: about a round trip
    /// (see [`rate::keyframe_gap`]).
    gap: Option<Duration>,
}

impl Requests {
    /// What a viewer asks for: tile `i` of `layout` as a keyframe for bit `i`. Bit
    /// [`FULL_FRAME_TILE`] (if the stream has a full-frame encoder) is the full-frame stream:
    /// a frame of it was lost, so its next frame is a keyframe, and the viewer lacks whatever
    /// that frame showed, so every tile is sent again (their own streams are intact).
    fn ask(&mut self, tiles: u64, layout: u64, full: bool) {
        self.keyframes |= tiles & layout;
        if full && tiles & FULL_FRAME_BIT != 0 {
            self.full_keyframe = true;
            if !self.full_resent {
                self.full_resent = true;
                self.resends |= layout;
            }
        }
    }

    /// The viewer can't decode full frames: stop sending them, and send every tile again (what
    /// the full frames it dropped showed is missing from its picture).
    fn no_full_frame(&mut self, layout: u64) {
        self.drop_full = true;
        self.resends |= layout;
    }

    /// How long until the pending tiles may be served; None if there are none.
    fn wait(&self, now: Instant) -> Option<Duration> {
        let gap = self.gap.unwrap_or(KEYFRAME_MIN_INTERVAL);
        (self.keyframes | self.resends != 0).then(|| self.served.map_or(Duration::ZERO, |t| (t + gap).saturating_duration_since(now)))
    }

    /// The work for the update starting now: the tiles only when it's their turn.
    fn take(&mut self, now: Instant) -> Work {
        let mut work = Work {
            full_keyframe: std::mem::take(&mut self.full_keyframe),
            drop_full: std::mem::take(&mut self.drop_full),
            ..Work::default()
        };
        if self.wait(now) == Some(Duration::ZERO) {
            self.served = Some(now);
            work.keyframes = std::mem::take(&mut self.keyframes);
            work.resends = std::mem::take(&mut self.resends) & !work.keyframes;
        }
        self.held = self.keyframes;
        work
    }

    /// The update is out. A tile it sent needs no resend any more (it showed the newest picture),
    /// and one it sent as a keyframe no keyframe asked for before it started. (One asked for
    /// since may be about that very keyframe.)
    fn done(&mut self, outcome: &Outcome) {
        if outcome.full_sent {
            self.full_resent = false;
        }
        self.resends = (self.resends & !outcome.sent) | outcome.resends;
        self.keyframes = (self.keyframes & !(outcome.sent_keyframes & self.held)) | outcome.keyframes;
    }
}

/// How an update went.
#[derive(Default)]
struct Outcome {
    /// Tiles that went out, and those of them that went as keyframes.
    sent: u64,
    sent_keyframes: u64,
    /// Tiles to send again ([`Fate::Lost`], or covered by a full frame that didn't go out).
    resends: u64,
    /// Tiles whose stream needs a keyframe ([`Fate::Unsent`], or a keyframe asked for that never
    /// came out).
    keyframes: u64,
    /// A full frame went out.
    full_sent: bool,
    /// The update's number and tile mask, if it sent anything.
    update: Option<(u32, u64)>,
    /// The captured frame, if the tiler failed on it: it's tried again.
    retry: Option<(CapturedFrame, anyhow::Error)>,
}

impl StreamSession {
    /// Frames go out through `video`, the connection's: frame ids and update numbers keep
    /// counting up when a new stream replaces an old one. `encoder` is the whole stream's.
    /// `on_stopped` is told if ScreenCaptureKit stops the capture by itself.
    fn start(
        video: Arc<VideoOut>,
        capture: &CaptureConfig,
        encoder: &EncoderConfig,
        on_stopped: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<Self> {
        let on_stopped: Arc<dyn Fn(String) + Send + Sync> = Arc::new(on_stopped);
        let (width, height) = (encoder.width, encoder.height);
        let layout = tile_layout(width, height, tile_grid());
        let at = full_frame_at();
        let started = Instant::now();
        // The media engine runs 32 hardware sessions at once, for every app: when another viewer
        // (or app) holds many, fewer, bigger tiles still beat one encoder for the whole picture.
        // (Columns stay at most TILE_MAX_WIDTH wide: some decoders take no wider.)
        let cols = width.div_ceil(TILE_MAX_WIDTH);
        let mut encoders = TileEncoders::new(&video, encoder, layout.clone(), at);
        for fewer in [Some((cols, 4)), Some((cols, 1)), Some((1, 1))] {
            let Err(e) = &encoders else { break };
            let fallback = tile_layout(width, height, fewer);
            if fallback.len() >= layout.len() {
                continue;
            }
            tracing::warn!("{} tiles: {e:#}; trying {}", layout.len(), fallback.len());
            encoders = TileEncoders::new(&video, encoder, fallback, at);
        }
        let encoders = encoders?;
        let tiler = Tiler::new(width, height, &encoders.tiles).context("tiler")?;
        let full = encoders.full.is_some();
        let hints = platform_mac::capture::native_pixel_size(capture.display_id) == Some((width, height));
        tracing::info!(width, height, tiles = encoders.tiles.len(), full_frame = full, full_frame_at = at, setup_ms = started.elapsed().as_millis() as u64, "tiled stream");
        let codec = encoders.codec();
        let shared = Arc::new(Shared {
            state: Mutex::new(EncodeState::default()),
            wake: Condvar::new(),
            layout: encoders.tiles.clone(),
            tiles: encoders.mask(),
            full: AtomicBool::new(full),
            hints,
        });
        let pipeline = Pipeline::new(tiler, encoders, on_stopped.clone());
        let encode_thread = std::thread::Builder::new().name("lankvm-encode".into()).spawn({
            let shared = shared.clone();
            move || encode_loop(&shared, pipeline, &video)
        })?;
        let mut session = Self { capturer: None, shared, encode_thread: Some(encode_thread), codec };
        let on_frame = session.shared.clone();
        session.capturer = Some(Arc::new(Capturer::start(capture, move |frame| on_frame.on_captured(frame), move |why| on_stopped(why))?));
        Ok(session)
    }

    fn codec(&self) -> Codec {
        self.codec
    }

    fn capturer(&self) -> Option<Arc<Capturer>> {
        self.capturer.clone()
    }

    fn request_keyframes(&self, tiles: u64) {
        self.shared.request_keyframes(tiles);
    }

    fn no_full_frame(&self) {
        self.shared.no_full_frame();
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        // Stop capture first so no new frames reach the encoders while they shut down.
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
        let touched = match (&frame.dirty, self.hints) {
            (Some(rects), true) => tiles_touched(&self.layout, rects),
            _ => u64::MAX,
        };
        let mut state = self.state.lock().unwrap();
        state.touched |= touched;
        state.next = Some(frame);
        drop(state);
        self.wake.notify_one();
    }

    /// Bits beyond the layout (and the full-frame stream's, without one) are ignored.
    fn request_keyframes(&self, tiles: u64) {
        let full = self.full.load(Ordering::Relaxed);
        if tiles & self.tiles != 0 || (full && tiles & FULL_FRAME_BIT != 0) {
            self.state.lock().unwrap().requests.ask(tiles, self.tiles, full);
            self.wake.notify_one();
        }
    }

    fn no_full_frame(&self) {
        if self.full.swap(false, Ordering::Relaxed) {
            self.state.lock().unwrap().requests.no_full_frame(self.tiles);
            self.wake.notify_one();
        }
    }
}

/// First wait before a frame the tiler failed on is tried again; it doubles while the tiler
/// keeps failing, up to [`MAX_TILER_RETRY`].
const TILER_RETRY: Duration = Duration::from_millis(5);
const MAX_TILER_RETRY: Duration = Duration::from_millis(500);
/// A tiler that keeps failing says so at most this often.
const TILER_WARN_INTERVAL: Duration = Duration::from_secs(5);
/// While video waits in QUIC's send buffer beyond its limit (over the internet), the next update
/// looks again this often whether it drained.
const BACKLOG_RECHECK: Duration = Duration::from_millis(2);

/// Paces the retries of frames the tiler failed on, and the warnings about it.
#[derive(Default)]
struct TilerRetry {
    delay: Duration,
    /// Failures not reported yet.
    failures: u32,
    warned: Option<Instant>,
}

impl TilerRetry {
    /// The tiler failed: when to try again, and the failures to report now (if it's time).
    fn failed(&mut self, now: Instant) -> (Instant, Option<u32>) {
        self.delay = (self.delay * 2).clamp(TILER_RETRY, MAX_TILER_RETRY);
        self.failures += 1;
        let report = self.warned.is_none_or(|t| now >= t + TILER_WARN_INTERVAL).then(|| {
            self.warned = Some(now);
            std::mem::take(&mut self.failures)
        });
        (now + self.delay, report)
    }

    fn succeeded(&mut self) {
        self.delay = Duration::ZERO;
    }
}

fn encode_loop(shared: &Shared, mut pipeline: Pipeline, video: &VideoOut) {
    // On the input-to-photon path: woken promptly and kept on a performance core.
    system::set_thread_interactive();
    let mut retry = TilerRetry::default();
    // When the screen will have been still long enough: the newest frame is then compared whole
    // if it wasn't, and the viewer told which update was the last (if not told yet).
    let mut quiet_at: Option<Instant> = None;
    let mut note: Option<(u32, u64)> = None;
    loop {
        // A frame or requests to send; or None: the screen has been still a while.
        let step = {
            let mut state = shared.state.lock().unwrap();
            loop {
                if state.stop {
                    return;
                }
                let now = Instant::now();
                if let Some(gap) = video.keyframe_gap() {
                    state.requests.gap = Some(gap);
                }
                let frame_wait = state.next.as_ref().map(|_| state.retry_at.map_or(Duration::ZERO, |t| t.saturating_duration_since(now)));
                let work_wait = [frame_wait, state.requests.wait(now)].into_iter().flatten().min();
                if work_wait == Some(Duration::ZERO) && video.backlogged() {
                    // Over the internet, video still waiting in QUIC's send buffer is late already:
                    // the next update waits for it to drain (a newer capture still replaces the
                    // one waiting), so a path that carries less costs frame rate, not latency.
                    state = shared.wake.wait_timeout(state, BACKLOG_RECHECK).unwrap().0;
                    continue;
                }
                if work_wait == Some(Duration::ZERO) {
                    let frame = if state.retry_at.is_none_or(|t| t <= now) { state.next.take() } else { None };
                    let touched = if frame.is_some() { std::mem::take(&mut state.touched) } else { 0 };
                    break Some((frame, touched, state.requests.take(now)));
                }
                if quiet_at.is_some_and(|at| at <= now) {
                    break None;
                }
                let quiet_wait = quiet_at.map(|at| at.saturating_duration_since(now));
                state = match [work_wait, quiet_wait].into_iter().flatten().min() {
                    Some(wait) => shared.wake.wait_timeout(state, wait).unwrap().0,
                    None => shared.wake.wait(state).unwrap(),
                };
            }
        };
        let Some((frame, touched, work)) = step else {
            // Before saying the screen is still, make sure nothing of the last frame went unsent.
            let outcome = pipeline.settle(video);
            match outcome.update {
                Some(sent) => {
                    note = Some(sent);
                    quiet_at = Some(Instant::now() + IDLE_NOTE_AFTER);
                }
                None => {
                    if let Some((update, mask)) = note.take() {
                        video.idle(update, mask);
                    }
                    quiet_at = None;
                }
            }
            shared.state.lock().unwrap().requests.done(&outcome);
            continue;
        };
        let tiled = frame.is_some();
        let mut outcome = pipeline.update(video, frame, touched, work);
        if let Some(sent) = outcome.update {
            note = Some(sent);
        }
        if outcome.update.is_some() || pipeline.has_unchecked() {
            quiet_at = Some(Instant::now() + IDLE_NOTE_AFTER);
        }
        let mut state = shared.state.lock().unwrap();
        match outcome.retry.take() {
            Some((frame, e)) => {
                let (at, report) = retry.failed(Instant::now());
                if let Some(failures) = report {
                    tracing::warn!(failures, "split frame into tiles: {e:#}; trying again");
                }
                state.retry_at = Some(at);
                state.touched |= touched;
                // Unless a newer frame came meanwhile: that one has everything this one had.
                state.next.get_or_insert(frame);
            }
            None if tiled => {
                retry.succeeded();
                state.retry_at = None;
            }
            None => {}
        }
        state.requests.done(&outcome);
    }
}

/// How the tiles and the full frame are kept right, update after update.
///
/// Every stream (each tile, and the full frame) keeps its own chain of references, and each
/// encoder/decoder pair sees exactly the same frames, so each decodes correctly; the viewer shows,
/// for every part of the picture, the newest image covering it. A tile's frame after a full frame
/// references that tile's older picture: bigger, still correct. The tiler's last content of every
/// tile is what the viewer shows there once the update is in, whichever stream carried it, so a
/// tile the viewer missed is sent again from it.
struct Pipeline {
    tiler: Tiler,
    encoders: TileEncoders,
    motion: Motion,
    /// The full-frame stream's next frame must be a keyframe: one of its frames never reached
    /// the viewer.
    full_keyframe: bool,
    /// When every tile was last compared (see [`Pipeline::compare`]).
    compared_all: Option<Instant>,
    /// The dirty rectangles missed a change lately: compare every tile until then.
    distrust: Option<Instant>,
    /// The newest captured frame, while only the tiles its dirty rectangles named (the mask) were
    /// compared: before the screen counts as still, every tile of it is (see [`Pipeline::settle`]).
    unchecked: Option<(CapturedFrame, u64)>,
    /// Compare every tile of the next frame, whatever the time (set by [`Pipeline::settle`]).
    check_all: bool,
    /// Submits in a row that each encoder (by tile, then the full frame's) refused.
    failures: Vec<u32>,
    /// Times in a row a tile's encoder couldn't be made again.
    rebuilds_failed: Vec<u32>,
    /// Tells the session the stream is broken (it starts it again, as when capture stops).
    on_broken: Arc<dyn Fn(String) + Send + Sync>,
}

/// A tile encoder that can't be made again this many times in a row breaks the stream.
const MAX_FAILED_REBUILDS: u32 = 3;

/// An encoder refusing this many frames in a row is broken: it is made again (the full frame's is
/// dropped: big changes go out as tiles).
const MAX_ENCODE_FAILURES: u32 = 3;

/// Where a submitted frame went.
#[derive(Clone, Copy)]
enum Route {
    Tile(usize),
    Full,
}

impl Pipeline {
    fn new(tiler: Tiler, encoders: TileEncoders, on_broken: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        let motion = Motion::new(encoders.bitrate_bps, encoders.tiles.clone(), encoders.video.wan.is_some(), Instant::now());
        let failures = vec![0; encoders.tiles.len() + 1];
        let rebuilds_failed = vec![0; encoders.tiles.len()];
        Self {
            tiler,
            encoders,
            motion,
            full_keyframe: false,
            compared_all: None,
            distrust: None,
            unchecked: None,
            check_all: false,
            failures,
            rebuilds_failed,
            on_broken,
        }
    }

    /// Whether the newest frame still waits for a comparison of every tile (see `settle`).
    fn has_unchecked(&self) -> bool {
        self.unchecked.is_some()
    }

    /// Compares every tile of the newest frame if only some of them were: ScreenCaptureKit sends
    /// nothing more while the screen is still, so a change its dirty rectangles missed would
    /// otherwise stay unsent. Sends what it finds as an update.
    fn settle(&mut self, video: &VideoOut) -> Outcome {
        match self.unchecked.take() {
            Some((frame, touched)) => {
                self.check_all = true;
                let mut outcome = self.update(video, Some(frame), touched, Work::default());
                self.check_all = false;
                outcome.retry = None; // a newer frame, or the next check, will do
                outcome
            }
            None => Outcome::default(),
        }
    }

    /// The tiles of `frame` that changed. Only those `touched` names are read (ScreenCaptureKit's
    /// dirty rectangles), except every [`FULL_COMPARE_INTERVAL`], when all are, in case it
    /// missed a change; if it did, its rectangles aren't used for [`DIRTY_RECTS_DISTRUST`].
    /// Returns the copies and the tiles it compared.
    fn compare(&mut self, frame: &CVPixelBuffer, touched: u64) -> Result<(Vec<TileCopy>, u64)> {
        let now = Instant::now();
        let trusted = self.distrust.is_none_or(|until| now >= until);
        let check = self.check_all || self.compared_all.is_none_or(|at| now >= at + FULL_COMPARE_INTERVAL);
        if trusted && !check && touched != u64::MAX {
            return Ok((self.tiler.changed_in(frame, touched)?, touched));
        }
        // Tiles with nothing to compare against yet count as changed whatever the rectangles say.
        let fresh = (0..self.encoders.tiles.len()).filter(|&i| self.tiler.last(i).is_none()).fold(0, |m, i| m | tile_bit(i));
        let copies = self.tiler.changed(frame)?;
        self.compared_all = Some(now);
        let missed = copies.iter().fold(0, |m, c| m | tile_bit(c.index)) & !touched & !fresh;
        if trusted && touched != u64::MAX && missed != 0 {
            self.missed(missed, now);
        }
        Ok((copies, u64::MAX))
    }

    /// The dirty rectangles missed changes to `tiles`: don't go by them for a while.
    fn missed(&mut self, tiles: u64, now: Instant) {
        tracing::warn!(missed = format!("{tiles:#x}"), "ScreenCaptureKit's dirty rectangles missed a change; comparing every tile for a while");
        self.distrust = Some(now + DIRTY_RECTS_DISTRUST);
    }

    /// Sends one update and waits until every frame of it is out (or isn't coming): the tiles of
    /// `frame` that changed, as tiles or, when they cover most of the picture, as one full frame
    /// (the captured frame itself); and `work`'s tiles, always as tiles. Each tile is encoded at
    /// most once, as a keyframe if asked for.
    ///
    /// `touched` are the tiles ScreenCaptureKit says changed since the last frame the tiler saw.
    fn update(&mut self, video: &VideoOut, frame: Option<CapturedFrame>, touched: u64, work: Work) -> Outcome {
        let mut outcome = Outcome::default();
        if work.drop_full && self.encoders.full.take().is_some() {
            video.no_full_frame.store(true, Ordering::Release);
            tracing::info!("the viewer can't decode full frames: big changes go out as tiles");
        }
        self.full_keyframe |= work.full_keyframe;
        let (frame, mut copies, compared, time_us) = match frame {
            Some(frame) => match self.compare(&frame.pixel_buffer, touched) {
                Ok((copies, compared)) => {
                    let time_us = frame.capture_time_us;
                    self.unchecked = (compared & self.encoders.mask() != self.encoders.mask())
                        .then(|| (CapturedFrame { pixel_buffer: frame.pixel_buffer.clone(), capture_time_us: time_us, dirty: None }, touched));
                    (Some(frame), copies, compared, time_us)
                }
                Err(e) => {
                    outcome.retry = Some((frame, e));
                    (None, Vec::new(), u64::MAX, clock::now_us())
                }
            },
            None => (None, Vec::new(), u64::MAX, clock::now_us()),
        };
        let now = Instant::now();
        let forced = (work.keyframes | work.resends) & self.encoders.mask();
        let mut changed = copies.iter().fold(0, |mask, c| mask | tile_bit(c.index));
        if let Some(frame) = &frame
            && self.encoders.full_frame(changed & !forced)
            && compared & self.encoders.mask() != self.encoders.mask()
        {
            // The full frame carries every tile's pixels, so every tile's last content must be
            // what it carries: compare the ones the dirty rectangles left out too.
            let rest = self.encoders.mask() & !compared;
            match self.tiler.changed_in(&frame.pixel_buffer, rest) {
                Ok(more) => {
                    let found = more.iter().fold(0, |mask, c| mask | tile_bit(c.index));
                    if found & rest != 0 {
                        self.missed(found & rest, now);
                    }
                    changed |= found;
                    copies.extend(more);
                    self.unchecked = None;
                }
                Err(e) => {
                    // They'll count as changed next time, whatever the hints say.
                    tracing::debug!("compare the tiles a full frame carries: {e:#}");
                    for i in (0..self.encoders.tiles.len()).filter(|&i| rest & tile_bit(i) != 0) {
                        self.tiler.invalidate(i);
                    }
                }
            }
        }
        self.motion.changed(changed, now);

        let mut jobs = Vec::new();
        // Changed tiles the full frame carries (forced ones still go as tiles).
        let mut covered = 0;
        match frame {
            // Decided on what the full frame would carry: forced tiles go as tiles anyway.
            Some(frame) if self.encoders.full_frame(changed & !forced) => {
                covered = changed & !forced;
                jobs.push((Route::Full, frame.pixel_buffer, self.full_keyframe));
            }
            // Back to ScreenCaptureKit: the tiles are copies.
            frame => drop(frame),
        }
        for copy in copies {
            if covered & tile_bit(copy.index) == 0 {
                jobs.push((Route::Tile(copy.index), copy.pixel_buffer, work.keyframes & tile_bit(copy.index) != 0));
            }
        }
        // Unchanged tiles asked for: what they show now. (A tile with no last content has never
        // been encoded, so its first frame will be a keyframe anyway.)
        for i in (0..self.encoders.tiles.len()).filter(|&i| forced & !changed & tile_bit(i) != 0) {
            if let Some(pixel_buffer) = self.tiler.last(i) {
                jobs.push((Route::Tile(i), pixel_buffer, work.keyframes & tile_bit(i) != 0));
            }
        }
        if jobs.is_empty() {
            return outcome;
        }

        // Over the internet, the stream's bitrate follows the connection.
        if let Some(bps) = video.take_ceiling() {
            self.motion.set_stream_bps(bps, now);
        }
        for (i, bitrate_bps) in self.motion.retarget(now) {
            if let Err(e) = self.encoders.encoders[i].set_bitrate(bitrate_bps) {
                tracing::debug!(tile = i, bitrate_bps, "set bitrate: {e:#}");
            }
        }
        let mask = jobs.iter().fold(0, |mask, (route, ..)| {
            mask | match route {
                Route::Tile(i) => tile_bit(*i),
                Route::Full => FULL_FRAME_BIT,
            }
        });
        let tag = video.start_update(mask);
        let mut submitted = Vec::with_capacity(jobs.len());
        for (route, pixel_buffer, keyframe) in jobs {
            let (encoder, track, failures) = match route {
                Route::Tile(i) => (&self.encoders.encoders[i], &self.encoders.tracks[i], i),
                Route::Full => {
                    let (encoder, track) = self.encoders.full.as_ref().expect("full-frame route");
                    (encoder, track, self.encoders.tiles.len())
                }
            };
            let keyframe = keyframe || track.repair.swap(false, Ordering::AcqRel);
            // Submitted or not, the encoder holds what it needs of the buffer.
            match encoder.encode(&pixel_buffer, time_us, keyframe, tag) {
                Ok(()) => submitted.push((route, keyframe)),
                Err(e) => {
                    self.failures[failures] += 1;
                    if self.failures[failures] == 1 {
                        tracing::warn!(full = matches!(route, Route::Full), "encode: {e:#}");
                    }
                    self.record(route, Fate::Lost, covered, keyframe, &mut outcome);
                }
            }
        }
        let deadline = Instant::now() + ENCODE_TIMEOUT;
        for (route, keyframe) in submitted {
            let (encoder, track) = match route {
                Route::Tile(i) => (&self.encoders.encoders[i], &self.encoders.tracks[i]),
                Route::Full => {
                    let (encoder, track) = self.encoders.full.as_ref().expect("full-frame route");
                    (encoder, track)
                }
            };
            let on_time = encoder.wait_idle(deadline.saturating_duration_since(Instant::now()));
            let mut fate = track.fate(tag);
            if !on_time {
                tracing::warn!(full = matches!(route, Route::Full), "encoder took over {ENCODE_TIMEOUT:?} for a frame");
                // It may still come out, and won't be sent then (its update is done): the
                // encoder's references would move past the viewer's, as for a frame that never
                // went out.
                if fate == Fate::Lost {
                    fate = Fate::Unsent;
                }
            }
            if let Fate::Sent { .. } = fate {
                let i = match route {
                    Route::Tile(i) => i,
                    Route::Full => self.encoders.tiles.len(),
                };
                self.failures[i] = 0;
            } else {
                tracing::debug!(tag, ?fate, full = matches!(route, Route::Full), "frame never reached the viewer");
            }
            self.record(route, fate, covered, keyframe, &mut outcome);
        }
        let sent = outcome.sent | if outcome.full_sent { FULL_FRAME_BIT } else { 0 };
        outcome.update = video.finish_update(tag, sent).map(|update| (update, sent));
        self.repair(&mut outcome);
        outcome
    }

    /// Makes encoders that keep refusing frames again (the full frame's goes: big changes then
    /// go out as tiles).
    fn repair(&mut self, outcome: &mut Outcome) {
        let tiles = self.encoders.tiles.len();
        for i in 0..=tiles {
            if self.failures[i] < MAX_ENCODE_FAILURES {
                continue;
            }
            self.failures[i] = 0;
            if i == tiles {
                tracing::warn!("the full-frame encoder keeps failing; big changes go out as tiles");
                self.encoders.full = None;
                continue;
            }
            // What the motion tracking last gave it, so a later retarget starts from the truth.
            let bitrate_bps = self.motion.bitrates[i];
            match self.encoders.rebuild(i, bitrate_bps) {
                Ok(()) => {
                    tracing::warn!(tile = i, "tile encoder kept failing: made it again");
                    self.rebuilds_failed[i] = 0;
                    // Its first frame is a keyframe anyway.
                    outcome.keyframes |= tile_bit(i);
                    outcome.resends &= !tile_bit(i);
                }
                Err(e) => {
                    self.rebuilds_failed[i] += 1;
                    if self.rebuilds_failed[i] == MAX_FAILED_REBUILDS {
                        // Start the stream over: it may get fewer tiles, or the media server is
                        // back by then.
                        (self.on_broken)(format!("tile {i}'s encoder can't be made again: {e:#}"));
                    } else if self.rebuilds_failed[i] == 1 {
                        tracing::warn!(tile = i, "make tile encoder again: {e:#}");
                    }
                }
            }
        }
    }

    /// Notes what became of a frame of this update; `covered` are the tiles a full frame carried,
    /// `asked_keyframe` whether this frame was to be a keyframe.
    fn record(&mut self, route: Route, fate: Fate, covered: u64, asked_keyframe: bool, outcome: &mut Outcome) {
        match (route, fate) {
            (Route::Tile(i), Fate::Sent { keyframe }) => {
                outcome.sent |= tile_bit(i);
                if keyframe {
                    outcome.sent_keyframes |= tile_bit(i);
                }
            }
            (Route::Tile(i), Fate::Unsent) => outcome.keyframes |= tile_bit(i),
            // A keyframe that never came out is still owed: the viewer's decoder waits for it.
            (Route::Tile(i), Fate::Lost) if asked_keyframe => outcome.keyframes |= tile_bit(i),
            (Route::Tile(i), Fate::Lost) => outcome.resends |= tile_bit(i),
            (Route::Full, Fate::Sent { keyframe }) => {
                outcome.full_sent = true;
                self.full_keyframe &= !keyframe;
            }
            (Route::Full, Fate::Unsent) => {
                self.full_keyframe = true;
                outcome.resends |= covered;
            }
            (Route::Full, Fate::Lost) => outcome.resends |= covered,
        }
    }
}

/// The stream a session sends. A display switch replaces it, always stopping the old one before
/// starting the new one (two encoders would mix their frames), and never on the async runtime.
#[derive(Default)]
struct StreamSlot(Mutex<Option<StreamSession>>);

impl StreamSlot {
    /// Bit `i` is tile `i`; bits beyond the stream's tiles are ignored.
    fn request_keyframes(&self, tiles: u64) {
        if let Some(stream) = self.0.lock().unwrap().as_ref() {
            stream.request_keyframes(tiles);
        }
    }

    fn capturer(&self) -> Option<Arc<Capturer>> {
        self.0.lock().unwrap().as_ref().and_then(|s| s.capturer())
    }

    /// The viewer can't decode full frames: this stream stops sending them (and the connection's
    /// next streams never start).
    fn no_full_frame(&self) {
        if let Some(stream) = self.0.lock().unwrap().as_ref() {
            stream.no_full_frame();
        }
    }

    fn codec(&self) -> Option<Codec> {
        self.0.lock().unwrap().as_ref().map(|s| s.codec())
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        // Stopping ScreenCaptureKit waits for a completion handler, which must not stall the
        // async runtime.
        if let Some(stream) = self.0.get_mut().unwrap().take() {
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => drop(rt.spawn_blocking(move || drop(stream))),
                Err(_) => drop(stream),
            }
        }
    }
}

/// What a session shows now.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Showing {
    /// As the viewer knows it: the main display, or its virtual display as it is.
    display: DisplayChoice,
    display_id: u32,
    /// The stream.
    width: u32,
    height: u32,
    fps: u32,
}

/// Starts a session's stream on a display. Cheap to clone: switches run in tasks of their own.
#[derive(Clone)]
struct Streamer {
    ctx: Arc<HostCtx>,
    conn: Connection,
    session_id: u64,
    /// The viewer's screen (from `Hello`): the main display is streamed at most this size.
    viewer_max: (u32, u32),
    viewer_fps: u32,
    video: bool,
    /// The viewer connected over the internet.
    internet: bool,
    stream: Arc<StreamSlot>,
    video_out: Arc<VideoOut>,
    input: Arc<InputShared>,
    cursor: CursorWish,
    /// Counts streams, so a stop reported by an old one is ignored.
    generation: Arc<AtomicU64>,
    events: mpsc::UnboundedSender<SessionEvt>,
}

impl Streamer {
    /// Shows this Mac's main display, scaled to fit the viewer's screen.
    async fn show_main(&self) -> Result<Showing> {
        let video = self.video;
        let display = tokio::task::spawn_blocking(move || -> Result<DisplayInfo> {
            if !video {
                return Ok(platform_mac::capture::main_display_bounds());
            }
            platform_mac::capture::display(main_display_now())
        })
        .await??;
        let (width, height) = fit_within(display.width, display.height, self.viewer_max.0, self.viewer_max.1);
        let fps = stream_fps(self.viewer_fps, self.internet);
        self.swap(display.id, width, height, fps).await?;
        Ok(Showing { display: DisplayChoice::Main, display_id: display.id, width, height, fps })
    }

    /// Shows the device's virtual display pixel for pixel: the viewer asked for its size.
    async fn show_virtual(&self, acquired: Acquired) -> Result<Showing> {
        let Acquired { display_id, spec, .. } = acquired;
        // As macOS draws it now (normally the size asked for), once ScreenCaptureKit lists it.
        let display = tokio::task::spawn_blocking(move || platform_mac::capture::display(display_id)).await??;
        let (width, height) = (display.width & !1, display.height & !1);
        if (width, height) != (spec.width, spec.height) {
            tracing::warn!(display = display_id, width, height, ?spec, "the virtual display isn't the size asked for");
        }
        let fps = stream_fps(spec.refresh_hz, self.internet);
        self.swap(display_id, width, height, fps).await?;
        Ok(Showing { display: DisplayChoice::Virtual(spec), display_id, width, height, fps })
    }

    /// Replaces the stream with one of `display_id`, and sends pointer input there.
    async fn swap(&self, display_id: u32, width: u32, height: u32, fps: u32) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.swap_now(display_id, width, height, fps)).await?
    }

    fn swap_now(&self, display_id: u32, width: u32, height: u32, fps: u32) -> Result<()> {
        // No cursor change applies to the old stream while it goes, or gets lost while the new one
        // starts: they wait for this lock.
        let _cursor = self.cursor.apply.lock().unwrap();
        let old = self.stream.0.lock().unwrap().take();
        drop(old);
        if self.video {
            let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
            let show_cursor = self.cursor.wanted.load(Ordering::Acquire);
            let capture = CaptureConfig { display_id, width, height, fps, show_cursor };
            // The bit budget stays at 60 fps worth, so bandwidth doesn't double at 120. Over the
            // internet, it starts at what the connection carries.
            let bitrate_bps = self.video_out.start_bitrate(bitrate_for(width, height, fps.min(60)));
            let encoder = EncoderConfig { width, height, fps, bitrate_bps, codec: Codec::Hevc };
            let events = self.events.clone();
            let stream = StreamSession::start(self.video_out.clone(), &capture, &encoder, move |why| {
                tracing::warn!(display = display_id, "capture stopped: {why}");
                let _ = events.send(SessionEvt::CaptureStopped(generation));
            })?;
            {
                // The viewer may have said it can't decode full frames while this one started:
                // the session stores that before it looks for the stream, so one of them sees it.
                let mut slot = self.stream.0.lock().unwrap();
                if self.video_out.no_full_frame.load(Ordering::Acquire) {
                    stream.no_full_frame();
                }
                *slot = Some(stream);
            }
            // The wish may have changed while the stream started.
            let wanted = self.cursor.wanted.load(Ordering::Acquire);
            if wanted != show_cursor
                && let Some(capturer) = self.stream.capturer()
                && let Err(e) = capturer.set_shows_cursor(wanted)
            {
                tracing::warn!("show cursor in video: {e:#}");
            }
        }
        self.input.set_display(display_id);
        Ok(())
    }

    /// Runs one display change; the result goes back to the session as [`SessionEvt::Switched`].
    async fn run(self, job: Job, before: Showing) -> Switched {
        let host = system::device_name();
        match job {
            Job::Request(request, DisplayChoice::Main, _) => {
                // Gone (unless another window of the device watches it) before the main display is
                // chosen: it may have been the main one. (A no-op if the session watches none.)
                self.ctx.displays.release(self.session_id).await;
                self.main(request, DisplayReason::NONE, String::new()).await
            }
            Job::Request(request, DisplayChoice::Virtual(spec), epoch) => match self.ctx.displays.acquire(self.session_id, spec, epoch).await {
                // Nothing changed: the stream goes on as it was.
                Err((reason, message)) => Switched { request, showing: before, reason, message, fatal: None, seq: 0 },
                Ok(acquired) => self.virtual_or_main(request, acquired, String::new(), &host).await,
            },
            Job::Main { reason, message, seq } => {
                // After whatever the display worker is still doing (giving the main role back).
                self.ctx.displays.release(self.session_id).await;
                Switched { seq, ..self.main(0, reason, message).await }
            }
            Job::Virtual { acquired, message } => self.virtual_or_main(0, acquired, message, &host).await,
            Job::Restart { delay } => {
                tokio::time::sleep(delay).await;
                match before.display {
                    DisplayChoice::Virtual(spec) if virtual_display::online_displays().contains(&before.display_id) => {
                        let acquired = Acquired { display_id: before.display_id, spec, seq: 0 };
                        self.virtual_or_main(0, acquired, String::new(), &host).await
                    }
                    DisplayChoice::Virtual(_) => {
                        self.ctx.displays.release(self.session_id).await;
                        self.main(0, DisplayReason::DISPLAY_GONE, format!("{host}'s virtual display went away.")).await
                    }
                    DisplayChoice::Main => self.main(0, DisplayReason::NONE, String::new()).await,
                }
            }
        }
    }

    async fn main(&self, request: u32, reason: DisplayReason, message: String) -> Switched {
        match retrying(|| self.show_main()).await {
            Ok(showing) => Switched { request, showing, reason, message, fatal: None, seq: 0 },
            Err(e) => Switched { request, showing: Showing::NONE, reason, message, fatal: Some(format!("{e:#}")), seq: 0 },
        }
    }

    /// Shows the virtual display, or, if that keeps failing, the main display again.
    async fn virtual_or_main(&self, request: u32, acquired: Acquired, message: String, host: &str) -> Switched {
        match retrying(|| self.show_virtual(acquired)).await {
            Ok(showing) => Switched { request, showing, reason: DisplayReason::NONE, message, fatal: None, seq: acquired.seq },
            Err(e) => {
                tracing::warn!("show the virtual display: {e:#}");
                self.ctx.displays.release(self.session_id).await;
                let message = format!("{host} couldn't show the virtual display ({e:#}), so it shows its own screen.");
                self.main(request, DisplayReason::FAILED, message).await
            }
        }
    }
}

/// Waits between tries to start a stream (see [`retrying`]).
const STREAM_RETRIES: [Duration; 3] = [Duration::from_millis(500), Duration::from_secs(2), Duration::from_secs(5)];

/// Starts a stream, trying again a few times if it fails. While the Mac's displays are being
/// rearranged (a virtual display made, resized, mirrored or removed), ScreenCaptureKit can stop
/// answering for several seconds; giving up then would cost the viewer its display (or its
/// connection) over a hiccup.
async fn retrying<F: std::future::Future<Output = Result<Showing>>>(mut start: impl FnMut() -> F) -> Result<Showing> {
    let mut waits = STREAM_RETRIES.iter();
    loop {
        match start().await {
            Ok(showing) => return Ok(showing),
            Err(e) => match waits.next() {
                Some(wait) => {
                    tracing::warn!("start the stream: {e:#}; trying again in {} ms", wait.as_millis());
                    tokio::time::sleep(*wait).await;
                }
                None => return Err(e),
            },
        }
    }
}

impl Showing {
    /// Nothing (a session that failed to show anything ends).
    const NONE: Self = Self { display: DisplayChoice::Main, display_id: 0, width: 0, height: 0, fps: 0 };
}

/// Whether a session watching "the main display" still does: its display shows its own picture,
/// and if it's a virtual display (another device's, which took the main role), it still has it.
fn still_main(display_id: u32) -> bool {
    virtual_display::is_active(display_id) && (!virtual_display::is_lankvm(display_id) || virtual_display::main_display() == display_id)
}

/// The main display, or if macOS hasn't settled on one after a change, the first display that
/// shows its own picture.
fn main_display_now() -> u32 {
    let main = virtual_display::main_display();
    if virtual_display::is_active(main) {
        return main;
    }
    virtual_display::online_displays().into_iter().find(|&d| virtual_display::is_active(d)).unwrap_or(main)
}

/// A display change for a session to make.
enum Job {
    /// The viewer asked for a display; the session decided it may at the displays' `epoch`.
    Request(u32, DisplayChoice, u64),
    /// Show the main display, saying why (the session's display went away or stopped showing);
    /// `seq` is the news that said so.
    Main { reason: DisplayReason, message: String, seq: u64 },
    /// Show the device's virtual display as it is now (another session of the device changed it).
    Virtual { acquired: Acquired, message: String },
    /// The capture stopped by itself: start it again after `delay`, on the same display if it's
    /// still there.
    Restart { delay: Duration },
}

/// What a display change did.
pub(crate) struct Switched {
    request: u32,
    showing: Showing,
    reason: DisplayReason,
    message: String,
    /// Nothing could be shown: the session ends with this.
    fatal: Option<String>,
    /// The display worker's news this follows (see `DisplayNotice`), 0 if none.
    seq: u64,
}

/// What a session shows, and switching it: one change at a time, in a task of its own (each takes
/// up to a couple of seconds), with the latest request waiting for its turn. The viewer hears
/// about every change, in order.
struct Screen {
    streamer: Streamer,
    out: mpsc::Sender<HostMsg>,
    showing: Showing,
    client_fp: Fingerprint,
    same_mac: bool,
    /// The display id the host's UI shows for this viewer.
    watching: Arc<AtomicU32>,
    /// The change in progress (aborted if the session ends).
    switching: Option<AbortOnDrop>,
    /// What the viewer asked for meanwhile (the latest wins), and news that came meanwhile.
    next_request: Option<NextRequest>,
    next_notice: Option<DisplayNotice>,
    /// The displays changed meanwhile: look again after.
    recheck: bool,
    available: (DisplayReason, String),
    /// Captures in a row that stopped by themselves (each restart waits longer).
    restarts: u32,
    /// The capture stopped during a change, which may not replace it: the stream generation then.
    stopped: Option<u64>,
    /// The display worker's newest news this session follows: older news is stale.
    seq_seen: u64,
}

enum NextRequest {
    Switch(u32, DisplayChoice),
    /// A request refused at once, answered in turn (after the change before it).
    Refuse(u32, DisplayReason, String),
}

impl Screen {
    fn new(streamer: Streamer, showing: Showing, out: mpsc::Sender<HostMsg>, client_fp: Fingerprint, same_mac: bool, watching: Arc<AtomicU32>) -> Self {
        let mut screen = Self {
            streamer,
            out,
            showing,
            client_fp,
            same_mac,
            watching,
            switching: None,
            next_request: None,
            next_notice: None,
            recheck: false,
            available: (DisplayReason::NONE, String::new()),
            restarts: 0,
            stopped: None,
            seq_seen: 0,
        };
        screen.available = screen.availability();
        screen
    }

    /// Tells the viewer what it watches now.
    fn announce(&self, request: u32, reason: DisplayReason, message: String) {
        let Showing { display, width, height, fps, .. } = self.showing;
        let (available, unavailable) = self.available.clone();
        let state = DisplayState { request, display, width, height, fps, reason, message, available, unavailable };
        if let Err(mpsc::error::TrySendError::Full(_)) = self.out.try_send(HostMsg::Display(state)) {
            tracing::warn!("viewer isn't reading its control stream; disconnecting");
            self.streamer.conn.close(4u32.into(), b"viewer not reading");
        }
    }

    /// Whether the viewer may ask for a virtual display, and if not why, naming this Mac.
    fn availability(&self) -> (DisplayReason, String) {
        let ctx = &self.streamer.ctx;
        let host = system::device_name();
        if !ctx.video {
            return (DisplayReason::NO_VIDEO, format!("{host} doesn't stream its screen, so it can't show a virtual display."));
        }
        if !virtual_display::supported() {
            return (DisplayReason::UNSUPPORTED, format!("{host}'s version of macOS can't make virtual displays."));
        }
        if !ctx.settings.lock().unwrap().allow_control {
            let message = format!("{host} lets paired Macs only view it. Virtual displays, like control, can be turned on under This Mac in LanKVM there.");
            return (DisplayReason::NOT_ALLOWED, message);
        }
        if !ctx.console_active.load(Ordering::Acquire) {
            return (DisplayReason::NOT_ALLOWED, format!("{host} switched to another user, so it can't show a virtual display now."));
        }
        if !ctx.trust.viewers.lock().unwrap().contains(&self.client_fp) {
            return (DisplayReason::NOT_ALLOWED, format!("{host} no longer trusts this Mac. Connect again to pair."));
        }
        if self.client_fp == ctx.trust.fingerprint {
            return (DisplayReason::NOT_ALLOWED, "This LanKVM is connected to itself.".into());
        }
        if self.same_mac && !ctx.allow_same_mac_control {
            let message = format!("That's this Mac ({host}): a virtual display can only go next to its screen, not replace it.");
            return (DisplayReason::SAME_MAC, message);
        }
        (DisplayReason::NONE, String::new())
    }

    /// Whether the viewer may have this virtual display, as the host will make it.
    fn check(&self, spec: VirtualDisplaySpec) -> Result<VirtualDisplaySpec, (DisplayReason, String)> {
        let (reason, message) = self.availability();
        if reason != DisplayReason::NONE && reason != DisplayReason::SAME_MAC {
            return Err((reason, message));
        }
        let spec = spec.validated().map_err(|m| (DisplayReason::INVALID, m))?;
        if spec.arrangement == Arrangement::EXTEND {
            return Ok(spec);
        }
        if reason == DisplayReason::SAME_MAC {
            return Err((reason, message));
        }
        // The main display is where the controlling device works: not another's to take.
        let ctx = &self.streamer.ctx;
        if let Some(name) = ctx.controller.lock().unwrap().as_ref().filter(|s| s.fingerprint != self.client_fp).map(|s| s.name.clone()) {
            let host = system::device_name();
            let message = format!("{name} is controlling {host}, so its main display isn't this Mac's to take. A display next to its screen is possible.");
            return Err((DisplayReason::IN_USE, message));
        }
        Ok(spec)
    }

    fn request(&mut self, request: u32, display: DisplayChoice) {
        let next = match display {
            DisplayChoice::Main => NextRequest::Switch(request, DisplayChoice::Main),
            DisplayChoice::Virtual(spec) => match self.check(spec) {
                Ok(spec) => NextRequest::Switch(request, DisplayChoice::Virtual(spec)),
                Err((reason, message)) => NextRequest::Refuse(request, reason, message),
            },
        };
        if self.switching.is_some() {
            self.next_request = Some(next);
        } else {
            self.next_request = Some(next);
            self.run_next();
        }
    }

    /// News from the virtual displays: follow it now, or after the change in progress.
    fn notice(&mut self, notice: DisplayNotice) {
        if self.switching.is_some() {
            match notice {
                DisplayNotice::Layout => self.recheck = true,
                notice => self.next_notice = Some(notice),
            }
            return;
        }
        if let Some(job) = self.follow(notice) {
            self.start(job);
        }
    }

    /// What to do about `notice`, if anything. News about a display the session doesn't watch, or
    /// older than what it follows (it came while a request of its own was answered), is stale.
    fn follow(&self, notice: DisplayNotice) -> Option<Job> {
        let watches = |id: u32| matches!(self.showing.display, DisplayChoice::Virtual(_)) && self.showing.display_id == id;
        match notice {
            DisplayNotice::Changed { display_id, spec, message, seq } if seq > self.seq_seen && watches(display_id) => {
                Some(Job::Virtual { acquired: Acquired { display_id, spec, seq }, message })
            }
            DisplayNotice::Removed { display_id, reason, message, seq } if seq > self.seq_seen && watches(display_id) => {
                Some(Job::Main { reason, message, seq })
            }
            DisplayNotice::Changed { .. } | DisplayNotice::Removed { .. } => None,
            DisplayNotice::Layout => self.check_layout(),
        }
    }

    /// After the Mac's displays changed: whether this session's display still shows, at its size.
    fn check_layout(&self) -> Option<Job> {
        if !self.streamer.video {
            return None;
        }
        let id = self.showing.display_id;
        match self.showing.display {
            // Watching the main display that now mirrors another (a viewer chose "only"), is gone, or
            // was another device's display that isn't main any more.
            DisplayChoice::Main if !still_main(id) => Some(Job::Main { reason: DisplayReason::NONE, message: String::new(), seq: 0 }),
            DisplayChoice::Main => None,
            DisplayChoice::Virtual(spec) => {
                if !virtual_display::online_displays().contains(&id) {
                    let message = format!("{}'s virtual display went away.", system::device_name());
                    return Some(Job::Main { reason: DisplayReason::DISPLAY_GONE, message, seq: 0 });
                }
                // Drawn at another size than streamed (macOS changed its mode for a moment).
                let size = virtual_display::pixel_size(id).map(|(w, h)| (w & !1, h & !1));
                (size.is_some() && size != Some((self.showing.width, self.showing.height)))
                    .then(|| Job::Virtual { acquired: Acquired { display_id: id, spec, seq: 0 }, message: String::new() })
            }
        }
    }

    fn start(&mut self, job: Job) {
        let (streamer, before) = (self.streamer.clone(), self.showing);
        let events = self.streamer.events.clone();
        self.switching = Some(AbortOnDrop(tokio::spawn(async move {
            let done = streamer.run(job, before).await;
            let _ = events.send(SessionEvt::Switched(Box::new(done)));
        })));
    }

    /// A change is done: tell the viewer, then go on with what waited.
    fn switched(&mut self, done: Switched) -> Result<()> {
        self.switching = None;
        if let Some(why) = done.fatal {
            bail!("no display to show: {why}");
        }
        if done.request != 0 {
            // The viewer chose: a capture that stops from now on starts a new count.
            self.restarts = 0;
        }
        self.seq_seen = self.seq_seen.max(done.seq);
        self.showing = done.showing;
        self.watching.store(self.showing.display_id, Ordering::Release);
        self.streamer.ctx.changed();
        self.announce(done.request, done.reason, done.message);
        self.run_next();
        Ok(())
    }

    fn run_next(&mut self) {
        // The capture stopped during a change that didn't replace it (a refused request): start
        // it again first.
        if let Some(generation) = self.stopped.take()
            && generation == self.streamer.generation.load(Ordering::Acquire)
        {
            self.capture_stopped(generation);
        }
        while self.switching.is_none() {
            if let Some(next) = self.next_request.take() {
                match next {
                    NextRequest::Refuse(request, reason, message) => self.announce(request, reason, message),
                    NextRequest::Switch(request, display) if display == self.showing.display && self.still_shown() => {
                        // Already showing that (a viewer re-applying its choice): nothing to change.
                        self.announce(request, DisplayReason::NONE, String::new());
                    }
                    NextRequest::Switch(request, display) => {
                        // Allowed when asked, but maybe not any more (this Mac's user took its
                        // screen back meanwhile): decide again, now. The display worker refuses it
                        // if that happens while it waits there.
                        let epoch = self.streamer.ctx.displays.epoch();
                        if let DisplayChoice::Virtual(spec) = display
                            && let Err((reason, message)) = self.check(spec)
                        {
                            self.announce(request, reason, message);
                            continue;
                        }
                        // The viewer's choice decides; news about the display it leaves no longer matters.
                        self.next_notice = None;
                        self.recheck = true;
                        self.start(Job::Request(request, display, epoch));
                    }
                }
            } else if let Some(notice) = self.next_notice.take() {
                if let Some(job) = self.follow(notice) {
                    self.start(job);
                }
            } else if std::mem::take(&mut self.recheck) {
                if let Some(job) = self.check_layout() {
                    self.start(job);
                }
            } else {
                return;
            }
        }
    }

    /// Whether the display shown is still there to show (as the main display, if that's what the
    /// viewer watches).
    fn still_shown(&self) -> bool {
        let id = self.showing.display_id;
        !self.streamer.video
            || match self.showing.display {
                DisplayChoice::Main => still_main(id),
                DisplayChoice::Virtual(_) => virtual_display::is_active(id),
            }
    }

    fn availability_changed(&mut self) {
        let available = self.availability();
        if available != self.available {
            self.available = available;
            self.announce(0, DisplayReason::NONE, String::new());
        }
    }

    /// ScreenCaptureKit stopped the capture by itself (the display changed or went away under it):
    /// start it again, waiting longer each time in a row so a display that keeps failing can't
    /// keep the Mac busy.
    fn capture_stopped(&mut self, generation: u64) {
        if generation != self.streamer.generation.load(Ordering::Acquire) {
            return;
        }
        if self.switching.is_some() {
            // The change in progress may replace the stream; if it doesn't, it restarts after.
            self.stopped = Some(generation);
            return;
        }
        let delay = (Duration::from_millis(250) * 2u32.saturating_pow(self.restarts)).min(MAX_CAPTURE_RESTART_DELAY);
        self.restarts += 1;
        self.start(Job::Restart { delay });
    }
}

/// Counts the session among the virtual displays' viewers while it lives. When it ends, a
/// virtual display only it watched goes: at once if the connection was closed, or a while later
/// if it was lost (the viewer may come back for it).
struct Lease {
    ctx: Arc<HostCtx>,
    session_id: u64,
    conn: Connection,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let lost = matches!(self.conn.close_reason(), Some(ConnectionError::TimedOut | ConnectionError::Reset));
        self.ctx.displays.leave(self.session_id, lost);
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
    fn fps_follows_the_viewer() {
        assert_eq!(choose_fps(120), 120);
        assert_eq!(choose_fps(60), 60);
        assert_eq!(choose_fps(1000), 120);
        assert_eq!(choose_fps(0), 15);
        // Over the internet, at most 60 (the main display and virtual ones alike).
        assert_eq!((stream_fps(120, false), stream_fps(120, true)), (120, 60));
        assert_eq!(stream_fps(30, true), 30);
    }

    #[test]
    fn tiles_share_the_bitrate_by_area() {
        let (w, h) = (6144, 2560);
        let total = bitrate_for(w, h, 60);
        let tiles = tile_layout(w, h, None);
        let shares = tile_bitrates(total, &tiles, 0, MIN_TILE_BITRATE);
        assert!(shares.iter().all(|&s| s == total / 16), "{shares:?}");
        let sum: u64 = shares.iter().map(|&s| u64::from(s)).sum();
        assert!(sum <= u64::from(total) && sum + 16 >= u64::from(total));
        // A small stream's tiles still get a usable bitrate each.
        let tiles = tile_layout(640, 480, Some((8, 7)));
        assert!(tile_bitrates(bitrate_for(640, 480, 60), &tiles, 0, MIN_TILE_BITRATE).iter().all(|&s| s == MIN_TILE_BITRATE));
        let whole = TileRect { index: 0, x: 0, y: 0, width: w, height: h };
        assert_eq!(tile_bitrates(total, &[whole], 0, MIN_TILE_BITRATE), [total]);
        assert_eq!(tile_bitrates(total, &[whole], 1, MIN_TILE_BITRATE), [total]);
    }

    #[test]
    fn moving_tiles_share_the_whole_budget() {
        let (w, h) = (6144, 2560);
        let total = bitrate_for(w, h, 60);
        let tiles = tile_layout(w, h, None);
        // One tile moves: it may use the whole budget, the still ones keep their share.
        let one = tile_bitrates(total, &tiles, 1 << 5, MIN_TILE_BITRATE);
        assert_eq!(one[5], total);
        assert!(one.iter().enumerate().all(|(i, &s)| i == 5 || s == total / 16), "{one:?}");
        // Four move: a quarter each.
        let four = tile_bitrates(total, &tiles, 0b1111 << 4, MIN_TILE_BITRATE);
        assert!((4..8).all(|i| four[i] == total / 4) && four[0] == total / 16, "{four:?}");
        // All move: by area, as when all are still.
        assert_eq!(tile_bitrates(total, &tiles, u64::MAX >> 48, MIN_TILE_BITRATE), tile_bitrates(total, &tiles, 0, MIN_TILE_BITRATE));
        // Uneven tiles: by area among the moving ones.
        let tiles = tile_layout(1000, 640, Some((2, 1)));
        let (a, b) = (u64::from(tiles[0].width) * 640, u64::from(tiles[1].width) * 640);
        let both = tile_bitrates(40_000_000, &tiles, 0b11, MIN_TILE_BITRATE);
        assert_eq!(both, [(40_000_000 * a / (a + b)) as u32, (40_000_000 * b / (a + b)) as u32]);
    }

    #[test]
    fn bitrates_follow_the_motion_now_and_then() {
        let (w, h) = (6144, 2560);
        let total = bitrate_for(w, h, 60);
        let tiles = tile_layout(w, h, None);
        let t0 = Instant::now();
        let mut motion = Motion::new(total, tiles.clone(), false, t0);
        assert!(motion.bitrates.iter().all(|&s| s == total / 16));
        motion.changed(1 << 3, t0);
        // Not before the interval is up.
        assert!(motion.retarget(t0 + RETARGET_INTERVAL / 2).is_empty());
        let t1 = t0 + RETARGET_INTERVAL;
        assert_eq!(motion.retarget(t1), [(3, total)]);
        // Then again only after another interval, and only real changes.
        motion.changed(1 << 3, t1);
        assert!(motion.retarget(t1 + RETARGET_INTERVAL / 2).is_empty());
        assert!(motion.retarget(t1 + RETARGET_INTERVAL).is_empty());
        // A second tile moving too: each gets half (a change above 25% for both).
        let t2 = t1 + RETARGET_INTERVAL * 2;
        motion.changed(1 << 3 | 1 << 9, t2);
        assert_eq!(motion.retarget(t2), [(3, total / 2), (9, total / 2)]);
        // Tile 9 goes on, tile 3 stops: it falls back to its share once its second is up.
        let t3 = t2 + MOTION_WINDOW;
        motion.changed(1 << 9, t3 - RETARGET_INTERVAL);
        assert_eq!(motion.retarget(t3), [(3, total / 16), (9, total)]);
        // Below the threshold: left alone.
        let mut motion = Motion::new(total, tile_layout(1000, 640, Some((2, 1))), false, t0);
        motion.changed(0b11, t0);
        let before = motion.bitrates.clone();
        assert!(motion.retarget(t1).is_empty(), "{before:?}");
    }

    /// Over the internet the stream's bitrate follows the connection: a new one reaches every
    /// tile at the next update, however small the change.
    #[test]
    fn a_new_stream_bitrate_reaches_every_tile_at_once() {
        let (w, h) = (6144, 2560);
        let total = bitrate_for(w, h, 60);
        let tiles = tile_layout(w, h, None);
        let t0 = Instant::now();
        let mut motion = Motion::new(total, tiles.clone(), false, t0);
        // 10% less, before the interval is up.
        let cut = total / 10 * 9;
        motion.set_stream_bps(cut, t0);
        let expected: Vec<_> = tile_bitrates(cut, &tiles, 0, MIN_TILE_BITRATE).into_iter().enumerate().collect();
        assert_eq!(motion.retarget(t0), expected);
        // Then by the threshold again.
        motion.set_stream_bps(cut, t0);
        assert!(motion.retarget(t0 + RETARGET_INTERVAL).is_empty());
        motion.set_stream_bps(cut / 10 * 9, t0 + RETARGET_INTERVAL);
        assert_eq!(motion.retarget(t0 + RETARGET_INTERVAL).len(), tiles.len());
    }

    #[test]
    fn over_the_internet_keyframes_are_a_round_trip_apart() {
        let t0 = Instant::now();
        let gap = Duration::from_millis(120);
        let mut k = Requests { gap: Some(gap), ..Requests::default() };
        k.ask(0b01, 0b11, false);
        assert_eq!(k.take(t0).keyframes, 0b01);
        k.ask(0b10, 0b11, false);
        assert_eq!(k.wait(t0 + KEYFRAME_MIN_INTERVAL), Some(gap - KEYFRAME_MIN_INTERVAL));
        assert_eq!(k.take(t0 + KEYFRAME_MIN_INTERVAL), Work::default());
        assert_eq!(k.take(t0 + gap).keyframes, 0b10);
    }

    #[test]
    fn tile_grid_from_the_environment() {
        assert_eq!(parse_grid("2x8"), Some((2, 8)));
        assert_eq!(parse_grid(" 1X1 "), Some((1, 1)));
        for bad in ["", "2", "x8", "2x", "0x4", "4x0", "-1x2", "2x8x1", "axb"] {
            assert_eq!(parse_grid(bad), None, "{bad:?}");
        }
    }

    /// Updates take numbers only when something of them goes out, so a gap the viewer sees is a
    /// loss; and what the next update says the one before sent is what really went out.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_updates_that_send_something_are_numbered() {
        let (host_conn, _conn) = loopback("numbers").await;
        let video = VideoOut::new(host_conn, false);
        let first = video.start_update(0b11);
        assert_eq!(video.finish_update(first, 0), None, "nothing went out: no number");
        let second = video.start_update(0b110);
        assert_ne!(second, first);
        assert_eq!(video.next_update.load(Ordering::Relaxed), 0);
        // Its first frame takes number 0; it ends with only tile 1 out.
        {
            let mut slots = video.slots.lock().unwrap();
            let slot = &mut slots[second as usize % UPDATE_MASKS];
            slot.update = Some(video.next_update.fetch_add(1, Ordering::Relaxed));
        }
        assert_eq!(video.finish_update(second, 0b010), Some(0));
        assert_eq!(video.slots.lock().unwrap()[second as usize % UPDATE_MASKS].mask, 0b010);
    }

    #[test]
    fn frames_that_never_went_out_are_told_apart() {
        let track = Track::default();
        assert_eq!(track.fate(0), Fate::Lost);
        // Put out but not sent: the encoder's references moved on.
        track.emitted.store(1, Ordering::Release);
        assert_eq!(track.fate(1), Fate::Unsent);
        track.sent.store(1, Ordering::Release);
        assert_eq!(track.fate(1), Fate::Sent { keyframe: false });
        track.emitted.store(2, Ordering::Release);
        track.sent.store(2, Ordering::Release);
        track.sent_keyframe.store(2, Ordering::Release);
        assert_eq!(track.fate(2), Fate::Sent { keyframe: true });
        // Nothing came out for update 3 (dropped, failed or late).
        assert_eq!(track.fate(3), Fate::Lost);
    }

    #[test]
    fn tiler_failures_back_off_and_warn_now_and_then() {
        let t0 = Instant::now();
        let mut retry = TilerRetry::default();
        assert_eq!(retry.failed(t0), (t0 + TILER_RETRY, Some(1)));
        assert_eq!(retry.failed(t0), (t0 + TILER_RETRY * 2, None));
        let mut at = t0;
        for _ in 0..20 {
            at = retry.failed(t0).0;
        }
        assert_eq!(at, t0 + MAX_TILER_RETRY);
        // The next warning counts the failures since the last one.
        assert_eq!(retry.failed(t0 + TILER_WARN_INTERVAL).1, Some(22));
        retry.succeeded();
        assert_eq!(retry.failed(t0 + TILER_WARN_INTERVAL).0, t0 + TILER_WARN_INTERVAL + TILER_RETRY);
    }

    /// A full-range NV12 frame, every byte from `value(plane, x_byte, y)`.
    fn nv12(w: usize, h: usize, value: impl Fn(usize, usize, usize) -> u8) -> CapturedFrame {
        use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType};
        use objc2_core_video::{
            CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
            CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
            kCVPixelBufferIOSurfacePropertiesKey,
        };
        let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
        let attrs: CFRetained<CFDictionary<CFString, CFType>> =
            CFDictionary::from_slices(&[unsafe { kCVPixelBufferIOSurfacePropertiesKey }], &[empty.as_ref()]);
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        let status = unsafe {
            CVPixelBufferCreate(None, w, h, u32::from_be_bytes(*b"420f"), Some(attrs.as_opaque()), std::ptr::NonNull::from(&mut raw))
        };
        assert_eq!(status, 0);
        let pixel_buffer = unsafe { CFRetained::from_raw(std::ptr::NonNull::new(raw).unwrap()) };
        unsafe {
            CVPixelBufferLockBaseAddress(&pixel_buffer, CVPixelBufferLockFlags::empty());
            for plane in 0..2 {
                let base = CVPixelBufferGetBaseAddressOfPlane(&pixel_buffer, plane) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&pixel_buffer, plane);
                for y in 0..CVPixelBufferGetHeightOfPlane(&pixel_buffer, plane) {
                    let row = std::slice::from_raw_parts_mut(base.add(y * stride), stride);
                    for (x, px) in row.iter_mut().enumerate() {
                        *px = value(plane, x, y);
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(&pixel_buffer, CVPixelBufferLockFlags::empty());
        }
        CapturedFrame { pixel_buffer, capture_time_us: clock::now_us(), dirty: None }
    }

    /// Receives tile frames until `count` came in (and checks nothing else arrives).
    async fn receive(conn: &Connection, reassemblers: &mut [transport::video::Reassembler], count: usize) -> Vec<VideoFrame> {
        let mut frames = Vec::new();
        while frames.len() < count {
            let datagram = tokio::time::timeout(Duration::from_secs(3), conn.read_datagram()).await.expect("tile frame in time").unwrap();
            let tile = transport::video::tile_of(&datagram).expect("video datagram");
            if let Some(assembled) = reassemblers[usize::from(tile)].push(&datagram) {
                let frame: VideoFrame = protocol::decode(&assembled.data).unwrap();
                assert_eq!(frame.tile.index, tile);
                frames.push(frame);
            }
        }
        frames.sort_by_key(|f| f.tile.index);
        frames
    }

    /// A host connection and the viewer's end of it, over loopback QUIC.
    async fn loopback(name: &str) -> (Connection, Connection) {
        use transport::endpoint::make_endpoint;
        use transport::identity::DeviceIdentity;
        let identity = |side: &str| {
            let dir = std::env::temp_dir().join(format!("lankvm-test-{name}-{side}-{}", std::process::id()));
            DeviceIdentity::load_or_create(&dir).unwrap()
        };
        let (host_id, client_id) = (identity("host"), identity("client"));
        let host = make_endpoint("127.0.0.1:0".parse().unwrap(), &host_id).unwrap();
        let client = make_endpoint("127.0.0.1:0".parse().unwrap(), &client_id).unwrap();
        let host_addr = host.local_addr().unwrap();
        let accept = tokio::spawn(async move { host.accept().await.unwrap().await.unwrap() });
        let conn = client.connect(host_addr, "lankvm").unwrap().await.unwrap();
        (accept.await.unwrap(), conn)
    }

    /// The host's video path for a `w`×`h` stream in `grid` tiles, without screen capture.
    /// Tests that make hardware encoders take turns: the media engine runs 32 sessions at once.
    static ENCODERS: Mutex<()> = Mutex::new(());

    fn encoders_turn() -> std::sync::MutexGuard<'static, ()> {
        ENCODERS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn pipeline(video: &Arc<VideoOut>, w: u32, h: u32, grid: (u32, u32), full_frame_at: f64) -> Pipeline {
        let cfg = EncoderConfig { width: w, height: h, fps: 60, bitrate_bps: bitrate_for(w, h, 60), codec: Codec::Hevc };
        let encoders = TileEncoders::new(video, &cfg, tile_layout(w, h, Some(grid)), full_frame_at).unwrap();
        let tiler = Tiler::new(w, h, &encoders.tiles).unwrap();
        Pipeline::new(tiler, encoders, Arc::new(|why| panic!("stream broke: {why}")))
    }

    fn summary(frames: &[VideoFrame]) -> Vec<(u8, u32, u64, bool)> {
        frames.iter().map(|f| (f.tile.index, f.update, f.update_mask, f.keyframe)).collect()
    }

    /// The host's video path without screen capture: synthetic frames through the tiler and one
    /// encoder per tile, over real QUIC datagrams.
    #[tokio::test(flavor = "multi_thread")]
    async fn sends_only_changed_and_requested_tiles() {
        let _encoders = encoders_turn();
        let (host_conn, conn) = loopback("tiles").await;
        let (w, h) = (1280u32, 720u32);
        let video = Arc::new(VideoOut::new(host_conn.clone(), false));
        // No full frames here (see `big_changes_go_out_as_one_full_frame`).
        let mut pipeline = pipeline(&video, w, h, (2, 2), 2.0);
        assert_eq!((pipeline.encoders.tiles.len(), pipeline.encoders.mask()), (4, 0b1111));
        let tiles = pipeline.encoders.tiles.clone();
        let mut reassemblers: Vec<_> = (0..MAX_TILES).map(|_| transport::video::Reassembler::new()).collect();
        let pattern = |p: usize, x: usize, y: usize| (x * 7 + y * 3 + p * 50) as u8;
        let mut update = |frame: Option<CapturedFrame>, work: Work| tokio::task::block_in_place(|| pipeline.update(&video, frame, u64::MAX, work));
        let tiles_only = |keyframes, resends| Work { keyframes, resends, full_keyframe: false, drop_full: false };

        // First frame: every tile, as keyframes, one update.
        let out = update(Some(nv12(w as usize, h as usize, pattern)), Work::default());
        assert_eq!((out.sent, out.sent_keyframes, out.resends, out.keyframes), (0b1111, 0b1111, 0, 0));
        let frames = receive(&conn, &mut reassemblers, 4).await;
        for (i, f) in frames.iter().enumerate() {
            assert_eq!(f.tile, tiles[i]);
            assert_eq!((f.width, f.height, f.update, f.update_mask), (w, h, 0, 0b1111));
            assert!(f.keyframe && !f.param_sets.is_empty(), "tile {i}");
        }

        // The same pixels: nothing is sent, and no update number is used.
        assert_eq!(update(Some(nv12(w as usize, h as usize, pattern)), Work::default()).sent, 0);
        assert_eq!(video.next_update.load(Ordering::Relaxed), 1);

        // One pixel in tile 3: only it, as a delta frame.
        let (px, py) = (w as usize - 5, h as usize - 5);
        let changed = move |p: usize, x: usize, y: usize| pattern(p, x, y) ^ u8::from(p == 0 && x == px && y == py) * 0x80;
        assert_eq!(update(Some(nv12(w as usize, h as usize, changed)), Work::default()).sent, 0b1000);
        let frames = receive(&conn, &mut reassemblers, 1).await;
        assert_eq!(summary(&frames), [(3, 1, 0b1000, false)]);

        // Keyframes asked for while the screen is still: re-encoded from the tiles' last content.
        let out = update(None, tiles_only(0b0110, 0));
        assert_eq!((out.sent, out.sent_keyframes), (0b0110, 0b0110));
        let frames = receive(&conn, &mut reassemblers, 2).await;
        assert_eq!(summary(&frames), [(1, 2, 0b110, true), (2, 2, 0b110, true)]);

        // Resends: the last content again, as ordinary frames.
        assert_eq!(update(None, tiles_only(0, 0b1001)).sent_keyframes, 0);
        let frames = receive(&conn, &mut reassemblers, 2).await;
        assert_eq!(summary(&frames), [(0, 3, 0b1001, false), (3, 3, 0b1001, false)]);

        // A tile that changed and was asked for goes once, as a keyframe; another one changed too.
        let both = move |p: usize, x: usize, y: usize| changed(p, x, y) ^ u8::from(p == 1 && (x == 0 || x == w as usize - 1) && y == 0);
        update(Some(nv12(w as usize, h as usize, both)), tiles_only(0b0001, 0));
        let frames = receive(&conn, &mut reassemblers, 2).await;
        assert_eq!(summary(&frames), [(0, 4, 0b11, true), (1, 4, 0b11, false)]);

        // Nothing else was sent.
        assert!(tokio::time::timeout(Duration::from_millis(100), conn.read_datagram()).await.is_err());

        // Encoded but never sent: its stream needs a keyframe. (Its content was taken as sent, so
        // the same pixels don't count as a change.)
        host_conn.close(0u32.into(), b"done");
        let out = update(None, tiles_only(0, 0b1000));
        assert_eq!((out.sent, out.keyframes, out.resends), (0, 0b1000, 0));
        assert!(update(Some(nv12(w as usize, h as usize, both)), Work::default()).keyframes == 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn big_changes_go_out_as_one_full_frame() {
        let _encoders = encoders_turn();
        let (host_conn, conn) = loopback("full").await;
        let (w, h) = (1280u32, 720u32);
        let video = Arc::new(VideoOut::new(host_conn.clone(), false));
        let mut pipeline = pipeline(&video, w, h, (2, 2), 0.75);
        assert!(pipeline.encoders.full.is_some());
        let mut reassemblers: Vec<_> = (0..MAX_TILES).map(|_| transport::video::Reassembler::new()).collect();
        let full = TileRect { index: FULL_FRAME_TILE, x: 0, y: 0, width: w, height: h };
        let tiles = pipeline.encoders.tiles.clone();
        // Tile areas: 640×384 at the top, 640×336 below.
        assert_eq!((tiles[0].height, tiles[3].height), (384, 336));
        let mut update = |frame: Option<CapturedFrame>, work: Work| tokio::task::block_in_place(|| pipeline.update(&video, frame, u64::MAX, work));
        // A picture whose tile `i` is the base picture plus `seeds[i]`.
        let frame = |seeds: [u8; 4]| {
            let tiles = tiles.clone();
            nv12(w as usize, h as usize, move |p, x, y| {
                let (px, py) = if p == 0 { (x, y) } else { (x / 2 * 2, y * 2) };
                let inside = |t: &TileRect| (t.x as usize..(t.x + t.width) as usize).contains(&px) && (t.y as usize..(t.y + t.height) as usize).contains(&py);
                let seed = tiles.iter().position(inside).map_or(0, |i| seeds[i]);
                ((x * 7 + y * 3 + p * 50) as u8).wrapping_add(seed)
            })
        };

        // The first frame changes everything: one full frame, a keyframe.
        let out = update(Some(frame([0, 0, 0, 0])), Work::default());
        assert_eq!((out.sent, out.resends, out.keyframes), (0, 0, 0));
        let frames = receive(&conn, &mut reassemblers, 1).await;
        assert_eq!(summary(&frames), [(FULL_FRAME_TILE, 0, FULL_FRAME_BIT, true)]);
        assert_eq!((frames[0].tile, frames[0].width, frames[0].height), (full, w, h));

        // A small change goes as tiles (tile 3's first frame of its own: a keyframe).
        update(Some(frame([0, 0, 0, 1])), Work::default());
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(3, 1, 0b1000, true)]);
        // Three tiles, 77% of the picture: a full frame again, a delta frame now.
        update(Some(frame([2, 2, 2, 1])), Work::default());
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(FULL_FRAME_TILE, 2, FULL_FRAME_BIT, false)]);
        // Smaller changes after it: tiles, delta frames once a tile's stream has started.
        update(Some(frame([3, 3, 2, 1])), Work::default());
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 2).await), [(0, 3, 0b11, true), (1, 3, 0b11, true)]);
        update(Some(frame([4, 3, 2, 4])), Work::default());
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 2).await), [(0, 4, 0b1001, false), (3, 4, 0b1001, false)]);

        // The viewer lost a full frame: every tile again (ordinary frames, from what they show
        // now), and the next full frame is a keyframe.
        let mut requests = Requests::default();
        requests.ask(FULL_FRAME_BIT, 0b1111, true);
        let work = requests.take(Instant::now());
        assert_eq!(work, Work { keyframes: 0, resends: 0b1111, full_keyframe: true, drop_full: false });
        let out = update(None, work);
        requests.done(&out);
        assert_eq!((out.sent, requests.resends), (0b1111, 0));
        let frames = receive(&conn, &mut reassemblers, 4).await;
        // Tile 2 never had a frame of its own: its first is a keyframe anyway.
        assert_eq!(summary(&frames), [(0, 5, 0b1111, false), (1, 5, 0b1111, false), (2, 5, 0b1111, true), (3, 5, 0b1111, false)]);
        update(Some(frame([6, 6, 6, 6])), Work::default());
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(FULL_FRAME_TILE, 6, FULL_FRAME_BIT, true)]);
        update(Some(frame([7, 7, 7, 7])), Work::default());
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(FULL_FRAME_TILE, 7, FULL_FRAME_BIT, false)]);

        // Tiles asked for never go in the full frame: they go beside it, in the same update.
        update(Some(frame([8, 8, 8, 8])), Work { keyframes: 0b1000, resends: 0, full_keyframe: false, drop_full: false });
        let mask = FULL_FRAME_BIT | 0b1000;
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 2).await), [(3, 8, mask, true), (FULL_FRAME_TILE, 8, mask, false)]);
        // Whether a full frame is worth it depends on what it would carry beside the tiles asked
        // for: here only half the picture, so it all goes as tiles.
        update(Some(frame([9, 9, 9, 9])), Work { keyframes: 0b0010, resends: 0b0100, full_keyframe: false, drop_full: false });
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 4).await), [(0, 9, 0b1111, false), (1, 9, 0b1111, true), (2, 9, 0b1111, false), (3, 9, 0b1111, false)]);
        assert!(tokio::time::timeout(Duration::from_millis(100), conn.read_datagram()).await.is_err());

        // A full frame that never went out: its stream needs a keyframe, and the tiles it carried
        // go again.
        host_conn.close(0u32.into(), b"done");
        let out = update(Some(frame([10, 10, 10, 9])), Work::default());
        assert_eq!((out.sent, out.resends, out.keyframes), (0, 0b0111, 0));
        assert!(pipeline.full_keyframe);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_tile_has_no_full_frame_stream() {
        let _encoders = encoders_turn();
        let (host_conn, conn) = loopback("one").await;
        let (w, h) = (640u32, 360u32);
        let video = Arc::new(VideoOut::new(host_conn.clone(), false));
        let mut pipeline = pipeline(&video, w, h, (1, 1), 0.0);
        assert!(pipeline.encoders.full.is_none());
        let mut reassemblers: Vec<_> = (0..MAX_TILES).map(|_| transport::video::Reassembler::new()).collect();
        let out = tokio::task::block_in_place(|| pipeline.update(&video, Some(nv12(w as usize, h as usize, |_, x, y| (x ^ y) as u8)), u64::MAX, Work::default()));
        assert_eq!(out.sent, 1);
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(0, 0, 1, true)]);
        // The full-frame stream's bit means nothing here.
        let mut requests = Requests::default();
        requests.ask(FULL_FRAME_BIT, 1, false);
        assert_eq!(requests.wait(Instant::now()), None);
    }

    /// Over the internet: tiles only, streams start within the ceiling, and a new ceiling reaches
    /// the encoders with the next update. On the local network none of it applies.
    #[tokio::test(flavor = "multi_thread")]
    async fn over_the_internet_tiles_follow_the_ceiling() {
        let _encoders = encoders_turn();
        let (host_conn, _conn) = loopback("wan").await;
        let (w, h) = (640u32, 384u32);
        let video = Arc::new(VideoOut::new(host_conn.clone(), true));
        let mut pipeline = pipeline(&video, w, h, (2, 1), 0.0);
        assert!(pipeline.encoders.full.is_none() && video.no_full_frame.load(Ordering::Relaxed));
        let wan = video.wan.as_ref().unwrap();
        assert_eq!(video.start_bitrate(50_000_000), rate::START_BPS);
        assert_eq!(video.start_bitrate(9_000_000), 9_000_000);
        assert_eq!(video.take_ceiling(), None, "nothing changed yet");
        // The ceiling never goes above the running stream's LAN bitrate.
        wan.ceiling_bps.store(10_000_000, Ordering::Release);
        wan.changed.store(true, Ordering::Release);
        assert_eq!(video.take_ceiling(), Some(9_000_000));
        assert_eq!(video.take_ceiling(), None, "once");
        wan.ceiling_bps.store(3_000_000, Ordering::Release);
        wan.changed.store(true, Ordering::Release);
        let out = tokio::task::block_in_place(|| pipeline.update(&video, Some(nv12(w as usize, h as usize, |_, x, y| (x ^ y) as u8)), u64::MAX, Work::default()));
        assert_eq!(out.sent, 0b11);
        assert_eq!(pipeline.motion.stream_bps, 3_000_000);
        assert!(!video.backlogged(), "nothing waits");
        assert_eq!(video.keyframe_gap(), Some(Duration::from_millis(50)), "a loopback round trip is far shorter");

        let lan = VideoOut::new(host_conn, false);
        assert!(!lan.no_full_frame.load(Ordering::Relaxed));
        assert_eq!(lan.start_bitrate(50_000_000), 50_000_000);
        assert_eq!((lan.take_ceiling(), lan.backlogged(), lan.keyframe_gap()), (None, false, None));
    }

    /// The encode thread as a stream runs it: newest frame wins, a frame the tiler failed on is
    /// tried again (or a newer one instead), requests are served without a capture.
    #[tokio::test(flavor = "multi_thread")]
    async fn encode_loop_retries_and_serves_requests() {
        let _encoders = encoders_turn();
        let (host_conn, conn) = loopback("loop").await;
        let (w, h) = (640u32, 384u32);
        let video = Arc::new(VideoOut::new(host_conn.clone(), false));
        let pipeline = pipeline(&video, w, h, (2, 1), 2.0);
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            wake: Condvar::new(),
            layout: pipeline.encoders.tiles.clone(),
            tiles: 0b11,
            full: AtomicBool::new(false),
            hints: false,
        });
        let (notes_tx, mut notes) = mpsc::channel(8);
        video.set_notices(notes_tx);
        let thread = std::thread::spawn({
            let (shared, video) = (shared.clone(), video.clone());
            move || encode_loop(&shared, pipeline, &video)
        });
        let mut reassemblers: Vec<_> = (0..MAX_TILES).map(|_| transport::video::Reassembler::new()).collect();
        // Byte `x` of a row is in column `x` in both planes (CbCr pairs cover two columns).
        let picture = |seed: u8| move |p: usize, x: usize, y: usize| ((x * 5 + y + p * 9) as u8).wrapping_add(seed * u8::from(x >= w as usize / 2 + 3));

        shared.on_captured(nv12(w as usize, h as usize, picture(0)));
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 2).await), [(0, 0, 0b11, true), (1, 0, 0b11, true)]);
        // Still for a moment: the viewer hears which update was the last.
        let note = tokio::time::timeout(Duration::from_millis(500), notes.recv()).await.unwrap();
        assert_eq!(note, Some(HostMsg::VideoIdle { update: 0, mask: 0b11 }));
        // A frame the tiler can't take (another size) is held and tried again, until a newer one
        // replaces it (only the right half changed).
        shared.on_captured(nv12(w as usize / 2, h as usize, picture(0)));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(shared.state.lock().unwrap().retry_at.is_some());
        shared.on_captured(nv12(w as usize, h as usize, picture(1)));
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(1, 1, 0b10, false)]);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(shared.state.lock().unwrap().retry_at.is_none(), "a frame went through: no more waiting");
        // Asked for while the screen is still.
        shared.request_keyframes(0b01 | FULL_FRAME_BIT);
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(0, 2, 0b01, true)]);
        assert!(tokio::time::timeout(Duration::from_millis(100), conn.read_datagram()).await.is_err());

        shared.state.lock().unwrap().stop = true;
        shared.wake.notify_one();
        tokio::task::block_in_place(|| thread.join().unwrap());
    }

    #[test]
    fn requests_merge_and_wait_their_turn() {
        let t0 = Instant::now();
        let mut k = Requests::default();
        assert_eq!(k.wait(t0), None);
        assert_eq!(k.take(t0), Work::default());
        k.ask(0b101, 0b1111, true);
        k.ask(0b100 | 1 << 40, 0b1111, true);
        assert_eq!(k.wait(t0), Some(Duration::ZERO));
        assert_eq!(k.take(t0), Work { keyframes: 0b101, ..Work::default() });
        assert_eq!(k.wait(t0), None);
        // More arrive right after: merged, held until the interval is up, then all served.
        k.ask(0b10, 0b1111, true);
        k.ask(0b1000, 0b1111, true);
        k.resends |= 0b1001;
        let soon = t0 + KEYFRAME_MIN_INTERVAL / 2;
        assert_eq!(k.wait(soon), Some(KEYFRAME_MIN_INTERVAL / 2));
        assert_eq!(k.take(soon), Work::default());
        let later = t0 + KEYFRAME_MIN_INTERVAL;
        assert_eq!(k.wait(later), Some(Duration::ZERO));
        // A tile asked for both ways goes once, as a keyframe.
        assert_eq!(k.take(later), Work { keyframes: 0b1010, resends: 0b0001, full_keyframe: false, drop_full: false });
        assert_eq!(k.take(later + KEYFRAME_MIN_INTERVAL), Work::default());

        // The full-frame stream's bit: its next frame is a keyframe (whenever that is), and every
        // tile goes again.
        k.ask(FULL_FRAME_BIT, 0b1111, true);
        let at = later + KEYFRAME_MIN_INTERVAL * 2;
        assert_eq!(k.take(at), Work { keyframes: 0, resends: 0b1111, full_keyframe: true, drop_full: false });
        k.ask(FULL_FRAME_BIT, 0b1111, true);
        assert_eq!(k.take(at), Work { keyframes: 0, resends: 0, full_keyframe: true, drop_full: false }, "the flag isn't paced");
        // Without a full-frame stream, nothing.
        let mut none = Requests::default();
        none.ask(FULL_FRAME_BIT, 0b1111, false);
        assert_eq!(none.take(t0), Work::default());
    }

    #[test]
    fn a_lost_full_frame_sends_the_tiles_again_once() {
        let t0 = Instant::now();
        let mut k = Requests::default();
        k.ask(FULL_FRAME_BIT, 0b1111, true);
        assert_eq!(k.take(t0), Work { keyframes: 0, resends: 0b1111, full_keyframe: true, drop_full: false });
        k.done(&Outcome { sent: 0b1111, ..Outcome::default() });
        // Asked again before any full frame went out: the tiles already carry that picture.
        let later = t0 + KEYFRAME_MIN_INTERVAL;
        k.ask(FULL_FRAME_BIT, 0b1111, true);
        assert_eq!(k.take(later), Work { keyframes: 0, resends: 0, full_keyframe: true, drop_full: false });
        // A full frame went out since, and was lost: every tile again.
        k.done(&Outcome { full_sent: true, ..Outcome::default() });
        k.ask(FULL_FRAME_BIT, 0b1111, true);
        assert_eq!(k.take(later + KEYFRAME_MIN_INTERVAL).resends, 0b1111);
    }

    #[test]
    fn no_full_frame_sends_every_tile_again() {
        let mut k = Requests::default();
        k.no_full_frame(0b111);
        assert_eq!(k.take(Instant::now()), Work { keyframes: 0, resends: 0b111, full_keyframe: false, drop_full: true });
    }

    /// A keyframe the viewer waits for is owed until one goes out, even if the encoder dropped
    /// it; an ordinary frame it dropped is just sent again.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_keyframes_are_still_owed() {
        let _encoders = encoders_turn();
        let (host_conn, _conn) = loopback("owed").await;
        let video = Arc::new(VideoOut::new(host_conn, false));
        let mut pipeline = pipeline(&video, 640, 384, (2, 1), 2.0);
        let mut out = Outcome::default();
        pipeline.record(Route::Tile(0), Fate::Lost, 0, true, &mut out);
        pipeline.record(Route::Tile(1), Fate::Lost, 0, false, &mut out);
        assert_eq!((out.keyframes, out.resends), (0b01, 0b10));
    }

    /// The viewer can't decode full frames: they stop for good (this connection's next streams
    /// too), and every tile goes again.
    #[tokio::test(flavor = "multi_thread")]
    async fn no_full_frame_stops_full_frames() {
        let _encoders = encoders_turn();
        let (host_conn, conn) = loopback("nofull").await;
        let (w, h) = (640u32, 384u32);
        let video = Arc::new(VideoOut::new(host_conn, false));
        let mut pipeline = pipeline(&video, w, h, (2, 1), 0.5);
        let mut reassemblers: Vec<_> = (0..MAX_TILES).map(|_| transport::video::Reassembler::new()).collect();
        tokio::task::block_in_place(|| pipeline.update(&video, Some(nv12(w as usize, h as usize, |_, x, y| (x + y) as u8)), u64::MAX, Work::default()));
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(FULL_FRAME_TILE, 0, FULL_FRAME_BIT, true)]);
        let mut k = Requests::default();
        k.no_full_frame(0b11);
        let out = tokio::task::block_in_place(|| pipeline.update(&video, Some(nv12(w as usize, h as usize, |_, x, y| (x * y) as u8)), u64::MAX, k.take(Instant::now())));
        assert!(pipeline.encoders.full.is_none() && video.no_full_frame.load(Ordering::Relaxed));
        assert_eq!(out.sent, 0b11);
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 2).await), [(0, 1, 0b11, true), (1, 1, 0b11, true)]);
        let next = TileEncoders::new(&video, &EncoderConfig { width: w, height: h, fps: 60, bitrate_bps: 8_000_000, codec: Codec::Hevc }, tile_layout(w, h, Some((2, 1))), 0.5).unwrap();
        assert!(next.full.is_none());
    }

    /// Only the tiles ScreenCaptureKit's dirty rectangles name are read, except now and then,
    /// when all are: a change they missed is caught then, and they aren't trusted for a while.
    #[tokio::test(flavor = "multi_thread")]
    async fn dirty_rectangles_limit_the_comparison_but_are_checked() {
        let _encoders = encoders_turn();
        let (host_conn, conn) = loopback("dirty").await;
        let (w, h) = (640u32, 384u32);
        let video = Arc::new(VideoOut::new(host_conn, false));
        let mut pipeline = pipeline(&video, w, h, (2, 1), 2.0);
        let mut reassemblers: Vec<_> = (0..MAX_TILES).map(|_| transport::video::Reassembler::new()).collect();
        let update = |pipeline: &mut Pipeline, frame: CapturedFrame, touched: u64| {
            tokio::task::block_in_place(|| pipeline.update(&video, Some(frame), touched, Work::default()).sent)
        };
        // Seed `s` changes the left tile only.
        let picture = |s: u8| nv12(w as usize, h as usize, move |p, x, y| ((x + y + p) as u8).wrapping_add(s * u8::from(x < 300)));
        assert_eq!(update(&mut pipeline, picture(0), u64::MAX), 0b11);
        receive(&conn, &mut reassemblers, 2).await;
        // The left tile changed, but the rectangles only name the right one: not read.
        assert_eq!(update(&mut pipeline, picture(1), 0b10), 0);
        assert!(pipeline.distrust.is_none());
        // Time for a full comparison: the change is found, and the rectangles lose trust.
        pipeline.compared_all = Some(Instant::now() - FULL_COMPARE_INTERVAL);
        assert_eq!(update(&mut pipeline, picture(1), 0b10), 0b01);
        assert!(pipeline.distrust.is_some());
        // Meanwhile every tile is compared whatever the rectangles say.
        assert_eq!(update(&mut pipeline, picture(2), 0b10), 0b01);
    }

    /// A change the dirty rectangles missed in the last frame before the screen went still is
    /// found before the screen counts as still.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_still_screen_is_checked_whole_before_it_counts_as_still() {
        let _encoders = encoders_turn();
        let (host_conn, conn) = loopback("settle").await;
        let (w, h) = (640u32, 384u32);
        let video = Arc::new(VideoOut::new(host_conn, false));
        let mut pipeline = pipeline(&video, w, h, (2, 1), 2.0);
        let mut reassemblers: Vec<_> = (0..MAX_TILES).map(|_| transport::video::Reassembler::new()).collect();
        let picture = |s: u8| nv12(w as usize, h as usize, move |p, x, y| ((x + y + p) as u8).wrapping_add(s * u8::from(x < 300)));
        tokio::task::block_in_place(|| pipeline.update(&video, Some(picture(0)), u64::MAX, Work::default()));
        receive(&conn, &mut reassemblers, 2).await;
        // The left tile changed; the rectangles only name the right one.
        let out = tokio::task::block_in_place(|| pipeline.update(&video, Some(picture(1)), 0b10, Work::default()));
        assert_eq!(out.sent, 0);
        let out = tokio::task::block_in_place(|| pipeline.settle(&video));
        assert_eq!(out.sent, 0b01);
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(0, 1, 0b01, false)]);
        assert!(pipeline.distrust.is_some());
        // Nothing more to check.
        assert_eq!(tokio::task::block_in_place(|| pipeline.settle(&video)).update, None);
    }

    /// A full frame carries every tile: tiles the dirty rectangles left out are compared too, so
    /// what the tiler remembers is what the viewer shows.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_frame_compares_every_tile_it_carries() {
        let _encoders = encoders_turn();
        let (host_conn, conn) = loopback("fullrest").await;
        let (w, h) = (1280u32, 384u32);
        let video = Arc::new(VideoOut::new(host_conn, false));
        let mut pipeline = pipeline(&video, w, h, (4, 1), 0.5);
        let mut reassemblers: Vec<_> = (0..MAX_TILES).map(|_| transport::video::Reassembler::new()).collect();
        // Seed `s[i]` changes tile `i` (320 px wide each).
        let picture = |s: [u8; 4]| nv12(w as usize, h as usize, move |p, x, y| ((x + y + p) as u8).wrapping_add(s[(x / 320).min(3)]));
        tokio::task::block_in_place(|| pipeline.update(&video, Some(picture([0; 4])), u64::MAX, Work::default()));
        receive(&conn, &mut reassemblers, 1).await;
        // Tiles 0-2 changed (75%: a full frame), and tile 3 too, but the rectangles miss it.
        tokio::task::block_in_place(|| pipeline.update(&video, Some(picture([1, 1, 1, 1])), 0b0111, Work::default()));
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(FULL_FRAME_TILE, 1, FULL_FRAME_BIT, false)]);
        // Tile 3 goes back: it differs from what the full frame showed, so it goes out.
        let out = tokio::task::block_in_place(|| pipeline.update(&video, Some(picture([1, 1, 1, 0])), 0b1000, Work::default()));
        assert_eq!(out.sent, 0b1000);
    }

    /// An encoder that keeps refusing frames is made again, and its tile's next frame is a
    /// keyframe; the full frame's is dropped.
    #[tokio::test(flavor = "multi_thread")]
    async fn encoders_that_keep_failing_are_made_again() {
        let _encoders = encoders_turn();
        let (host_conn, _conn) = loopback("repair").await;
        let video = Arc::new(VideoOut::new(host_conn, false));
        let mut pipeline = pipeline(&video, 640, 384, (2, 1), 0.5);
        pipeline.failures[1] = MAX_ENCODE_FAILURES;
        pipeline.failures[2] = MAX_ENCODE_FAILURES;
        let mut out = Outcome { resends: 0b10, ..Outcome::default() };
        tokio::task::block_in_place(|| pipeline.repair(&mut out));
        assert_eq!((out.keyframes, out.resends), (0b10, 0));
        assert!(pipeline.encoders.full.is_none());
        assert_eq!(pipeline.failures, [0, 0, 0]);
    }

    /// A frame whose dirty rectangles name the wrong tile sends nothing; the change it missed is
    /// still found once the screen is still, and the viewer told.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_change_the_rectangles_missed_is_found_once_still() {
        let _encoders = encoders_turn();
        let (host_conn, conn) = loopback("quiet").await;
        let (w, h) = (640u32, 384u32);
        let video = Arc::new(VideoOut::new(host_conn.clone(), false));
        let pipeline = pipeline(&video, w, h, (2, 1), 2.0);
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            wake: Condvar::new(),
            layout: pipeline.encoders.tiles.clone(),
            tiles: 0b11,
            full: AtomicBool::new(false),
            hints: true,
        });
        let (notes_tx, mut notes) = mpsc::channel(8);
        video.set_notices(notes_tx);
        let thread = std::thread::spawn({
            let (shared, video) = (shared.clone(), video.clone());
            move || encode_loop(&shared, pipeline, &video)
        });
        let mut reassemblers: Vec<_> = (0..MAX_TILES).map(|_| transport::video::Reassembler::new()).collect();
        let picture = |s: u8| move |p: usize, x: usize, y: usize| ((x + y + p) as u8).wrapping_add(s * u8::from(x < 300));
        shared.on_captured(nv12(w as usize, h as usize, picture(0)));
        receive(&conn, &mut reassemblers, 2).await;
        let note = tokio::time::timeout(Duration::from_millis(500), notes.recv()).await.unwrap();
        assert_eq!(note, Some(HostMsg::VideoIdle { update: 0, mask: 0b11 }));
        // The left tile changes, but the rectangles say the right one did.
        let mut frame = nv12(w as usize, h as usize, picture(1));
        frame.dirty = Some(vec![[400, 0, 410, 10]]);
        shared.on_captured(frame);
        assert_eq!(summary(&receive(&conn, &mut reassemblers, 1).await), [(0, 1, 0b01, false)]);
        let note = tokio::time::timeout(Duration::from_millis(500), notes.recv()).await.unwrap();
        assert_eq!(note, Some(HostMsg::VideoIdle { update: 1, mask: 0b01 }));
        shared.state.lock().unwrap().stop = true;
        shared.wake.notify_one();
        tokio::task::block_in_place(|| thread.join().unwrap());
    }

    #[test]
    fn requests_clear_once_an_update_sent_the_tile() {
        let t0 = Instant::now();
        let mut k = Requests::default();
        k.ask(0b0001, 0b1111, true);
        assert_eq!(k.take(t0).keyframes, 0b0001);
        // While it's out: tile 1 is asked for, and lost frames come back.
        k.ask(0b0010, 0b1111, true);
        k.done(&Outcome { sent: 0b0001, sent_keyframes: 0b0001, resends: 0b0100, keyframes: 0b1000, ..Outcome::default() });
        assert_eq!((k.keyframes, k.resends), (0b1010, 0b0100));
        // Not their turn yet; meanwhile an update sends tiles 1 and 2 (1 as a keyframe, as its
        // encoder had to): 2 needs no resend, 1 no keyframe.
        let soon = t0 + KEYFRAME_MIN_INTERVAL / 2;
        assert_eq!(k.take(soon), Work::default());
        k.done(&Outcome { sent: 0b0110, sent_keyframes: 0b0010, ..Outcome::default() });
        assert_eq!((k.keyframes, k.resends), (0b1000, 0));
        // A keyframe asked for while the update is out may be about that very keyframe: kept.
        assert_eq!(k.take(soon), Work::default());
        k.ask(0b0100, 0b1111, true);
        k.done(&Outcome { sent: 0b1100, sent_keyframes: 0b1100, ..Outcome::default() });
        assert_eq!(k.keyframes, 0b0100);
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
    fn bitrate_is_clamped() {
        assert_eq!(bitrate_for(640, 480, 30), 8_000_000);
        assert_eq!(bitrate_for(7680, 4320, 120), 150_000_000);
    }
}
