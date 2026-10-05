//! Client role: connect to a host, receive and decode its screen, hand frames to the view.

use std::ffi::c_void;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use objc2_core_video::{CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use platform_mac::decoder::{DecodedFrame, Decoder};
use platform_mac::{CVPixelBuffer, clock, system};
use protocol::{
    Arrangement, ClientMsg, Codec, ControlState, CursorState, DEFAULT_PORT, DisplayChoice, DisplayReason, DisplayState, FULL_FRAME_TILE,
    HostMsg, InputMsg, MAX_TILES, MicrophoneReason, MicrophoneState, PROTOCOL_VERSION, STREAM_INPUT, TileRect, VideoFrame,
};
use quinn::{Connection, ConnectionError, RecvStream, SendStream};
use tokio::sync::{mpsc, watch};
use transport::endpoint::{Network, peer_fingerprint};
use transport::framing::{read_msg, write_msg};
use transport::gate::DirectPath;
use transport::identity::Fingerprint;
use transport::knock::AccessKey;
use transport::rendezvous::RendezvousId;
use transport::pairing::client_start;
use serde::Serialize;
use transport::video::{Reassembler, tile_of};

use crate::Trust;
use crate::address_book::Via;
use crate::clipboard::{Clipboard, Note, SessionClipboard};
use crate::microphone::{MicGuard, Microphone};
use crate::rendezvous::{Elsewhere, RelaySession, Rendezvous};
use crate::stats::{FrameTiming, Stats, StatsView};
use crate::view::{TileImage, ViewHandle, ViewSlot};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The same over the internet, where a host that doesn't take this Mac's knock never answers.
const CONNECT_TIMEOUT_INTERNET: Duration = Duration::from_secs(10);
/// When a paired host's address on its own network is on this Mac's network too, a connection
/// over the internet that comes first (a router that sends its public address back inside, or the
/// LanKVM server's relay) waits this much longer for that one: the same Macs, a few ms apart
/// instead of a trip out to the internet, and with the local network's settings.
const LAN_GRACE: Duration = Duration::from_millis(300);
/// A target naming a paired host by its fingerprint, as Paired Devices connects to one.
const PAIRED_TARGET: &str = "lankvm:";
const PING_INTERVAL: Duration = Duration::from_millis(500);
/// A tile frame missing packets for this long is considered lost even if no newer one arrives.
const PARTIAL_FRAME_TIMEOUT: Duration = Duration::from_millis(60);
/// The same for the full-frame stream: a whole-picture frame is many times a tile's size, and on
/// Wi-Fi its packets can take that long to come in.
const FULL_PARTIAL_FRAME_TIMEOUT: Duration = Duration::from_millis(250);
/// An update still missing tiles this long after its first one came is done with: no newer one
/// came to say so earlier, so the screen went still (and the host's encoders are long done).
const UPDATE_TIMEOUT: Duration = Duration::from_millis(150);
/// A tile an update said it sent, but that never came (no packet of it either), is lost after
/// this long unless its next frame comes first and shows nothing was skipped: then the host's
/// encoder dropped that frame (the decoder is fine) and sent this one instead.
const SUSPECT_TIMEOUT: Duration = Duration::from_millis(150);
const KEYFRAME_RETRY: Duration = Duration::from_millis(200);
/// Over the internet, a frame still coming in also gets the time its size takes at what this Mac
/// received over the last second, taken as at least this (bit/s): a still screen receives next to
/// nothing, which says nothing about the path.
const MIN_RECEIVE_BPS: f64 = crate::rate::FLOOR_BPS as f64;
/// Over the internet, missing video is waited on at most this long, however slow the path or long
/// the queue ahead of it.
const MAX_INTERNET_WAIT: Duration = Duration::from_secs(3);
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
    /// Connected over the internet, not on the local network.
    pub internet: bool,
    /// Its traffic goes through a LanKVM server's relay: the routers wouldn't let a direct path
    /// through.
    pub relayed: bool,
    /// The host's fingerprint, for recent hosts.
    #[serde(skip)]
    pub(crate) host_fingerprint: Fingerprint,
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
    /// What this Mac knows about a paired host changed (how to reach it over the internet).
    TrustChanged,
    /// A clipboard of `bytes` was too big to share, so the other Mac's was emptied: this Mac's
    /// (`sent`), or the host's (this Mac's was emptied).
    ClipboardTooLarge { bytes: u64, sent: bool },
    /// Whether the host plays this Mac's microphone now, or why not (see `Event::Microphone`).
    Microphone(MicrophoneState),
}

/// Input written per batch at most; anything more waits for the next write.
const MAX_INPUT_BATCH: usize = 256;

pub type SessionEvents = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// State shared between the network task and the render thread.
#[derive(Default)]
pub struct Shared {
    /// Hand-off to the render thread: decoded tiles go there, never into a queue.
    pub slot: Arc<ViewSlot>,
    pub stats: Mutex<Stats>,
    /// Whether the host lets us control it right now. Input is dropped otherwise.
    controlling: AtomicBool,
    /// The same, for the clipboard (shared while controlling).
    controlling_changed: watch::Sender<bool>,
    /// Id of our latest control request and whether it asked for control; answers to older
    /// ones are stale.
    latest_request: Mutex<(u32, bool)>,
    /// Test hook: sees every decoded tile before it's drawn. Read-locked, so tiles decoded at
    /// once don't wait for each other.
    frame_probe: RwLock<Option<FrameProbe>>,
    /// Id of our latest display request (they count apart from control requests).
    latest_display_request: Mutex<u32>,
    /// What the session shows, as the host last said.
    info: Mutex<Option<SessionInfo>>,
    /// A display change waiting for its first frame (see [`DISPLAY_SHOWN_TIMEOUT`]).
    pending_display: Mutex<Option<PendingDisplay>>,
    /// The host's latest [`HostMsg::VideoIdle`], for the video task.
    video_idle: Mutex<Option<(u32, u64)>>,
    /// What the host last said about this Mac's microphone.
    microphone: watch::Sender<Option<MicrophoneState>>,
}

/// A `Display` event to send once a frame of `size` is decoded.
struct PendingDisplay {
    event: (u32, SessionInfo, u16, String),
    size: (u32, u32),
    since: Instant,
}

/// A decoded tile, as a [`FrameProbe`] sees it.
pub struct ProbeFrame<'a> {
    /// The tile's picture: `tile.width`×`tile.height`.
    pub pixel_buffer: &'a CVPixelBuffer,
    /// Where the tile sits in the stream.
    pub tile: TileRect,
    /// Size of the whole stream.
    pub stream: (u32, u32),
    /// [`protocol::VideoFrame::update`] and `update_mask`: tiles of one captured frame share them.
    pub update: u32,
    pub update_mask: u64,
    pub timing: FrameTiming,
}

/// Called with each decoded tile (see [`Session::set_frame_probe`]), on a decoder thread.
pub type FrameProbe = Box<dyn Fn(&ProbeFrame<'_>) + Send + Sync>;

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
    clipboard: Option<Arc<Clipboard>>,
    /// Whether the user wants the host to have this Mac's microphone.
    microphone: Arc<watch::Sender<bool>>,
}

impl Session {
    pub fn start(
        rt: &tokio::runtime::Handle,
        network: Network,
        trust: Arc<Trust>,
        rendezvous: Arc<Rendezvous>,
        target: String,
        max_size: (u32, u32),
        max_fps: u32,
        clipboard: Option<(Arc<Clipboard>, watch::Receiver<bool>)>,
        microphone: Option<Arc<Microphone>>,
        events: SessionEvents,
    ) -> Self {
        let shared = Arc::new(Shared::default());
        let conn = Arc::new(Mutex::new(None));
        let (pin_tx, pin_rx) = mpsc::unbounded_channel();
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let hub = clipboard.as_ref().map(|(hub, _)| hub.clone());
        let wants_microphone = Arc::new(watch::channel(false).0);
        let task = rt.spawn({
            let (shared, conn, ctl_tx) = (shared.clone(), conn.clone(), ctl_tx.clone());
            let wants_microphone = wants_microphone.clone();
            async move {
                let ctx = RunCtx {
                    trust,
                    rendezvous,
                    max_size,
                    max_fps,
                    shared,
                    conn_slot: conn,
                    events: events.clone(),
                    pin_rx,
                    ctl: (ctl_tx, ctl_rx),
                    input_rx,
                    clipboard,
                    microphone: (microphone, wants_microphone),
                };
                let result = run(network, &target, ctx).await;
                let error = result.err().map(|e| format!("{e:#}"));
                if let Some(e) = &error {
                    tracing::info!("session ended: {e}");
                }
                events(SessionEvent::Ended { error });
            }
        });
        // Test runs (scripts/e2e-control.sh): what the overlay shows, in the log every second.
        if std::env::var_os("LANKVM_LOG_STATS").is_some() {
            let shared = Arc::downgrade(&shared);
            rt.spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(1));
                tick.tick().await;
                loop {
                    tick.tick().await;
                    let Some(shared) = shared.upgrade() else { break };
                    let view = shared.stats.lock().unwrap().view();
                    tracing::info!(stats = %serde_json::to_string(&view).unwrap_or_default(), "viewer stats");
                }
            });
        }
        Self { shared, conn, pin_tx, ctl_tx, input_tx, task, view: Mutex::new(None), clipboard: hub, microphone: wants_microphone }
    }

    /// Whether to share this Mac's microphone with the host (see `Event::Microphone`).
    pub fn set_microphone(&self, on: bool) {
        self.microphone.send_replace(on);
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
            self.shared.set_controlling(false);
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
            // The user may be about to paste on the host what they just copied here: it goes now
            // rather than at the next poll.
            if forwarding && let Some(clipboard) = &self.clipboard {
                clipboard.check();
            }
            let _ = self.ctl_tx.send(ClientMsg::Focus { forwarding });
        }
    }

    /// Lets a test look at every decoded tile (e.g. to time how fast input shows on screen).
    pub fn set_frame_probe(&self, probe: Option<FrameProbe>) {
        *self.shared.frame_probe.write().unwrap() = probe;
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
    rendezvous: Arc<Rendezvous>,
    max_size: (u32, u32),
    /// The viewer screen's refresh rate: no point streaming faster.
    max_fps: u32,
    shared: Arc<Shared>,
    conn_slot: Arc<Mutex<Option<Connection>>>,
    events: SessionEvents,
    pin_rx: mpsc::UnboundedReceiver<String>,
    ctl: (mpsc::UnboundedSender<ClientMsg>, mpsc::UnboundedReceiver<ClientMsg>),
    input_rx: mpsc::UnboundedReceiver<(InputMsg, u64)>,
    /// This Mac's clipboard, and whether its user lets it be shared.
    clipboard: Option<(Arc<Clipboard>, watch::Receiver<bool>)>,
    /// This Mac's microphone, and whether the user wants the host to have it.
    microphone: (Option<Arc<Microphone>>, Arc<watch::Sender<bool>>),
}

async fn run(network: Network, target: &str, mut ctx: RunCtx) -> Result<()> {
    // A paired host named by its fingerprint, or an address. The relay session a connection
    // through a LanKVM server uses lasts as long as the session.
    let paired = paired_target(target)?;
    // How messages name the host: by its name when connecting to it as a paired Mac, looked up
    // first, as this Mac may forget it meanwhile.
    let name = paired.and_then(|host| host_name(&ctx.trust, &host));
    // How the user wants it reached by name. A typed address goes where it says.
    let via = paired.map_or(Via::Auto, |host| ctx.trust.address_book.lock().unwrap().via(&host));
    let (conn, addr, internet, relay) = match paired {
        Some(host) => {
            let Reached { conn, internet, relay } = connect_to_paired(&network, &ctx.rendezvous, &ctx.trust, host, None, via).await?;
            let addr = conn.remote_address();
            (conn, addr, internet, relay)
        }
        None => {
            let addr = resolve(target).await?;
            let internet = network.gate.is_internet(addr);
            // A paired host known to be at that address (its public IP, say) is reached as
            // Paired Devices does: there, and through its LanKVM server, so its router needn't
            // forward a port.
            let known = internet.then(|| ctx.trust.internet_hosts.lock().unwrap().known_at(target, addr, &ctx.trust.hosts.lock().unwrap())).flatten();
            if let Some(host) = known {
                let Reached { conn, internet, relay } =
                    connect_to_paired(&network, &ctx.rendezvous, &ctx.trust, host, Some(target), Via::Auto).await?;
                let addr = conn.remote_address();
                (conn, addr, internet, relay)
            } else if internet && ctx.trust.internet_hosts.lock().unwrap().any_server(&ctx.trust.hosts.lock().unwrap()) {
                // An IP this Mac doesn't know: its paired Macs' LanKVM servers can say whose it is.
                let Reached { conn, internet, relay } = connect_by_ip(&network, &ctx.rendezvous, &ctx.trust, target, addr).await?;
                let addr = conn.remote_address();
                (conn, addr, internet, relay)
            } else {
                let conn = if internet {
                    connect_over_internet(&network, target, addr, &ctx.trust).await?
                } else {
                    tokio::time::timeout(CONNECT_TIMEOUT, network.endpoint.connect(addr, "lankvm")?)
                        .await
                        .map_err(|_| anyhow!("no answer from {addr} — is LanKVM running there?"))?
                        .context("connect")?
                };
                (conn, addr, internet, None)
            }
        }
    };
    // At the start of a sentence, and within one.
    let who = match (&name, paired) {
        (Some(name), _) => name.clone(),
        (None, Some(_)) => "That Mac".to_string(),
        (None, None) => format!("The Mac at {target}"),
    };
    let within = name.unwrap_or_else(|| "that Mac".to_string());
    *ctx.conn_slot.lock().unwrap() = Some(conn.clone());
    let host_fp = peer_fingerprint(&conn).context("host sent no certificate")?;
    let trusts_host = ctx.trust.hosts.lock().unwrap().contains(&host_fp);
    // Forgotten while connecting.
    if internet && !trusts_host {
        conn.close(0u32.into(), b"");
        match paired {
            Some(_) => bail!("This Mac forgot {within} while connecting to it."),
            None => bail!("The Mac at {target} isn't the one this Mac paired with."),
        }
    }

    // From here until the host says what it shows, a close without a word means it turned this
    // Mac away.
    let away = |e| turned_away(e, &conn, internet, &who);
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| away(e.into()))?;
    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        device_name: system::device_name(),
        max_width: ctx.max_size.0,
        max_height: ctx.max_size.1,
        fps: ctx.max_fps,
        trusts_host,
    };
    write_msg(&mut send, &hello).await.map_err(away)?;
    let info = loop {
        match read_msg::<HostMsg>(&mut recv).await.map_err(away)?.context("host closed the connection")? {
            HostMsg::Welcome { device_name, width, height, fps, codec } => {
                if !trusts_host {
                    ctx.trust.hosts.lock().unwrap().add(host_fp, &device_name)?;
                }
                // The name the user calls it by, if they gave it one.
                let alias = ctx.trust.address_book.lock().unwrap().alias(&host_fp);
                break SessionInfo {
                    host_name: alias.unwrap_or(device_name),
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
                    internet,
                    relayed: network.gate.is_relayed(addr),
                    host_fingerprint: host_fp,
                };
            }
            // Never over the internet: a host there only asks if something is off (it forgot
            // this Mac), and a code typed here would be guessable from anywhere.
            HostMsg::PairingRequired if internet => {
                conn.close(0u32.into(), b"pairing needs the local network");
                let host = host_name(&ctx.trust, &host_fp).unwrap_or_else(|| target.to_string());
                bail!("{host} asked for a pairing code. Pairing only works on the same network: connect to it there first.");
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
                    let on = if paired.is_some() { within.clone() } else { addr.to_string() };
                    bail!("LanKVM on {on} is older than this Mac's. Update LanKVM there, then quit and reopen it.");
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
        .map_err(|_| anyhow!("{} didn't say what it shows", info.host_name))?
        .map_err(away)?
        .context("host closed the connection")?;
    let early = match first {
        HostMsg::Display(state) => {
            apply_display(&mut info, &state);
            None
        }
        other => Some(other),
    };
    let RunCtx { trust, shared, events, ctl: (ctl_tx, mut ctl_rx), input_rx, clipboard, microphone, .. } = ctx;
    tracing::info!(?info, "connected");
    let same_machine = info.same_machine;
    // Where it was reached, to try first next time (and show under Paired Devices). A paired
    // host connected to by name was reached at one of the addresses known already, or through
    // its server.
    // Not one the session went through the server's relay for: that address never answered.
    let remembered = internet && !info.relayed && paired.is_none() && trust.internet_hosts.lock().unwrap().remember_used(&host_fp, target);
    // And where it is on the local network, to connect there by name (and show in Connect).
    let found_locally = !internet && trust.address_book.lock().unwrap().set_local(&host_fp, &addr.to_string());
    *shared.info.lock().unwrap() = Some(info.clone());
    events(SessionEvent::Connected(info));
    if remembered || found_locally {
        events(SessionEvent::TrustChanged);
    }
    // Through the LanKVM server's relay, though the host may be on this Mac's network after all:
    // it says where it is there as the session starts. Not when the user asked for the internet.
    let shortcut = (relay.is_some() && via != Via::Internet).then(|| {
        tokio::spawn(take_the_shortcut(network.clone(), conn.clone(), trust.clone(), host_fp, shared.clone(), events.clone()))
    });
    let host = PairedHost { trust, fingerprint: host_fp };
    if let Some(msg) = early {
        handle_host_msg(msg, &shared, &events, &host);
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
                handle_host_msg(msg, &shared, &events, &host);
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
    // The clipboard is shared while this Mac controls the host, if its user lets it.
    let clipboard = clipboard.map(|(hub, share)| {
        let controlling = shared.controlling_changed.subscribe();
        tokio::spawn(share_clipboard(conn.clone(), hub, share, controlling, same_machine, internet, ctl_tx.clone(), events.clone()))
    });
    let microphone = tokio::spawn(share_microphone(
        conn.clone(),
        microphone,
        shared.microphone.subscribe(),
        ctl_tx.clone(),
        events.clone(),
    ));
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

    let result = receive_video(&conn, internet, &shared, &ctl_tx, &events).await;
    // The shortcut, if any, ends with the connection.
    drop(shortcut);
    drop(relay);
    pinger.abort();
    reader.abort();
    writer.abort();
    input.abort();
    if let Some(clipboard) = clipboard {
        clipboard.abort();
    }
    microphone.abort();
    shared.set_controlling(false);
    result
}

/// Sends this Mac's microphone to the host while the user wants that (`wants`) and the host plays
/// it (`host`), and tells the UI whether it does. A refusal, from the host or this Mac's
/// microphone, turns it off: asking again is the user's call.
async fn share_microphone(
    conn: Connection,
    (hub, wants): (Option<Arc<Microphone>>, Arc<watch::Sender<bool>>),
    mut host: watch::Receiver<Option<MicrophoneState>>,
    ctl: mpsc::UnboundedSender<ClientMsg>,
    events: SessionEvents,
) {
    let mut wanted = wants.subscribe();
    // What the host was last asked for.
    let mut asked = false;
    let mut sending: Option<MicGuard> = None;
    // What the UI was last told.
    let mut shown: Option<MicrophoneState> = None;
    let mut show = |state: MicrophoneState| {
        if shown.as_ref() != Some(&state) {
            shown = Some(state.clone());
            events(SessionEvent::Microphone(state));
        }
    };
    loop {
        let want = *wanted.borrow_and_update();
        let said = host.borrow_and_update().clone();
        if want != asked {
            asked = want;
            if ctl.send(ClientMsg::Microphone { on: want }).is_err() {
                return;
            }
        }
        match said {
            // The host turned it down, or stopped it: off until the user asks again (and the host
            // is told so, should it have changed its mind meanwhile).
            Some(state) if !state.active && state.reason != MicrophoneReason::NONE => {
                sending = None;
                if want {
                    wants.send_replace(false);
                }
                show(state);
            }
            Some(state) if state.active && want => {
                if sending.is_none() {
                    let opened = match &hub {
                        Some(hub) => {
                            let (hub, conn) = (hub.clone(), conn.clone());
                            tokio::task::spawn_blocking(move || hub.join(conn)).await.unwrap_or_else(|e| {
                                Err((MicrophoneReason::CAPTURE_FAILED, format!("Couldn't open this Mac's microphone: {e}")))
                            })
                        }
                        None => Err((MicrophoneReason::NO_INPUT, "This copy of LanKVM shares no microphone.".to_string())),
                    };
                    match opened {
                        Ok(guard) => sending = Some(guard),
                        Err((reason, message)) => {
                            tracing::info!("microphone: {message}");
                            wants.send_replace(false);
                            show(MicrophoneState { active: false, reason, message });
                            continue;
                        }
                    }
                }
                show(state);
            }
            // Asked and not answered yet, stopping, or nothing said yet.
            _ => {
                sending = None;
                show(MicrophoneState { active: false, reason: MicrophoneReason::NONE, message: String::new() });
            }
        }
        tokio::select! {
            changed = wanted.changed() => if changed.is_err() { return },
            changed = host.changed() => if changed.is_err() { return },
        }
    }
}

/// Shares the clipboard with the host while this Mac controls it and its user lets it (`share`),
/// and takes the host's clipboard transfers.
#[allow(clippy::too_many_arguments)]
async fn share_clipboard(
    conn: Connection,
    hub: Arc<Clipboard>,
    mut share: watch::Receiver<bool>,
    mut controlling: watch::Receiver<bool>,
    same_machine: bool,
    internet: bool,
    ctl: mpsc::UnboundedSender<ClientMsg>,
    events: SessionEvents,
) {
    let mut clipboard = SessionClipboard::new(&hub, conn.clone(), internet, same_machine, move |note| {
        let (bytes, sent) = match note {
            Note::TooLargeToSend(bytes) => (bytes, true),
            Note::TooLargeToReceive(bytes) => (bytes, false),
        };
        events(SessionEvent::ClipboardTooLarge { bytes, sent });
    });
    let mut told = None;
    loop {
        let wants = *share.borrow_and_update();
        // The host shares its clipboard only once told.
        if told != Some(wants) {
            told = Some(wants);
            if ctl.send(ClientMsg::ShareClipboard { on: wants }).is_err() {
                return;
            }
        }
        clipboard.set(wants && *controlling.borrow_and_update());
        tokio::select! {
            changed = share.changed() => if changed.is_err() { return },
            changed = controlling.changed() => if changed.is_err() { return },
            // Hosts open streams only for clipboard transfers.
            stream = conn.accept_uni() => match stream {
                Ok(recv) => clipboard.incoming_unread(recv),
                Err(_) => return,
            },
        }
    }
}

/// How often a session through a relay looks for a way straight to the host on this Mac's
/// network, and for how long: the host says where it is there as the session starts.
const SHORTCUT_EVERY: Duration = Duration::from_secs(1);
const SHORTCUT_CHECKS: u32 = 10;
/// How long checking that an address on this Mac's network is the host's may take.
const SHORTCUT_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the host has to answer straight before the session goes back through the relay.
const SHORTCUT_ANSWER: Duration = Duration::from_millis(500);
/// The way straight to the host stays this long after the connection closed, so the packets that
/// close it reach the host (the server may have ended the relay session by then).
const SHORTCUT_LINGER: Duration = Duration::from_secs(1);

/// A session through the LanKVM server's relay moves straight to the host when the host is on
/// this Mac's network after all: two Macs behind one router that doesn't send its public address
/// back inside, reached through the server the first time (this Mac didn't know the host's
/// address on that network yet) or since it got another one. The host's addresses there are
/// checked first, each with a connection of its own, as whoever is at one gets the session's
/// packets. The session's connection then carries on as it was, a few ms apart instead of a trip
/// to the server and back (see `Network::go_direct`). Ends with the connection.
async fn take_the_shortcut(network: Network, conn: Connection, trust: Arc<Trust>, host: Fingerprint, shared: Arc<Shared>, events: SessionEvents) {
    let found = tokio::select! {
        found = find_the_shortcut(&network, &conn, &trust, host) => found,
        _ = conn.closed() => None,
    };
    let Some((path, addr)) = found else { return };
    tracing::info!(%addr, "the session left the relay: straight to the host on the local network");
    shared.set_route(false, addr.to_string(), &events);
    if trust.address_book.lock().unwrap().set_local(&host, &addr.to_string()) {
        events(SessionEvent::TrustChanged);
    }
    conn.closed().await;
    tokio::time::sleep(SHORTCUT_LINGER).await;
    drop(path);
}

/// Looks for the host at its addresses on this Mac's network, as it announces them, a few times;
/// moves the connection to the first that answers as the host.
async fn find_the_shortcut(network: &Network, conn: &Connection, trust: &Trust, host: Fingerprint) -> Option<(DirectPath, SocketAddr)> {
    let mut tried: Vec<SocketAddr> = Vec::new();
    for _ in 0..SHORTCUT_CHECKS {
        tokio::time::sleep(SHORTCUT_EVERY).await;
        let lan = local_addresses(trust, &host);
        let fresh: Vec<SocketAddr> = lan
            .iter()
            .filter_map(|a| a.parse::<SocketAddr>().ok())
            .filter(|a| !tried.contains(a) && crate::on_this_network(a.ip()))
            .collect();
        tried.extend(&fresh);
        // All at once: one that doesn't answer mustn't hold up the others.
        let mut checks = tokio::task::JoinSet::new();
        for addr in fresh {
            let Ok(connecting) = network.endpoint.connect(addr, "lankvm") else { continue };
            checks.spawn(async move {
                let checked = tokio::time::timeout(SHORTCUT_CHECK_TIMEOUT, connecting).await.ok()?.ok()?;
                let is_host = peer_fingerprint(&checked) == Some(host);
                checked.close(0u32.into(), b"checked the way");
                is_host.then_some(addr)
            });
        }
        while let Some(checked) = checks.join_next().await {
            let Ok(Some(addr)) = checked else { continue };
            let path = match network.go_direct(conn.remote_address(), addr) {
                Ok(path) => path,
                Err(e) => {
                    tracing::info!(%addr, "couldn't go straight to the host: {e:#}");
                    return None;
                }
            };
            tokio::time::sleep(SHORTCUT_ANSWER).await;
            if path.heard() > 0 {
                return Some((path, addr));
            }
            // Dropped: back through the relay.
            tracing::info!(%addr, "the host didn't answer straight: staying with the relay");
        }
    }
    None
}

/// Connects to a host outside the local network. It answers only a knock made with the key it
/// gave this Mac, so each paired host `target` may be gets a try with its key, all at once; the
/// first to answer, and to be that host, wins.
async fn connect_over_internet(network: &Network, target: &str, addr: SocketAddr, trust: &Trust) -> Result<Connection> {
    let (candidates, any_key) = {
        let internet = trust.internet_hosts.lock().unwrap();
        let hosts = trust.hosts.lock().unwrap();
        (internet.candidates(target, addr, &hosts), internet.any_key(&hosts))
    };
    if candidates.is_empty() && any_key {
        // Every paired host said it is somewhere else.
        bail!(
            "This Mac doesn't know {target} as the address of a Mac it paired with. Connect to that Mac on the same network once, \
             so it can tell this Mac where to reach it, then connect over the internet."
        );
    }
    if candidates.is_empty() {
        bail!(
            "This Mac hasn't paired with a Mac at {target} yet. Pair on the same network first: connect to it there and enter its code. \
             After that, you can connect over the internet."
        );
    }
    let mut attempts = tokio::task::JoinSet::new();
    for (host_fp, key) in candidates {
        let connecting = network.endpoint.connect_with(network.internet_client_config(key), addr, "lankvm").context("connect")?;
        attempts.spawn(async move { (host_fp, connecting.await) });
    }
    // Dropping the set (an answer, or the timeout) gives up on the other attempts.
    let race = async move {
        let (mut impostor, mut refused) = (false, None);
        while let Some(attempt) = attempts.join_next().await {
            let Ok((host_fp, result)) = attempt else { continue };
            match result {
                Ok(conn) if peer_fingerprint(&conn) == Some(host_fp) => return Ok(conn),
                Ok(conn) => {
                    conn.close(0u32.into(), b"");
                    impostor = true;
                }
                Err(e) => refused = Some(e),
            }
        }
        Err((impostor, refused))
    };
    match tokio::time::timeout(CONNECT_TIMEOUT_INTERNET, race).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err((true, _))) => bail!("The Mac at {target} isn't the one this Mac paired with."),
        // A host that takes the knock but not this Mac's certificate closes once the handshake
        // is over, which is after it is over here: see `turned_away`.
        Ok(Err((false, Some(e)))) => Err(e).context("connect"),
        Ok(Err((false, None))) | Err(_) => bail!(
            "No answer from {target}. On that Mac, check that internet access is on (This Mac in LanKVM) and that its router \
             forwards UDP port {} to it. Its public address may also have changed.",
            addr.port()
        ),
    }
}

/// The paired host a `lankvm:<fingerprint>` target names; None for an address.
fn paired_target(target: &str) -> Result<Option<Fingerprint>> {
    let Some(fingerprint) = target.strip_prefix(PAIRED_TARGET) else { return Ok(None) };
    crate::from_hex(fingerprint.trim()).map(Some).ok_or_else(|| anyhow!("{target} doesn't name a paired Mac."))
}

/// What connecting to a paired host by fingerprint reached.
struct Reached {
    conn: Connection,
    /// Over the internet. (An address a host announced can also lead to the local network.)
    internet: bool,
    /// The LanKVM server's relay session the connection goes through, if it does.
    relay: Option<RelaySession>,
}

/// Connects to paired host `host` by name, the way `via` says. Automatically: at its address on
/// the local network (where this Mac last reached it there, and where it says it is), at every
/// address on the internet it announced or was reached at, and through its LanKVM server, all at
/// once. The first to reach that host wins, and the others are given up (a relay session one of
/// them started ends), except that a connection over the internet that comes first waits up to
/// [`LAN_GRACE`] for one at an address on this Mac's network, still being tried, which wins if it
/// comes in time. `Via::Local` and `Via::Internet` try only the ways on that network. `typed`: an
/// address the user typed for it, tried too.
async fn connect_to_paired(
    network: &Network,
    rendezvous: &Arc<Rendezvous>,
    trust: &Trust,
    host: Fingerprint,
    typed: Option<&str>,
    via: Via,
) -> Result<Reached> {
    if !trust.hosts.lock().unwrap().contains(&host) {
        bail!("This Mac isn't paired with that Mac any more. Connect to it on the same network and enter its code to pair again.");
    }
    // At the start of a sentence, and within one.
    let name = host_name(trust, &host);
    let who = name.clone().unwrap_or_else(|| "That Mac".to_string());
    let within = name.unwrap_or_else(|| "that Mac".to_string());
    let way = trust.internet_hosts.lock().unwrap().way_to(&host);
    // Its addresses on its own network: the way to it when both are at home.
    let local = if via == Via::Internet { Vec::new() } else { local_addresses(trust, &host) };
    let (key, mut addresses, server) = match way {
        Some(way) if via != Via::Local => (Some(way.key), way.addresses, way.rendezvous),
        Some(way) => (Some(way.key), Vec::new(), None),
        None => (None, Vec::new(), None),
    };
    if let Some(typed) = typed
        && via != Via::Local
        && key.is_some()
        && !addresses.iter().any(|a| crate::internet::same_address(a, typed))
    {
        addresses.insert(0, typed.trim().to_string());
    }
    if local.is_empty() && addresses.is_empty() && server.is_none() {
        match via {
            Via::Local => bail!(
                "This Mac doesn't know where {within} is on the local network yet. Type its address in Connect once (This Mac in \
                 LanKVM on {within} shows it), then try again."
            ),
            Via::Internet => bail!(
                "{who} hasn't told this Mac how to reach it over the internet. Turn on internet access on {within} (This Mac in \
                 LanKVM), connect to it once on the same network, then try again."
            ),
            Via::Auto => bail!(
                "This Mac doesn't know where {within} is yet. Type its address in Connect once, on the same network (This Mac in \
                 LanKVM on {within} shows it). To reach it from anywhere, also turn on internet access there."
            ),
        }
    }
    let mut attempts = tokio::task::JoinSet::new();
    // Its addresses on its own network that are on this Mac's network too are tried along with
    // the others but waited for (see LAN_GRACE).
    let mut nearby = 0;
    if !rendezvous.force_relay() {
        let local = local.into_iter().map(|address| {
            let near = address.parse::<SocketAddr>().is_ok_and(|a| crate::on_this_network(a.ip()));
            nearby += usize::from(near);
            (Attempt::Lan { near }, address)
        });
        for (kind, address) in addresses.into_iter().map(|address| (Attempt::Direct, address)).chain(local) {
            let (network, within) = (network.clone(), within.clone());
            // The internet only: an address that leads to the local network here isn't a way.
            let lan_ok = via != Via::Internet;
            attempts.spawn(async move { (kind, connect_at(&network, &within, &address, key, lan_ok).await) });
        }
    }
    if let (Some((server, id)), Some(key)) = (server, key) {
        let (network, rendezvous, who) = (network.clone(), rendezvous.clone(), who.clone());
        attempts.spawn(async move {
            let introduced = rendezvous.connect(&who, &server, id, key, None).await;
            // The address the server named can be on the local network (a server may name any):
            // the host then takes the connection for one from there, and so does this Mac. A
            // relay session's address counts as the internet.
            let reached = introduced.map(|i| Reached { internet: network.gate.is_internet(i.conn.remote_address()), conn: i.conn, relay: i.relay });
            (Attempt::Server, reached)
        });
    }
    // Dropping the set (on an answer) gives up on the other attempts. A connection over the
    // internet that comes first is kept while the nearby ones may still win.
    let (mut through_server, mut direct, mut on_lan) = (None, None, None);
    let mut first: Option<(Reached, Attempt, Instant)> = None;
    loop {
        let next = match &first {
            Some((_, _, deadline)) => match tokio::time::timeout_at((*deadline).into(), attempts.join_next()).await {
                Ok(next) => next,
                Err(_) => break,
            },
            None => attempts.join_next().await,
        };
        let Some(attempt) = next else { break };
        let Ok((kind, result)) = attempt else { continue };
        let near = matches!(kind, Attempt::Lan { near: true });
        nearby -= usize::from(near);
        let error = match result {
            Ok(reached) if peer_fingerprint(&reached.conn) == Some(host) => {
                if !reached.internet || nearby == 0 {
                    if let Some((other, ..)) = first.take() {
                        other.conn.close(0u32.into(), b"");
                    }
                    log_reached(&within, &reached, kind);
                    return Ok(reached);
                }
                match &first {
                    Some(_) => reached.conn.close(0u32.into(), b""),
                    None => first = Some((reached, kind, Instant::now() + LAN_GRACE)),
                }
                continue;
            }
            Ok(reached) => {
                reached.conn.close(0u32.into(), b"");
                anyhow!("The Mac that answered isn't {within}.")
            }
            Err(e) => e,
        };
        tracing::info!(attempt = kind.name(), "couldn't reach {within} this way: {error:#}");
        match kind {
            Attempt::Server => &mut through_server,
            Attempt::Direct => &mut direct,
            Attempt::Lan { .. } => &mut on_lan,
        }
        .get_or_insert(error);
        if nearby == 0
            && let Some((reached, kind, _)) = first.take()
        {
            log_reached(&within, &reached, kind);
            return Ok(reached);
        }
    }
    if let Some((reached, kind, _)) = first {
        log_reached(&within, &reached, kind);
        return Ok(reached);
    }
    // What the server said is the surer news (the host isn't online, or doesn't answer); then
    // what the address tried first came to. Its local network is likely not this Mac's: last.
    Err(through_server.or(direct).or(on_lan).unwrap_or_else(|| anyhow!("Couldn't connect to {within}.")))
}

/// Says in the log which way a paired host was reached.
fn log_reached(within: &str, reached: &Reached, kind: Attempt) {
    let way = if reached.relay.is_some() { "relayed" } else if reached.internet { "internet" } else { "local network" };
    tracing::info!(attempt = kind.name(), way, address = %reached.conn.remote_address(), "reached {within}");
}

/// How [`connect_to_paired`] tries a host.
#[derive(Clone, Copy)]
enum Attempt {
    /// Through its LanKVM server.
    Server,
    /// At an address on the internet it announced or was reached at.
    Direct,
    /// At its address on its own local network; `near` when that is on this Mac's network too.
    Lan { near: bool },
}

/// Where paired host `host` is on its own local network: where this Mac last reached it there,
/// then the addresses it says it has there.
fn local_addresses(trust: &Trust, host: &Fingerprint) -> Vec<String> {
    let mut local: Vec<String> = trust.address_book.lock().unwrap().local(host).into_iter().collect();
    let announced = trust.internet_hosts.lock().unwrap().way_to(host).map(|way| way.lan).unwrap_or_default();
    for address in announced {
        if !local.iter().any(|a| crate::internet::same_address(a, &address)) {
            local.push(address);
        }
    }
    local
}

impl Attempt {
    fn name(self) -> &'static str {
        match self {
            Attempt::Server => "server",
            Attempt::Direct => "direct",
            Attempt::Lan { .. } => "local network",
        }
    }
}

/// Connects to the paired Mac at `addr`, a public IP typed as `target` that this Mac doesn't
/// know yet. As before, it knocks there for the paired Macs that never said where they are; and
/// it asks the LanKVM server of each paired Mac that has one where it sees that Mac, going on with
/// the one it sees at that IP. So a Mac's public IP works the first time, even when its router
/// doesn't forward a port (and the next time, as a known address, it is tried directly too).
async fn connect_by_ip(network: &Network, rendezvous: &Arc<Rendezvous>, trust: &Arc<Trust>, target: &str, addr: SocketAddr) -> Result<Reached> {
    let ways: Vec<(Fingerprint, String, (String, RendezvousId), AccessKey)> = {
        let internet = trust.internet_hosts.lock().unwrap();
        let hosts = trust.hosts.lock().unwrap();
        hosts
            .entries()
            .iter()
            .filter_map(|(fp, name)| {
                let way = internet.way_to(fp)?;
                Some((*fp, name.clone(), way.rendezvous?, way.key))
            })
            .collect()
    };
    let mut attempts = tokio::task::JoinSet::new();
    {
        let (network, trust, target) = (network.clone(), trust.clone(), target.to_string());
        attempts.spawn(async move {
            let conn = connect_over_internet(&network, &target, addr, &trust).await;
            (None, conn.map(|conn| Reached { conn, internet: true, relay: None }))
        });
    }
    let asked = ways.len();
    for (host, name, (server, id), key) in ways {
        let (network, rendezvous) = (network.clone(), rendezvous.clone());
        let name = if name.is_empty() { "That Mac".to_string() } else { name };
        attempts.spawn(async move {
            let introduced = rendezvous.connect(&name, &server, id, key, Some(addr.ip())).await;
            let reached = introduced.map(|i| Reached { internet: network.gate.is_internet(i.conn.remote_address()), conn: i.conn, relay: i.relay });
            (Some(host), reached)
        });
    }
    // Dropping the set (on an answer) gives up on the other attempts.
    let (mut through_server, mut direct) = (None, None);
    while let Some(attempt) = attempts.join_next().await {
        let Ok((host, result)) = attempt else { continue };
        match (host, result) {
            // The knock checked the host's certificate already.
            (None, Ok(reached)) => return Ok(reached),
            (Some(host), Ok(reached)) if peer_fingerprint(&reached.conn) == Some(host) => return Ok(reached),
            (Some(_), Ok(reached)) => reached.conn.close(0u32.into(), b""),
            (None, Err(e)) => direct = Some(e),
            // Seen at another IP: not the Mac meant, nothing to say about it.
            (Some(_), Err(e)) if e.is::<Elsewhere>() => {}
            (Some(_), Err(e)) => {
                through_server.get_or_insert(e);
            }
        }
    }
    // What a server said is about the Mac meant only when there was just one to ask.
    let through_server = through_server.filter(|_| asked == 1);
    Err(through_server.or(direct).unwrap_or_else(|| anyhow!("No answer from {target}.")))
}

/// Connects to paired host `name` (as named within a sentence) at `address`, one it announced or
/// was reached at. An address on the internet takes a knock with `key`, so none goes there without
/// one; `lan_ok`: one that leads to the local network here is a way too.
async fn connect_at(network: &Network, name: &str, address: &str, key: Option<AccessKey>, lan_ok: bool) -> Result<Reached> {
    let addr = resolve(address).await?;
    if !network.gate.is_internet(addr) {
        if !lan_ok {
            bail!("{address} leads to the local network here, not over the internet.");
        }
        // A name that leads to the local network here (the host's own name, at home).
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, network.endpoint.connect(addr, "lankvm")?)
            .await
            .map_err(|_| anyhow!("No answer from {name} at {address}."))?
            .context("connect")?;
        return Ok(Reached { conn, internet: false, relay: None });
    }
    // Never a connection without a knock: the gate there would drop it anyway.
    let Some(key) = key else { bail!("{address} isn't on the local network here.") };
    let connecting = network.endpoint.connect_with(network.internet_client_config(key), addr, "lankvm").context("connect")?;
    let conn = tokio::time::timeout(CONNECT_TIMEOUT_INTERNET, connecting)
        .await
        .map_err(|_| {
            anyhow!(
                "No answer from {name} at {address}. On that Mac, check that internet access is on (This Mac in LanKVM) and that \
                 its router forwards UDP port {} to it. Its public address may also have changed.",
                addr.port()
            )
        })?
        .context("connect")?;
    Ok(Reached { conn, internet: true, relay: None })
}

/// `e`, unless the host closed the connection without a word over the internet: it took this
/// Mac's knock but not its certificate (it forgot this Mac while it connected, or this Mac's
/// identity is new), and says nothing more to a device it doesn't know. `who` names the host
/// ("The Mac at 203.0.113.7", or its name).
fn turned_away(e: anyhow::Error, conn: &Connection, internet: bool, who: &str) -> anyhow::Error {
    match conn.close_reason() {
        Some(ConnectionError::ApplicationClosed(close)) if internet && close.error_code.into_inner() == 0 && close.reason.is_empty() => anyhow!(
            "{who} turned this Mac away. Connect to it on the same network once more, then try again over the internet."
        ),
        _ => e,
    }
}

/// The name this Mac calls `host` by: the user's for it, or the one it paired under.
fn host_name(trust: &Trust, host: &Fingerprint) -> Option<String> {
    if let Some(alias) = trust.address_book.lock().unwrap().alias(host) {
        return Some(alias);
    }
    let hosts = trust.hosts.lock().unwrap();
    hosts.entries().iter().find(|(fp, _)| fp == host).map(|(_, name)| name.clone()).filter(|name| !name.is_empty())
}

/// The host a session talks to, for what it tells this Mac to remember about it.
struct PairedHost {
    trust: Arc<Trust>,
    fingerprint: Fingerprint,
}

fn handle_host_msg(msg: HostMsg, shared: &Shared, events: &SessionEvents, host: &PairedHost) {
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
            shared.set_controlling(state.active);
            drop(latest);
            events(SessionEvent::Control(state));
        }
        HostMsg::CursorShape { id, png, width, height, hot_x, hot_y } => {
            events(SessionEvent::CursorShape { id, png, width, height, hot_x, hot_y });
        }
        HostMsg::Cursor(state) => events(SessionEvent::Cursor(state)),
        HostMsg::InputAck { seq, received_us, injected_us } => shared.stats.lock().unwrap().on_input_ack(seq, received_us, injected_us),
        HostMsg::Display(state) => on_display(state, shared, events),
        HostMsg::VideoIdle { update, mask } => *shared.video_idle.lock().unwrap() = Some((update, mask)),
        HostMsg::Microphone(state) => {
            shared.microphone.send_replace(Some(state));
        }
        HostMsg::InternetAccess { key, addresses, rendezvous_server, rendezvous_id } => {
            let Ok(key) = AccessKey::try_from(key.as_slice()) else {
                tracing::warn!(len = key.len(), "ignoring an internet access key of the wrong size");
                return;
            };
            // Before the lock: a host sending a great many costs next to nothing.
            let addresses = crate::internet::clean_announced(addresses);
            let rendezvous = crate::internet::clean_rendezvous(rendezvous_server, rendezvous_id);
            tracing::info!(?addresses, server = rendezvous.as_ref().map(|(server, _)| server.as_str()), "host's internet addresses");
            if host.trust.internet_hosts.lock().unwrap().set_announced(host.fingerprint, key, addresses, rendezvous) {
                events(SessionEvent::TrustChanged);
            }
        }
        other => tracing::debug!("ignoring {other:?}"),
    }
}

/// The host says what the session shows now. That is always the truth about the stream, even when
/// it answers an older request (only the UI cares which request it answers), so it always applies.
fn on_display(state: DisplayState, shared: &Shared, events: &SessionEvents) {
    let (width, height) = (state.width, state.height);
    let mut before = (width, height);
    let mut pending = shared.pending_display.lock().unwrap();
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

impl Shared {
    fn set_controlling(&self, on: bool) {
        self.controlling.store(on, Ordering::Release);
        self.controlling_changed.send_replace(on);
    }

    /// The session's connection now goes to the host `relayed` or not, at `address`: told to the
    /// app with the display change waiting for its first frame, if there is one (it carries the
    /// whole session info, which would undo it otherwise), or now.
    fn set_route(&self, relayed: bool, address: String, events: &SessionEvents) {
        // In the order `on_display` takes them, so an info it copied can't come after this.
        let mut pending = self.pending_display.lock().unwrap();
        let info = self.info.lock().unwrap().as_mut().map(|info| {
            info.relayed = relayed;
            info.address = address.clone();
            info.clone()
        });
        if let Some(waiting) = pending.as_mut() {
            (waiting.event.1.relayed, waiting.event.1.address) = (relayed, address);
        } else if let Some(info) = info {
            emit_display((0, info, DisplayReason::NONE.0, String::new()), events);
        }
    }
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
                let mut stream = conn.open_uni().await.context("open input stream")?;
                // Ahead of the clipboard's streams (it doesn't outrank video datagrams).
                let _ = stream.set_priority(100);
                // Goes out with the first input written below.
                stream.write_all(&[STREAM_INPUT]).await.context("write input")?;
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

/// What a tile frame's decoder callback needs besides the picture.
struct TileMeta {
    tile: TileRect,
    stream: (u32, u32),
    update: u32,
    update_mask: u64,
    keyframe: bool,
    /// [`TileState::submitted`] when it went into the decoder.
    seq: u64,
    timing: FrameTiming,
}

/// A tile frame the decoder couldn't decode (reported from its callback).
struct DecodeFailed {
    tile: u8,
    update: u32,
    seq: u64,
    keyframe: bool,
    /// It decoded, but not to its tile's size. The host's next keyframe would most likely do the
    /// same, so it is asked for as rarely as one this Mac can't decode.
    wrong_size: bool,
}

/// One tile's receive state: each tile is its own video stream, with its own frame ids and its
/// own decoder. The full-frame stream ([`FULL_FRAME_TILE`]) is one more of them.
#[derive(Default)]
struct TileState {
    reassembler: Reassembler,
    decoder: Option<Decoder<TileMeta>>,
    /// Parameter sets this Mac's decoder refused, and when: their keyframes aren't tried again
    /// for [`UNDECODABLE_RETRY`] (a refusal may pass, e.g. while the media server restarts).
    undecodable: Option<(Vec<Vec<u8>>, Instant)>,
    /// Frames handed to a decoder so far, and the count when the latest keyframe went in: a
    /// failure from before that keyframe no longer matters.
    submitted: u64,
    keyframe_seq: u64,
}

/// Datagrams → tile frames → hardware decoders → view. Runs until the connection closes.
///
/// Each tile is reassembled and decoded on its own. Decoding is asynchronous, so this task only
/// queues frames and never waits for the hardware; the decoder callbacks hand tiles to the view.
/// Over the `internet`, it waits on missing video in step with the round trip (see [`Timeouts`]).
async fn receive_video(
    conn: &Connection,
    internet: bool,
    shared: &Arc<Shared>,
    ctl: &mpsc::UnboundedSender<ClientMsg>,
    events: &SessionEvents,
) -> Result<()> {
    let mut tiles: Vec<TileState> = (0..MAX_TILES).map(|_| TileState::default()).collect();
    let mut needs = KeyframeNeeds::new(Instant::now(), None);
    let mut meter = UpdateMeter::default();
    // Size of the stream being shown; frames of any other size are leftovers of the last one.
    let mut stream: Option<(u32, u32)> = None;
    let mut other_size = OtherSize::default();
    // The stream size already reported as undecodable: once per picture, not once per tile.
    let mut reported_undecodable: Option<(u32, u32)> = None;
    let (failed_tx, mut failed_rx) = mpsc::unbounded_channel::<DecodeFailed>();
    let mut check = tokio::time::interval(Duration::from_millis(20));
    // Tiles an update sent that never came, since when (see [`SUSPECT_TIMEOUT`]).
    let mut suspects: [Option<Instant>; MAX_TILES] = [None; MAX_TILES];
    // Where each tile of the stream is: a keyframe placing one elsewhere starts a new layout (the
    // host fell back to fewer tiles at the same size).
    let mut rects: [Option<TileRect>; MAX_TILES] = [None; MAX_TILES];
    // What the view was last told about tiles waiting for a keyframe.
    let mut waiting = 0u64;
    let mut canvas = CanvasRepair::default();
    let mut timeouts = Timeouts::LAN;
    // Over the internet, when the last video came (see [`Timeouts::internet`]).
    let mut last_video = Instant::now();

    loop {
        tokio::select! {
            datagram = conn.read_datagram() => {
                let datagram = match datagram {
                    Ok(d) => d,
                    Err(quinn::ConnectionError::ApplicationClosed(_) | quinn::ConnectionError::LocallyClosed) => return Ok(()),
                    Err(e) => return Err(e).context("connection lost"),
                };
                let Some(t) = tile_of(&datagram) else { continue };
                if internet {
                    last_video = Instant::now();
                }
                let Some(frame) = tiles[usize::from(t)].reassembler.push(&datagram) else { continue };
                let received_us = clock::now_us();
                let now = Instant::now();
                let video: VideoFrame = match protocol::decode(&frame.data) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("bad frame: {e}");
                        continue;
                    }
                };
                if !tile_fits(&video, t) {
                    tracing::warn!(t, tile = ?video.tile, video.width, video.height, "frame's tile doesn't fit its stream");
                    continue;
                }
                let size = (video.width, video.height);
                let moved = rects[usize::from(t)].is_some_and(|r| r != video.tile);
                if moved && !video.keyframe && stream == Some(size) {
                    continue; // a straggler of the layout before
                }
                if video.keyframe && (stream != Some(size) || moved) {
                    // A new picture (another display, a new size, or tiles laid out anew): its tiles
                    // start over. Decoders
                    // of the old one go; tiles it doesn't have never come back.
                    tracing::debug!(?size, "new stream");
                    stream = Some(size);
                    reported_undecodable = None;
                    needs = KeyframeNeeds::new(now, Some(size));
                    needs.pace(&timeouts);
                    other_size = OtherSize::default();
                    suspects = [None; MAX_TILES];
                    rects = [None; MAX_TILES];
                    meter.flush(|bytes| shared.stats.lock().unwrap().on_frame_received(bytes));
                    for tile in &mut tiles {
                        tile.decoder = None;
                        tile.undecodable = None;
                    }
                } else if stream != Some(size) {
                    // A straggler from the picture before, or the next picture whose keyframes
                    // were lost: nothing to decode it with either way.
                    if other_size.frame(size, frame.skipped > 0, now, timeouts.keyframe_retry) {
                        tracing::debug!(?size, ?stream, "frames of another size keep coming");
                        send_keyframe_request(shared, ctl, ClientMsg::RequestKeyframe);
                    }
                    continue;
                }
                rects[usize::from(t)] = Some(video.tile);
                needs.seen(t);
                if frame.skipped > 0 {
                    shared.stats.lock().unwrap().frames_lost += u64::from(frame.skipped);
                    // A keyframe repairs the tile whatever it skipped (one that doesn't decode is
                    // asked for again below).
                    if !video.keyframe {
                        needs.lost(t);
                    }
                }
                // This tile's own frame settles what it was suspected of: frames skipped before
                // it are a loss (just noted); none means the host's encoder dropped a frame and
                // this one replaces it.
                suspects[usize::from(t)] = None;
                let missing = record_arrival(shared, &mut meter, &video, frame.data.len(), received_us, now);
                if missing.unknown {
                    // A run of updates lost entirely: which tiles they had is unknown.
                    let this = if video.keyframe { video.tile.bit() } else { 0 };
                    for t in tiles_in(needs.seen_mask() & !this) {
                        needs.lost(t);
                    }
                }
                // Packets of a missing tile that came before this frame started are the missing
                // frame's (the host sends an update only once the one before is out); later ones
                // may be a resend of it.
                let cutoff = frame.first_seen.checked_sub(Duration::from_micros(500));
                suspect(missing.tiles & !video.tile.bit(), &mut suspects, &mut needs, &tiles, cutoff, now);
                request_keyframes(&mut needs, &mut tiles, &timeouts, shared, ctl);

                if video.keyframe {
                    let tile = &mut tiles[usize::from(t)];
                    if !tile.decoder.as_ref().is_some_and(|d| d.matches(video.codec, &video.param_sets)) {
                        tile.decoder = None;
                        if tile.undecodable.as_ref().is_some_and(|(sets, at)| *sets == video.param_sets && now.duration_since(*at) < UNDECODABLE_RETRY) {
                            continue;
                        }
                        match new_decoder(&video, shared, events, &failed_tx) {
                            Ok(d) => {
                                tile.decoder = Some(d);
                                tile.undecodable = None;
                                needs.set_undecodable(t, false, now);
                            }
                            Err(e) if t == FULL_FRAME_TILE => {
                                // The tiles decode, only not the whole picture at once: the host
                                // sends everything as tiles from now on.
                                tracing::warn!(width = video.width, height = video.height, "full-frame decoder: {e:#}; asking for tiles only");
                                tile.undecodable = Some((video.param_sets.clone(), now));
                                needs.drop_full();
                                send_keyframe_request(shared, ctl, ClientMsg::NoFullFrame);
                                continue;
                            }
                            Err(e) => {
                                // Keep the session: the host can still switch to something this
                                // Mac decodes (its own screen, a smaller display).
                                tracing::warn!(width = video.width, height = video.height, ?video.tile, "decoder: {e:#}");
                                if reported_undecodable != Some(size) {
                                    reported_undecodable = Some(size);
                                    events(SessionEvent::StreamError {
                                        message: format!("This Mac can't decode the {}×{} picture ({e:#}).", video.width, video.height),
                                        width: video.width,
                                        height: video.height,
                                    });
                                }
                                tile.undecodable = Some((video.param_sets.clone(), now));
                                needs.set_undecodable(t, true, now);
                                needs.mark(t);
                                continue;
                            }
                        }
                    }
                    needs.got_keyframe(&video.tile);
                }
                if needs.needs(t) {
                    needs.frame_while_needed(t, now);
                    request_keyframes(&mut needs, &mut tiles, &timeouts, shared, ctl);
                    continue;
                }
                let tile = &mut tiles[usize::from(t)];
                let Some(dec) = tile.decoder.as_ref() else {
                    needs.mark(t);
                    continue;
                };
                let seq = tile.submitted;
                tile.submitted += 1;
                if video.keyframe {
                    tile.keyframe_seq = seq;
                }
                let capture_local_us = shared.stats.lock().unwrap().clock.to_local(video.capture_time_us);
                let meta = TileMeta {
                    tile: video.tile,
                    stream: size,
                    update: video.update,
                    update_mask: video.update_mask,
                    keyframe: video.keyframe,
                    seq,
                    timing: FrameTiming { capture_local_us, received_us, decoded_us: 0 },
                };
                shared.slot.expect_tile(video.tile, size, video.update, video.update_mask);
                if let Err(e) = dec.decode(&video.data, meta) {
                    // The session itself may be broken (e.g. after a GPU reset): make a new one
                    // with the next keyframe.
                    tracing::warn!(t, "decode: {e:#}");
                    shared.slot.tile_failed(t, video.update);
                    tile.decoder = None;
                    needs.lost(t);
                    request_keyframes(&mut needs, &mut tiles, &timeouts, shared, ctl);
                }
            }
            Some(failed) = failed_rx.recv() => {
                shared.slot.tile_failed(failed.tile, failed.update);
                let tile = &mut tiles[usize::from(failed.tile)];
                if failed.seq < tile.keyframe_seq || tile.decoder.is_none() {
                    continue; // a keyframe since then repaired it, or one is awaited anyway
                }
                shared.stats.lock().unwrap().frames_lost += 1;
                if failed.keyframe {
                    // Even a fresh start failed: the next keyframe gets a new session.
                    tile.decoder = None;
                    if failed.wrong_size && failed.tile == FULL_FRAME_TILE {
                        tracing::warn!("full frames don't decode to the picture's size; asking for tiles only");
                        needs.drop_full();
                        send_keyframe_request(shared, ctl, ClientMsg::NoFullFrame);
                        continue;
                    }
                    if failed.wrong_size {
                        needs.set_undecodable(failed.tile, true, Instant::now());
                    }
                }
                needs.lost(failed.tile);
                request_keyframes(&mut needs, &mut tiles, &timeouts, shared, ctl);
            }
            _ = check.tick() => {
                let now = Instant::now();
                if internet {
                    let receiving = shared.stats.lock().unwrap().mbps() * 1e6;
                    timeouts = Timeouts::internet(conn.rtt(), receiving, now.saturating_duration_since(last_video));
                    needs.pace(&timeouts);
                }
                flush_display(shared, events, None);
                // Updates nothing newer followed: the screen went still, so what they still miss
                // isn't coming.
                for t in tiles_in(meter.flush_stale(now, timeouts.update, |bytes| shared.stats.lock().unwrap().on_frame_received(bytes))) {
                    needs.seen(t);
                    // One still coming in is left to its own (size-aware) timeout below.
                    if tiles[usize::from(t)].reassembler.oldest_partial().is_none() {
                        needs.lost(t);
                    }
                }
                // The host says the screen went still after an update: anything of it (or of
                // updates before it) not here yet is a suspect.
                if let Some((update, mask)) = shared.video_idle.lock().unwrap().take() {
                    let missing = meter.idle(update, mask, |bytes| shared.stats.lock().unwrap().on_frame_received(bytes));
                    if missing.unknown {
                        for t in tiles_in(needs.seen_mask()) {
                            needs.lost(t);
                        }
                    }
                    suspect(missing.tiles, &mut suspects, &mut needs, &tiles, None, now);
                }
                for (t, since) in suspects.iter_mut().enumerate() {
                    if since.is_some_and(|at| now.duration_since(at) >= timeouts.suspect) {
                        *since = None;
                        // One still coming in is left to its own (size-aware) timeout below.
                        if tiles[t].reassembler.oldest_partial().is_none() {
                            needs.lost(t as u8);
                        }
                    }
                }
                let mut unknown = 0u64;
                for (t, tile) in tiles.iter_mut().enumerate() {
                    if !timeouts.stale(&tile.reassembler, t == usize::from(FULL_FRAME_TILE), now) {
                        continue;
                    }
                    if needs.is_seen(t as u8) {
                        needs.lost(t as u8);
                    } else {
                        // A tile this stream never showed: a leftover of the picture before, or
                        // a tile whose whole update was this one frame. Asked for once: the
                        // host ignores a tile it doesn't have.
                        tile.reassembler.clear_partial();
                        unknown |= 1u64 << t;
                    }
                }
                if unknown != 0 {
                    send_keyframe_request(shared, ctl, ClientMsg::RequestKeyframes { tiles: unknown });
                }
                if canvas.due(shared.slot.take_canvas_lost(), now, timeouts.keyframe_retry) {
                    // The decoders are fine, only the picture they drew is gone: every tile again.
                    tracing::info!("view lost its picture: asking for every tile");
                    send_keyframe_request(shared, ctl, ClientMsg::RequestKeyframe);
                }
                request_keyframes(&mut needs, &mut tiles, &timeouts, shared, ctl);
            }
        }
        // Updates needn't wait for tiles whose frames are dropped until their keyframe comes.
        if needs.need != waiting {
            waiting = needs.need;
            shared.slot.set_waiting(waiting);
        }
    }
}

/// How long missing video may take before it counts as lost, and how often a keyframe is asked
/// for again. On the local network, the constants above. Over the internet they grow with the
/// round trip, with the size of a frame still coming in, and while video keeps coming in: a frame
/// still on its way there isn't lost, and asking for it again would cost a keyframe, bigger
/// still, that takes longer yet to come (and so on: a keyframe storm).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Timeouts {
    /// A tile frame missing packets (the full frame's apart).
    partial: Duration,
    full_partial: Duration,
    /// Over the internet, frames also get the time their size takes at this rate (bit/s).
    receive_bps: Option<f64>,
    /// An update missing tiles (see [`UpdateMeter`]), and a tile an update said it sent.
    update: Duration,
    suspect: Duration,
    keyframe_retry: Duration,
    /// Before every tile of a new stream is asked for again (see [`KeyframeNeeds`]).
    all_retry: Duration,
}

impl Timeouts {
    const LAN: Self = Self {
        partial: PARTIAL_FRAME_TIMEOUT,
        full_partial: FULL_PARTIAL_FRAME_TIMEOUT,
        receive_bps: None,
        update: UPDATE_TIMEOUT,
        suspect: SUSPECT_TIMEOUT,
        keyframe_retry: KEYFRAME_RETRY,
        all_retry: KEYFRAME_RETRY,
    };

    /// Over the internet, with a round trip of `rtt`, `receive_bps` received over the last second,
    /// and the last video `quiet` ago. Tiles of an update may wait in the host's send buffer behind
    /// the others for long after a round trip (a stream's first keyframes take seconds at a few
    /// Mbit/s): while video keeps coming in, none of them is missing yet, and the first keyframes
    /// of a stream may be among it (up to [`MAX_INTERNET_WAIT`]). (A tile an update said it sent
    /// is suspected only once something sent after it arrived: the host's buffer sends in order.)
    fn internet(rtt: Duration, receive_bps: f64, quiet: Duration) -> Self {
        let update = UPDATE_TIMEOUT + rtt * 2;
        let arriving = quiet < update;
        let keyframe_retry = KEYFRAME_RETRY.max(rtt * 3);
        Self {
            partial: PARTIAL_FRAME_TIMEOUT + rtt * 2,
            full_partial: FULL_PARTIAL_FRAME_TIMEOUT + rtt * 2,
            receive_bps: Some(receive_bps.max(MIN_RECEIVE_BPS)),
            update: if arriving { update.max(MAX_INTERNET_WAIT) } else { update },
            suspect: SUSPECT_TIMEOUT + rtt * 2,
            keyframe_retry,
            all_retry: if arriving { keyframe_retry.max(MAX_INTERNET_WAIT) } else { keyframe_retry },
        }
    }

    /// How long a frame of `len` bytes (of the full-frame stream if `full`) may take to come in
    /// whole, from its first packet.
    fn partial_for(&self, full: bool, len: u32) -> Duration {
        let base = if full { self.full_partial } else { self.partial };
        match self.receive_bps {
            None => base,
            Some(bps) => (base + Duration::from_secs_f64(f64::from(len) * 8.0 / bps)).min(MAX_INTERNET_WAIT),
        }
    }

    /// Whether a frame of `reassembler`'s (a tile's, or the full frame's if `full`) has been
    /// missing packets for too long. Over the internet, only once none is still coming in: the
    /// newest may be the keyframe that repairs the tile, and once it completes (or is given up on
    /// too) frames skipped before it show anyway.
    fn stale(&self, reassembler: &Reassembler, full: bool, now: Instant) -> bool {
        match self.receive_bps {
            None => reassembler.has_stale_partial(if full { self.full_partial } else { self.partial }),
            Some(_) => reassembler.partials().next().is_some() && !self.coming_in(reassembler, full, now),
        }
    }

    /// Over the internet, whether a frame of `reassembler`'s is still coming in, within its time
    /// (never on the local network).
    fn coming_in(&self, reassembler: &Reassembler, full: bool, now: Instant) -> bool {
        self.receive_bps.is_some() && reassembler.partials().any(|(since, len)| now.duration_since(since) <= self.partial_for(full, len))
    }
}

/// Asks the host for every tile again when the view lost its picture: at most every
/// [`KEYFRAME_RETRY`] (see [`Timeouts`]), so a view that keeps failing doesn't turn the stream
/// into keyframes.
#[derive(Default)]
struct CanvasRepair {
    pending: bool,
    asked: Option<Instant>,
}

impl CanvasRepair {
    /// `lost`: the view lost (part of) its picture since the last call. Returns whether to ask
    /// for every tile now, at most every `retry`.
    fn due(&mut self, lost: bool, now: Instant, retry: Duration) -> bool {
        self.pending |= lost;
        if !self.pending || self.asked.is_some_and(|at| now.duration_since(at) < retry) {
            return false;
        }
        self.pending = false;
        self.asked = Some(now);
        true
    }
}

/// Whether a frame's tile is the one its datagrams named and lies inside its stream, where the
/// renderer will copy it.
fn tile_fits(video: &VideoFrame, t: u8) -> bool {
    let tile = &video.tile;
    tile.index == t
        && tile.width > 0
        && tile.height > 0
        && tile.x.checked_add(tile.width).is_some_and(|right| right <= video.width)
        && tile.y.checked_add(tile.height).is_some_and(|bottom| bottom <= video.height)
}

/// The tiles in a mask, lowest first.
fn tiles_in(mask: u64) -> impl Iterator<Item = u8> {
    (0..MAX_TILES as u8).filter(move |t| mask & (1u64 << t) != 0)
}

/// Tiles an update sent that never arrived. One with packets here from before `cutoff` (when
/// the frames after the missing one started) lost the rest: the host sends an update only once
/// the one before is out, and datagrams keep their order on a LAN; its decoder missed a frame, so
/// it needs a keyframe now. Otherwise the frame may have been dropped by the host's encoder (its
/// decoder is fine, and the host sends it again), or still be on its way: a suspect until its next
/// frame says which, or [`SUSPECT_TIMEOUT`] passes (a frame still coming in is also watched by
/// the stale-partial check). No `cutoff`: no packet counts as the missing frame's.
fn suspect(
    missing: u64,
    suspects: &mut [Option<Instant>; MAX_TILES],
    needs: &mut KeyframeNeeds,
    tiles: &[TileState],
    cutoff: Option<Instant>,
    now: Instant,
) {
    for t in tiles_in(missing) {
        if !needs.wanted(t) {
            continue;
        }
        // Part of the stream even if no frame of it ever arrived.
        needs.seen(t);
        let partial = tiles[usize::from(t)].reassembler.oldest_partial();
        if partial.zip(cutoff).is_some_and(|(since, cutoff)| since < cutoff) {
            needs.lost(t);
        } else {
            suspects[usize::from(t)].get_or_insert(now);
        }
    }
}

/// Sends a keyframe request for the tiles that need one now, if any, and forgets their partial
/// frames (the keyframe replaces them). Over the internet, not while one is still coming in (see
/// [`Timeouts::coming_in`]): it may be the keyframe asked for last time, which is taking longer to
/// come than asking again does; forgetting it would only have the host send it again, and again.
fn request_keyframes(
    needs: &mut KeyframeNeeds,
    tiles: &mut [TileState],
    timeouts: &Timeouts,
    shared: &Shared,
    ctl: &mpsc::UnboundedSender<ClientMsg>,
) {
    let now = Instant::now();
    let Some((msg, asked)) = needs.due(now) else { return };
    for t in tiles_in(asked) {
        let reassembler = &mut tiles[usize::from(t)].reassembler;
        if !timeouts.coming_in(reassembler, t == FULL_FRAME_TILE, now) {
            reassembler.clear_partial();
        }
    }
    send_keyframe_request(shared, ctl, msg);
}

fn send_keyframe_request(shared: &Shared, ctl: &mpsc::UnboundedSender<ClientMsg>, msg: ClientMsg) {
    shared.stats.lock().unwrap().keyframe_requests += 1;
    tracing::debug!(?msg, "keyframe request");
    let _ = ctl.send(msg);
}

/// Which tiles wait for a keyframe, and when to ask (again) for each.
struct KeyframeNeeds {
    /// Until every part of the picture has had a keyframe (of its tile, or of the full frame),
    /// ask for all tiles: a tile whose first keyframe never arrived at all is otherwise unknown
    /// here, and its part of the picture would stay black.
    all: bool,
    all_asked: Instant,
    /// Pixels in the stream, and how many of them tiles that had a keyframe cover (tiles of a
    /// layout don't overlap).
    area: u64,
    covered: u64,
    /// Tiles that had a keyframe since the stream started.
    started: u64,
    /// Tiles that had any frame of the stream (or that an update said it sent): only they are
    /// asked for one at a time. (Before that, the request for every tile covers them; after, a
    /// tile never seen isn't part of the stream, e.g. a leftover of a bigger picture before.)
    seen: u64,
    need: u64,
    /// Tiles whose keyframes this Mac can't decode: asked for less often.
    undecodable: u64,
    asked: [Option<Instant>; MAX_TILES],
    /// The full-frame stream doesn't decode here, and the host was told to stop using it.
    full_off: bool,
    /// How often to ask again: [`KEYFRAME_RETRY`], longer over the internet (see [`Timeouts`]).
    retry: Duration,
    /// The same for every tile while `all`: over the internet, longer still while video keeps
    /// coming in.
    retry_all: Duration,
}

impl KeyframeNeeds {
    /// A new stream of size `stream` (None: none yet): its first keyframes are on their way, so
    /// ask only if they don't arrive.
    fn new(now: Instant, stream: Option<(u32, u32)>) -> Self {
        Self {
            all: true,
            all_asked: now,
            area: stream.map_or(u64::MAX, |(w, h)| u64::from(w) * u64::from(h)),
            covered: 0,
            started: 0,
            seen: 0,
            need: 0,
            undecodable: 0,
            asked: [None; MAX_TILES],
            full_off: false,
            retry: KEYFRAME_RETRY,
            retry_all: KEYFRAME_RETRY,
        }
    }

    /// Asks (again) at the pace of `timeouts`.
    fn pace(&mut self, timeouts: &Timeouts) {
        self.retry = timeouts.keyframe_retry;
        self.retry_all = timeouts.all_retry;
    }

    /// The tiles that had a frame of this stream (see `seen`).
    fn seen_mask(&self) -> u64 {
        self.seen
    }

    /// Whether tile `t` is one this Mac shows (not the full frame once it's off).
    fn wanted(&self, t: u8) -> bool {
        !(self.full_off && t == FULL_FRAME_TILE)
    }

    /// The full-frame stream doesn't decode here: never wait or ask for it again.
    fn drop_full(&mut self) {
        self.full_off = true;
        let bit = 1u64 << FULL_FRAME_TILE;
        self.need &= !bit;
        self.seen &= !bit;
        self.undecodable &= !bit;
    }

    fn seen(&mut self, t: u8) {
        if self.wanted(t) {
            self.seen |= 1u64 << t;
        }
    }

    fn is_seen(&self, t: u8) -> bool {
        self.seen & (1u64 << t) != 0
    }

    /// Tile `t` needs a keyframe: asked for at the usual pace.
    fn mark(&mut self, t: u8) {
        if self.wanted(t) {
            self.need |= 1u64 << t;
        }
    }

    /// Tile `t` lost a frame (again). Like [`Self::mark`], and the full frame, asked for once
    /// per loss (see [`Self::due`]), is asked for again.
    fn lost(&mut self, t: u8) {
        if !self.wanted(t) {
            return;
        }
        self.mark(t);
        if t == FULL_FRAME_TILE {
            self.asked[usize::from(t)] = None;
        }
    }

    /// A frame of tile `t` came while it waits for a keyframe. Full frames keep coming without
    /// one long after it was asked for: the host didn't get the request, so ask again.
    fn frame_while_needed(&mut self, t: u8, now: Instant) {
        let retry = if self.undecodable & (1u64 << t) != 0 { UNDECODABLE_RETRY } else { self.retry };
        if t == FULL_FRAME_TILE && self.asked[usize::from(t)].is_some_and(|at| now.duration_since(at) >= retry) {
            self.asked[usize::from(t)] = None;
        }
    }

    fn needs(&self, t: u8) -> bool {
        self.need & (1u64 << t) != 0
    }

    /// An undecodable keyframe just arrived: asking again at once would only bring the same one.
    fn set_undecodable(&mut self, t: u8, undecodable: bool, now: Instant) {
        if undecodable {
            self.undecodable |= 1u64 << t;
            self.asked[usize::from(t)] = Some(now);
        } else {
            self.undecodable &= !(1u64 << t);
        }
    }

    /// `tile` got a keyframe it decodes.
    fn got_keyframe(&mut self, tile: &TileRect) {
        let bit = tile.bit();
        self.need &= !bit;
        self.asked[usize::from(tile.index)] = None;
        self.seen |= bit;
        if self.started & bit == 0 {
            self.started |= bit;
            let area = if tile.index == FULL_FRAME_TILE { self.area } else { u64::from(tile.width) * u64::from(tile.height) };
            self.covered = self.covered.saturating_add(area);
        }
        if self.covered >= self.area {
            self.all = false;
        }
    }

    /// How often to ask for every tile: rarely if all that is known to be missing is tiles this
    /// Mac can't decode. (The full frame doesn't count: tiles may decode where it doesn't.)
    fn all_retry(&self) -> Duration {
        let waiting = self.seen & !self.started & !(1u64 << FULL_FRAME_TILE);
        if waiting != 0 && waiting & !self.undecodable == 0 { UNDECODABLE_RETRY } else { self.retry_all }
    }

    /// The request to send now, if any, with the tiles it asks for: each tile at most every
    /// [`KEYFRAME_RETRY`] (or [`UNDECODABLE_RETRY`]), so a lost keyframe is asked for again but
    /// the host isn't flooded while one is on its way.
    ///
    /// Except the full frame: the host answers by sending every tile again at once and making its
    /// next full frame a keyframe, which may not come for long (a still screen). Asking again
    /// meanwhile would only send every tile again, so it is asked for once per loss.
    fn due(&mut self, now: Instant) -> Option<(ClientMsg, u64)> {
        let mut tiles = 0u64;
        for t in 0..MAX_TILES {
            let bit = 1u64 << t;
            if self.need & self.seen & bit == 0 || (t == usize::from(FULL_FRAME_TILE) && self.asked[t].is_some()) {
                continue;
            }
            let retry = if self.undecodable & bit != 0 { UNDECODABLE_RETRY } else { self.retry };
            if self.asked[t].is_none_or(|at| now.duration_since(at) >= retry) {
                tiles |= bit;
            }
        }
        let all = self.all && now.duration_since(self.all_asked) >= self.all_retry();
        if all {
            self.all_asked = now;
            tiles = self.need;
        }
        for (t, asked) in self.asked.iter_mut().enumerate() {
            if tiles & (1u64 << t) != 0 {
                *asked = Some(now);
            }
        }
        match (all, tiles) {
            (true, _) => Some((ClientMsg::RequestKeyframe, tiles)),
            (false, 0) => None,
            (false, tiles) => Some((ClientMsg::RequestKeyframes { tiles }, tiles)),
        }
    }
}

/// Frames of a size other than the stream's that aren't keyframes. Right after a switch they are
/// stragglers of the picture before and stop at once; if they keep coming, they are the next
/// picture and its keyframes were lost, and without one nothing of it can ever be shown.
#[derive(Default)]
struct OtherSize {
    /// Their size, and since when they come.
    since: Option<((u32, u32), Instant)>,
    asked: Option<Instant>,
}

impl OtherSize {
    /// A frame of `size` came (after `lost_before`: its tile lost frames before it, maybe its
    /// keyframe). Returns whether to ask for keyframes of every tile now (at most every `retry`).
    fn frame(&mut self, size: (u32, u32), lost_before: bool, now: Instant, retry: Duration) -> bool {
        let since = match self.since {
            Some((s, at)) if s == size => at,
            _ => self.since.insert((size, now)).1,
        };
        let lasting = lost_before || now.duration_since(since) >= retry;
        let due = lasting && self.asked.is_none_or(|at| now.duration_since(at) >= retry);
        if due {
            self.asked = Some(now);
        }
        due
    }
}

/// Wrapping-aware "update a comes after b".
fn is_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// The newest update while its tiles come in.
struct PendingUpdate {
    /// The tiles it sent, and those that arrived.
    mask: u64,
    got: u64,
    bytes: usize,
    since: Instant,
}

/// Tiles of updates that didn't come in whole.
#[derive(Debug, Default, PartialEq)]
struct Missing {
    /// Tiles they sent that never came.
    tiles: u64,
    /// More updates than one were lost entirely: which tiles they had is unknown.
    unknown: bool,
}

/// Follows updates (captured frames) by their masks. Counts updates, not tile frames, for the
/// fps and Mbit/s readouts: an update is reported once, with the bytes of all its tiles, when
/// they are all in, when a newer update starts, or after [`UPDATE_TIMEOUT`]. And finds tile
/// frames that never came: the host sends updates in order, so tiles of an update still missing
/// once a newer one arrives are missing for good, even if no datagram of them came; and update
/// numbers count up by one, so an update lost entirely shows as a gap (its tiles are in the next
/// one's `previous_mask`).
#[derive(Default)]
struct UpdateMeter {
    /// Newest update seen.
    newest: Option<u32>,
    /// It, while tiles of it are still to come.
    pending: Option<PendingUpdate>,
    /// Bytes of late tiles, reported with the next update.
    carry: usize,
}

impl UpdateMeter {
    /// A frame of tile `bit` arrived, of `update` that sent the tiles in `mask` (and the update
    /// before it those in `previous`). Returns what the updates before never delivered, if this
    /// one ends them.
    fn add(&mut self, update: u32, mask: u64, previous: u64, bit: u64, bytes: usize, now: Instant, mut report: impl FnMut(usize)) -> Missing {
        let mut lost = Missing::default();
        match self.newest {
            Some(newest) if update == newest => match &mut self.pending {
                Some(p) => {
                    p.bytes += bytes;
                    p.got |= bit;
                    p.mask |= mask;
                }
                None => self.carry += bytes,
            },
            Some(newest) if !is_newer(update, newest) => match &mut self.pending {
                Some(p) => p.bytes += bytes,
                None => self.carry += bytes,
            },
            _ => {
                if let Some(p) = self.pending.take() {
                    report(p.bytes);
                    lost.tiles = p.mask & !p.got;
                    // The update right after it says what it really sent: its frames named what it
                    // was to encode, before the host's encoder dropped any.
                    if self.newest.is_some_and(|n| update.wrapping_sub(n) == 1) && previous != 0 {
                        lost.tiles &= previous;
                    }
                }
                if let Some(newest) = self.newest {
                    lost = Self::skipped(lost, newest, update, previous);
                }
                self.newest = Some(update);
                self.pending = Some(PendingUpdate { mask: mask | bit, got: bit, bytes: std::mem::take(&mut self.carry) + bytes, since: now });
            }
        }
        if let Some(p) = &self.pending
            && p.got & p.mask == p.mask
        {
            report(p.bytes);
            self.pending = None;
        }
        lost
    }

    /// Reports what is pending and starts over (a new stream counts its updates afresh; what the
    /// old one lost no longer matters).
    fn flush(&mut self, mut report: impl FnMut(usize)) {
        if let Some(p) = self.pending.take() {
            report(p.bytes);
        }
        *self = Self::default();
    }

    /// What updates after `newest` and before `update` sent: none if they're next to each other;
    /// else `previous` (the tiles of the one just before `update`), and if more than one is
    /// missing, unknown tiles too.
    fn skipped(mut lost: Missing, newest: u32, update: u32, previous: u64) -> Missing {
        let gap = update.wrapping_sub(newest);
        if gap >= 2 {
            lost.tiles |= previous;
            lost.unknown |= gap > 2;
        }
        lost
    }

    /// The host says the screen went still after `update`, which sent the tiles in `mask`:
    /// whatever of it (or of updates before it) isn't here won't come any more. It counts as
    /// seen from now on, so nothing is reported twice.
    fn idle(&mut self, update: u32, mask: u64, mut report: impl FnMut(usize)) -> Missing {
        let Some(newest) = self.newest else {
            // Nothing of this stream came yet: the request for every tile covers it.
            return Missing::default();
        };
        if update != newest && !is_newer(update, newest) {
            return Missing::default(); // already past it
        }
        let mut missing = Missing::default();
        if let Some(p) = self.pending.take() {
            report(p.bytes);
            // The note names what really went out of it (the host's encoder may have dropped some).
            let sent = if update == newest { mask } else { u64::MAX };
            missing.tiles = p.mask & sent & !p.got;
        }
        if update != newest {
            // `update` came not at all, and what came between it and `newest` is unknown.
            missing.tiles |= mask;
            missing.unknown = update.wrapping_sub(newest) > 1;
            self.newest = Some(update);
        }
        missing
    }

    /// Reports an update whose missing tiles are too late to wait for (after `timeout`, see
    /// [`UPDATE_TIMEOUT`]); returns them.
    fn flush_stale(&mut self, now: Instant, timeout: Duration, mut report: impl FnMut(usize)) -> u64 {
        match self.pending.take_if(|p| now.duration_since(p.since) >= timeout) {
            Some(p) => {
                report(p.bytes);
                p.mask & !p.got
            }
            None => 0,
        }
    }
}

/// Notes a tile frame's arrival in the stats; returns the tiles lost whole before it (see
/// [`UpdateMeter::add`]).
fn record_arrival(shared: &Shared, meter: &mut UpdateMeter, video: &VideoFrame, bytes: usize, received_us: u64, now: Instant) -> Missing {
    let mut stats = shared.stats.lock().unwrap();
    let lost = meter.add(video.update, video.update_mask, video.previous_mask, video.tile.bit(), bytes, now, |sum| stats.on_frame_received(sum));
    stats.capture.add(video.encode_start_us.saturating_sub(video.capture_time_us) as f64);
    stats.encode.add(video.encoded_time_us.saturating_sub(video.encode_start_us) as f64);
    if let Some(encoded_local) = stats.clock.to_local(video.encoded_time_us) {
        stats.network.add(received_us.saturating_sub(encoded_local) as f64);
    }
    lost
}

/// When a warning repeated by every frame was last logged (µs, [`clock`]).
static WRONG_SIZE_WARNED_US: AtomicU64 = AtomicU64::new(0);

/// Whether a warning that may repeat with every frame can be logged now: once a second at most.
fn warn_now(last_us: &AtomicU64) -> bool {
    let now = clock::now_us();
    let last = last_us.load(Ordering::Relaxed);
    (last == 0 || now.saturating_sub(last) >= 1_000_000)
        && last_us.compare_exchange(last, now.max(1), Ordering::Relaxed, Ordering::Relaxed).is_ok()
}

/// A decoder for one tile. Its callback (on a VideoToolbox thread) stamps the decode time and
/// hands the tile to the view, or reports a failure back to [`receive_video`].
fn new_decoder(
    video: &VideoFrame,
    shared: &Arc<Shared>,
    events: &SessionEvents,
    failed: &mpsc::UnboundedSender<DecodeFailed>,
) -> Result<Decoder<TileMeta>> {
    let (shared, events, failed) = (shared.clone(), events.clone(), failed.clone());
    Decoder::new(video.codec, &video.param_sets, video.nal_length_size, move |decoded: DecodedFrame<TileMeta>| {
        let DecodedFrame { image, tag: meta } = decoded;
        let fail = |wrong_size| {
            let _ = failed.send(DecodeFailed { tile: meta.tile.index, update: meta.update, seq: meta.seq, keyframe: meta.keyframe, wrong_size });
        };
        let Ok(pixel_buffer) = image else { return fail(false) };
        // The renderer copies the picture into the tile's place: one of another size would land
        // outside it, or leave part of it stale.
        let size = (CVPixelBufferGetWidth(&pixel_buffer), CVPixelBufferGetHeight(&pixel_buffer));
        if size != (meta.tile.width as usize, meta.tile.height as usize) {
            if warn_now(&WRONG_SIZE_WARNED_US) {
                tracing::warn!(tile = ?meta.tile, ?size, "decoded picture isn't its tile's size");
            }
            return fail(true);
        }
        let mut timing = meta.timing;
        timing.decoded_us = clock::now_us();
        {
            let mut stats = shared.stats.lock().unwrap();
            stats.decode.add(timing.decoded_us.saturating_sub(timing.received_us) as f64);
            stats.frames_decoded += 1;
        }
        if let Some(probe) = shared.frame_probe.read().unwrap().as_ref() {
            probe(&ProbeFrame {
                pixel_buffer: &pixel_buffer,
                tile: meta.tile,
                stream: meta.stream,
                update: meta.update,
                update_mask: meta.update_mask,
                timing,
            });
        }
        shared.slot.publish_tile(TileImage {
            pixel_buffer,
            tile: meta.tile,
            stream: meta.stream,
            update: meta.update,
            update_mask: meta.update_mask,
            cover: Vec::new(),
            timing,
        });
        flush_display(&shared, &events, Some(meta.stream));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requested(due: Option<(ClientMsg, u64)>) -> Option<u64> {
        match due {
            Some((ClientMsg::RequestKeyframes { tiles }, asked)) => {
                assert_eq!(tiles, asked);
                Some(tiles)
            }
            Some((ClientMsg::RequestKeyframe, _)) => Some(u64::MAX),
            Some((other, _)) => panic!("unexpected {other:?}"),
            None => None,
        }
    }

    /// Tile `index` of a 200×400 stream cut into four rows of 200×100.
    fn row(index: u8) -> TileRect {
        TileRect { index, x: 0, y: u32::from(index) * 100, width: 200, height: 100 }
    }

    const STREAM: (u32, u32) = (200, 400);
    const FULL: TileRect = TileRect { index: FULL_FRAME_TILE, x: 0, y: 0, width: 200, height: 400 };

    #[test]
    fn new_stream_asks_for_everything_only_if_keyframes_are_late() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        assert_eq!(requested(needs.due(t0)), None, "the first keyframes are on their way");
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY)), Some(u64::MAX));
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY * 3 / 2)), None, "paced");
        // Three of the four rows arrive: still asking for all of them (the fourth is unknown).
        for t in 0..3 {
            needs.got_keyframe(&row(t));
        }
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY * 2)), Some(u64::MAX));
        needs.got_keyframe(&row(3));
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY * 10)), None, "the whole picture started");
    }

    #[test]
    fn a_few_forced_tiles_dont_end_asking_for_everything() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        // The first frames of the stream to arrive are an update of only two tiles (keyframes
        // the host was asked for, say): two rows of four are still unknown.
        needs.got_keyframe(&row(1));
        needs.got_keyframe(&row(2));
        needs.got_keyframe(&row(2));
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY)), Some(u64::MAX));
    }

    #[test]
    fn a_full_frame_starts_the_whole_picture_but_isnt_waited_for() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        needs.got_keyframe(&FULL);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY * 5)), None);
        // Tiles alone cover it too: the full-frame stream never has to start.
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        for t in 0..4 {
            needs.got_keyframe(&row(t));
        }
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY * 5)), None);
        // Once started, a lost full frame is asked for like any tile.
        needs.seen(FULL_FRAME_TILE);
        needs.mark(FULL_FRAME_TILE);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY * 5)), Some(1 << 63));
    }

    #[test]
    fn undecodable_from_the_start_is_asked_for_rarely() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        // Every keyframe of the stream arrives and can't be decoded.
        for t in 0..4 {
            needs.seen(t);
            needs.set_undecodable(t, true, t0);
            needs.mark(t);
        }
        for ms in (0..UNDECODABLE_RETRY.as_millis() as u64).step_by(20) {
            assert_eq!(requested(needs.due(t0 + Duration::from_millis(ms))), None, "at {ms} ms");
        }
        assert_eq!(requested(needs.due(t0 + UNDECODABLE_RETRY)), Some(u64::MAX));
        assert_eq!(requested(needs.due(t0 + UNDECODABLE_RETRY + KEYFRAME_RETRY)), None);
    }

    #[test]
    fn an_undecodable_full_frame_doesnt_slow_asking_for_tiles() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        // The full frame is too big for this Mac's decoder; its tiles may not be.
        needs.seen(FULL_FRAME_TILE);
        needs.set_undecodable(FULL_FRAME_TILE, true, t0);
        needs.mark(FULL_FRAME_TILE);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY / 2)), None);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY)), Some(u64::MAX));
        // A decodable tile still missing next to an undecodable one: asked for at the usual pace.
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        needs.seen(0);
        needs.set_undecodable(0, true, t0);
        needs.mark(0);
        needs.seen(1);
        needs.mark(1);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY)), Some(u64::MAX));
    }

    #[test]
    fn lost_tiles_are_asked_for_together_and_paced_per_tile() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        for t in 0..4 {
            needs.got_keyframe(&row(t));
        }
        needs.mark(1);
        needs.mark(3);
        assert!(needs.needs(1) && needs.needs(3) && !needs.needs(2));
        assert_eq!(requested(needs.due(t0)), Some(1 << 1 | 1 << 3));
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY / 2)), None);
        // Another tile breaks meanwhile: asked for at once, without repeating the others.
        needs.lost(2);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY / 2)), Some(1 << 2));
        needs.got_keyframe(&row(1));
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY)), Some(1 << 3), "1 is repaired, 2 not due yet");
        // A repaired tile that breaks again is asked for right away.
        needs.mark(1);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY)), Some(1 << 1));
    }

    #[test]
    fn the_full_frame_is_asked_for_once_per_loss() {
        let t0 = Instant::now();
        let ms = |ms| t0 + Duration::from_millis(ms);
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        needs.got_keyframe(&FULL);
        needs.lost(FULL_FRAME_TILE);
        assert_eq!(requested(needs.due(ms(0))), Some(1 << 63));
        // The host sent every tile again; its next full frame is a keyframe, whenever that is.
        assert_eq!(requested(needs.due(ms(10_000))), None);
        // A stale partial of it again: lost again, asked again.
        needs.lost(FULL_FRAME_TILE);
        assert_eq!(requested(needs.due(ms(10_010))), Some(1 << 63));
        // Full frames that aren't keyframes keep coming: the request went missing.
        needs.frame_while_needed(FULL_FRAME_TILE, ms(10_100));
        assert_eq!(requested(needs.due(ms(10_100))), None, "maybe sent before the host had it");
        needs.frame_while_needed(FULL_FRAME_TILE, ms(10_010) + KEYFRAME_RETRY);
        assert_eq!(requested(needs.due(ms(10_010) + KEYFRAME_RETRY)), Some(1 << 63));
        // Tiles go on at their usual pace meanwhile.
        needs.seen(1);
        needs.lost(1);
        assert_eq!(requested(needs.due(ms(20_000))), Some(1 << 1));
        assert_eq!(requested(needs.due(ms(20_000) + KEYFRAME_RETRY)), Some(1 << 1));
        needs.got_keyframe(&FULL);
        needs.got_keyframe(&row(1));
        assert_eq!(requested(needs.due(ms(30_000))), None);
    }

    #[test]
    fn tiles_outside_the_stream_are_never_asked_for() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        needs.got_keyframe(&FULL);
        // Tile 9 isn't part of this stream (a stale partial of the picture before, say).
        needs.mark(9);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY * 5)), None);
        // Until a frame of it shows up after all.
        needs.seen(9);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY * 5)), Some(1 << 9));
    }

    #[test]
    fn undecodable_tiles_are_asked_for_rarely() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        needs.got_keyframe(&FULL);
        needs.seen(1);
        needs.set_undecodable(1, true, t0);
        needs.mark(1);
        assert_eq!(requested(needs.due(t0)), None, "the keyframe that just came is the answer");
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY * 2)), None);
        assert_eq!(requested(needs.due(t0 + UNDECODABLE_RETRY)), Some(1 << 1));
        needs.set_undecodable(1, false, t0);
        assert_eq!(requested(needs.due(t0 + UNDECODABLE_RETRY + KEYFRAME_RETRY)), Some(1 << 1));
    }

    #[test]
    fn frames_of_another_size_ask_for_keyframes_only_if_they_keep_coming() {
        let t0 = Instant::now();
        let ms = |ms| t0 + Duration::from_millis(ms);
        let mut other = OtherSize::default();
        // Stragglers of the picture before: a few, for a moment.
        assert!(!other.frame((640, 480), false, ms(0), KEYFRAME_RETRY));
        assert!(!other.frame((640, 480), false, ms(30), KEYFRAME_RETRY));
        // The next picture's P-frames keep coming: its keyframes were lost.
        assert!(!other.frame((800, 600), false, ms(100), KEYFRAME_RETRY));
        assert!(!other.frame((800, 600), false, ms(250), KEYFRAME_RETRY));
        assert!(other.frame((800, 600), false, ms(300), KEYFRAME_RETRY));
        assert!(!other.frame((800, 600), false, ms(400), KEYFRAME_RETRY), "paced");
        assert!(other.frame((800, 600), false, ms(500), KEYFRAME_RETRY));
        // A frame whose tile lost frames before it asks at once (paced all the same).
        let mut other = OtherSize::default();
        assert!(other.frame((800, 600), true, ms(0), KEYFRAME_RETRY));
        assert!(!other.frame((800, 600), true, ms(100), KEYFRAME_RETRY));
        assert!(other.frame((800, 600), true, ms(200), KEYFRAME_RETRY));
    }

    /// Runs `steps` of (update, mask, tile, bytes) through a meter: the bytes reported, and the
    /// tiles found lost.
    fn meter_run(steps: &[(u32, u64, u8, usize)]) -> (Vec<usize>, u64) {
        let mut meter = UpdateMeter::default();
        let (mut out, mut lost) = (Vec::new(), 0);
        let now = Instant::now();
        for &(update, mask, tile, bytes) in steps {
            lost |= meter.add(update, mask, 0, 1 << tile, bytes, now, |b| out.push(b)).tiles;
        }
        (out, lost)
    }

    #[test]
    fn meter_counts_updates_not_tiles() {
        let reports = |steps: &[(u32, u64, u8, usize)]| meter_run(steps).0;
        // Three tiles of update 7, then a one-tile update 8.
        assert_eq!(reports(&[(7, 0b111, 0, 100), (7, 0b111, 1, 200), (7, 0b111, 2, 300), (8, 0b1, 0, 50)]), vec![600, 50]);
        // Update 7 lost a tile: reported when 8 starts.
        assert_eq!(reports(&[(7, 0b111, 0, 100), (7, 0b111, 2, 200), (8, 0b11, 0, 50), (8, 0b11, 1, 50)]), vec![300, 100]);
        // A late tile of update 7 counts with the update in progress.
        assert_eq!(reports(&[(7, 0b11, 0, 100), (8, 0b11, 0, 10), (7, 0b11, 1, 100), (8, 0b11, 1, 10)]), vec![100, 120]);
        // ...or, when none is, with the next one.
        assert_eq!(reports(&[(8, 0b1, 0, 10), (7, 0b11, 1, 100), (9, 0b1, 0, 1)]), vec![10, 101]);
        // Update numbers wrap.
        assert_eq!(reports(&[(u32::MAX, 0b11, 0, 1), (0, 0b1, 0, 2), (u32::MAX, 0b11, 1, 4), (1, 0b1, 0, 8)]), vec![1, 2, 12]);
        // Completion goes by which tiles arrived, not how many: the full frame alone is a whole update.
        assert_eq!(reports(&[(3, 1 << 63, 63, 500), (4, 0b101, 2, 5), (4, 0b101, 2, 5)]), vec![500]);
    }

    #[test]
    fn meter_finds_tiles_lost_whole() {
        // Tile 2 of update 7 never came: found when update 8 starts.
        assert_eq!(meter_run(&[(7, 0b111, 0, 1), (7, 0b111, 1, 1), (8, 0b1, 0, 1)]).1, 0b100);
        // Nothing is lost when every tile came, whatever their order.
        assert_eq!(meter_run(&[(7, 0b110, 2, 1), (7, 0b110, 1, 1), (8, 0b1, 0, 1)]).1, 0);
        // A lost full frame.
        assert_eq!(meter_run(&[(7, 1 << 63, 0, 1), (8, 0b1, 0, 1)]).1, 1 << 63);
        // A late tile of an older update finds nothing lost.
        assert_eq!(meter_run(&[(7, 0b11, 0, 1), (7, 0b11, 1, 1), (8, 0b1, 0, 1), (6, 0b11, 1, 1)]).1, 0);
    }

    #[test]
    fn meter_reports_incomplete_update_after_timeout() {
        let mut meter = UpdateMeter::default();
        let mut out = Vec::new();
        let t0 = Instant::now();
        let none = Missing::default();
        assert_eq!(meter.add(1, 0b1111, 0, 1 << 0, 100, t0, |b| out.push(b)), none);
        assert_eq!(meter.add(1, 0b1111, 0, 1 << 3, 100, t0, |b| out.push(b)), none);
        assert_eq!(meter.flush_stale(t0 + UPDATE_TIMEOUT / 2, UPDATE_TIMEOUT, |b| out.push(b)), 0);
        assert!(out.is_empty());
        assert_eq!(meter.flush_stale(t0 + UPDATE_TIMEOUT, UPDATE_TIMEOUT, |b| out.push(b)), 0b0110, "tiles 1 and 2 never came");
        assert_eq!(out, vec![200]);
        assert_eq!(meter.flush_stale(t0 + UPDATE_TIMEOUT * 2, UPDATE_TIMEOUT, |b| out.push(b)), 0, "found once");
        // Its tile shows up after all: bytes go with the next update, not as a frame.
        assert_eq!(meter.add(1, 0b1111, 0, 1 << 1, 7, t0, |b| out.push(b)), none);
        assert_eq!(meter.add(2, 0b1, 0b1111, 1 << 0, 1, t0, |b| out.push(b)), none, "update 1 was already reported");
        assert_eq!(out, vec![200, 8]);
    }

    #[test]
    fn a_new_stream_forgets_what_the_old_one_lost() {
        let mut meter = UpdateMeter::default();
        let now = Instant::now();
        meter.add(5, 0b11, 0, 0b01, 1, now, |_| {});
        meter.flush(|_| {});
        assert_eq!(meter.add(9, 0b1, 0b10, 0b1, 1, now, |_| {}), Missing::default(), "no gap from the old stream's numbers");
    }

    #[test]
    fn lost_tiles_still_arriving_are_left_to_their_reassembler() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        needs.got_keyframe(&FULL);
        let mut tiles: Vec<TileState> = (0..MAX_TILES).map(|_| TileState::default()).collect();
        // Tile 2's frame has some of its packets here; tile 3's none.
        let packets = transport::video::Packetizer::for_tile(2).packetize(&[7; 5000], 1200).unwrap();
        assert!(tiles[2].reassembler.push(&packets[0]).is_none());
        std::thread::sleep(Duration::from_millis(1));
        let mut suspects = [None; MAX_TILES];
        // Packets of tile 2 that came before the next update's frames started: the rest were
        // lost. A keyframe now. Nothing of tile 3 came: maybe the host's encoder dropped it.
        let later = Instant::now();
        suspect(1 << 2 | 1 << 3, &mut suspects, &mut needs, &tiles, Some(later), t0);
        assert!(needs.needs(2) && !needs.needs(3));
        assert!(suspects[2].is_none() && suspects[3] == Some(t0));
        assert_eq!(requested(needs.due(t0)), Some(1 << 2), "never seen before, but the update said it was sent");
        // Tile 3 stays a suspect, first noted when it was.
        suspect(1 << 3, &mut suspects, &mut needs, &tiles, Some(later), t0 + SUSPECT_TIMEOUT);
        assert_eq!(suspects[3], Some(t0));
    }

    /// A frame of a missing tile that started arriving only after the next update's frames did
    /// may be the host's resend of it (its encoder dropped the missing one): a suspect, not a loss.
    #[test]
    fn a_resend_still_arriving_is_not_taken_for_a_loss() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        needs.got_keyframe(&FULL);
        let mut tiles: Vec<TileState> = (0..MAX_TILES).map(|_| TileState::default()).collect();
        let cutoff = Instant::now();
        std::thread::sleep(Duration::from_millis(1));
        let packets = transport::video::Packetizer::for_tile(2).packetize(&[7; 5000], 1200).unwrap();
        assert!(tiles[2].reassembler.push(&packets[0]).is_none());
        let mut suspects = [None; MAX_TILES];
        suspect(1 << 2, &mut suspects, &mut needs, &tiles, Some(cutoff), t0);
        assert!(!needs.needs(2) && suspects[2] == Some(t0));
        // Without a cutoff (the screen went still), packets here never count as the missing frame.
        let mut suspects = [None; MAX_TILES];
        suspect(1 << 2, &mut suspects, &mut needs, &tiles, None, t0);
        assert!(!needs.needs(2) && suspects[2] == Some(t0));
    }

    #[test]
    fn a_gap_in_update_numbers_names_what_was_lost() {
        let now = Instant::now();
        let mut meter = UpdateMeter::default();
        let add = |meter: &mut UpdateMeter, update, mask, previous, tile: u8| meter.add(update, mask, previous, 1 << tile, 1, now, |_| {});
        assert_eq!(add(&mut meter, 4, 0b1, 0, 0), Missing::default());
        // Update 5 (tiles 2 and 3) never came at all: update 6 says what it had.
        assert_eq!(add(&mut meter, 6, 0b1, 0b1100, 0), Missing { tiles: 0b1100, unknown: false });
        // Two updates in a row: what the first had is unknown.
        assert_eq!(add(&mut meter, 9, 0b1, 0b10, 0), Missing { tiles: 0b10, unknown: true });
        // An update's frames name what it was to encode; the next one, what really went out: the
        // tile the host's encoder dropped (and sends again) isn't missing.
        let mut meter = UpdateMeter::default();
        add(&mut meter, 1, 0b11, 0, 0);
        assert_eq!(add(&mut meter, 2, 0b10, 0b01, 1), Missing::default());
        // Numbers wrap.
        let mut meter = UpdateMeter::default();
        add(&mut meter, u32::MAX, 0b1, 0, 0);
        assert_eq!(add(&mut meter, 1, 0b1, 0b100, 0), Missing { tiles: 0b100, unknown: false });
    }

    #[test]
    fn the_host_saying_the_screen_is_still_finds_what_never_came() {
        let now = Instant::now();
        let mut meter = UpdateMeter::default();
        let mut out = Vec::new();
        assert_eq!(meter.idle(3, 0b1, |b| out.push(b)), Missing::default(), "nothing of the stream yet");
        meter.add(3, 0b111, 0, 0b001, 10, now, |b| out.push(b));
        meter.add(3, 0b111, 0, 0b100, 10, now, |b| out.push(b));
        // The last update before the screen went still is 3: tile 1 isn't coming.
        assert_eq!(meter.idle(3, 0b111, |b| out.push(b)), Missing { tiles: 0b010, unknown: false });
        assert_eq!(out, vec![20]);
        assert_eq!(meter.flush_stale(now + UPDATE_TIMEOUT, UPDATE_TIMEOUT, |b| out.push(b)), 0, "not twice");
        // Update 4 (tiles 5 and 6) never came at all.
        assert_eq!(meter.idle(4, 0b110_0000, |_| {}), Missing { tiles: 0b110_0000, unknown: false });
        assert_eq!(meter.idle(4, 0b110_0000, |_| {}), Missing::default(), "said once");
        // Two updates never came: the one before the last is unknown.
        assert_eq!(meter.idle(6, 0b1, |_| {}), Missing { tiles: 0b1, unknown: true });
        // An old note after newer updates: nothing.
        assert_eq!(meter.idle(5, 0b1, |_| {}), Missing::default());
    }

    #[test]
    fn over_the_internet_timeouts_grow_with_the_round_trip() {
        let ms = Duration::from_millis;
        let quiet = Duration::from_secs(1);
        // A short round trip: a little more patience than on the local network.
        let near = Timeouts::internet(ms(10), 0.0, quiet);
        let expected = Timeouts {
            partial: ms(80),
            full_partial: ms(270),
            receive_bps: Some(MIN_RECEIVE_BPS),
            update: ms(170),
            suspect: ms(170),
            keyframe_retry: ms(200),
            all_retry: ms(200),
        };
        assert_eq!(near, expected);
        let far = Timeouts::internet(ms(150), 4e6, quiet);
        let expected = Timeouts {
            partial: ms(360),
            full_partial: ms(550),
            receive_bps: Some(4e6),
            update: ms(450),
            suspect: ms(450),
            keyframe_retry: ms(450),
            all_retry: ms(450),
        };
        assert_eq!(far, expected);
        // Keyframes are asked for again at that pace.
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        needs.pace(&far);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY)), None);
        assert_eq!(requested(needs.due(t0 + far.keyframe_retry)), Some(u64::MAX));
        let mut canvas = CanvasRepair::default();
        assert!(canvas.due(true, t0, far.keyframe_retry));
        assert!(!canvas.due(true, t0 + KEYFRAME_RETRY, far.keyframe_retry));
        // The local network keeps its constants.
        assert_eq!(Timeouts::LAN.partial, PARTIAL_FRAME_TIMEOUT);
        assert_eq!(Timeouts::LAN.keyframe_retry, KEYFRAME_RETRY);
        assert_eq!(Timeouts::LAN.partial_for(false, 1 << 20), PARTIAL_FRAME_TIMEOUT);
    }

    #[test]
    fn over_the_internet_a_frame_gets_the_time_its_size_takes() {
        let ms = Duration::from_millis;
        let close = |a: Duration, b: Duration| a.abs_diff(b) < Duration::from_micros(1);
        // 25 KB at 2 Mbit/s: 100 ms on top of the round trip's allowance.
        let timeouts = Timeouts::internet(ms(10), 2e6, Duration::from_secs(1));
        assert!(close(timeouts.partial_for(false, 25_000), ms(180)));
        assert!(close(timeouts.partial_for(true, 25_000), ms(370)));
        // A still screen received next to nothing: taken as the floor's rate.
        let still = Timeouts::internet(ms(10), 1e3, Duration::from_secs(1));
        assert!(close(still.partial_for(false, 75_000), ms(480)));
        // However slow, given up on within a few seconds.
        assert_eq!(still.partial_for(false, 10_000_000), MAX_INTERNET_WAIT);

        // A keyframe of 100 packets, its first one in.
        let packets = transport::video::Packetizer::for_tile(3).packetize(&vec![7u8; 100 * 1_100], 1_200).unwrap();
        let mut reassembler = Reassembler::new();
        assert!(reassembler.push(&packets[0]).is_none());
        let now = Instant::now();
        assert!(timeouts.coming_in(&reassembler, false, now) && !timeouts.stale(&reassembler, false, now));
        let later = now + timeouts.partial_for(false, 100 * 1_100) + ms(1);
        assert!(!timeouts.coming_in(&reassembler, false, later) && timeouts.stale(&reassembler, false, later));
        // On the local network, a frame is never kept for still coming in.
        assert!(!Timeouts::LAN.coming_in(&reassembler, false, now));
    }

    #[test]
    fn over_the_internet_tiles_still_coming_in_arent_missing() {
        let ms = Duration::from_millis;
        // Video came 50 ms ago: tiles of an update may still be waiting behind it, and the first
        // keyframes of a stream may be among it.
        let arriving = Timeouts::internet(ms(10), 4e6, ms(50));
        assert_eq!((arriving.update, arriving.all_retry), (MAX_INTERNET_WAIT, MAX_INTERNET_WAIT));
        assert_eq!((arriving.suspect, arriving.keyframe_retry), (ms(170), ms(200)), "these wait for nothing queued");
        let t0 = Instant::now();
        let mut meter = UpdateMeter::default();
        meter.add(1, 0b1111, 0, 0b1, 100, t0, |_| {});
        assert_eq!(meter.flush_stale(t0 + ms(500), arriving.update, |_| {}), 0);
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        needs.pace(&arriving);
        assert_eq!(requested(needs.due(t0 + ms(500))), None);
        // Nothing for 170 ms: what still misses isn't coming.
        let quiet = Timeouts::internet(ms(10), 4e6, ms(170));
        assert_eq!(meter.flush_stale(t0 + ms(500), quiet.update, |_| {}), 0b1110);
        needs.pace(&quiet);
        assert_eq!(requested(needs.due(t0 + ms(500))), Some(u64::MAX));
        // Video that keeps coming is waited on for a few seconds at most.
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        needs.pace(&arriving);
        assert_eq!(requested(needs.due(t0 + MAX_INTERNET_WAIT)), Some(u64::MAX));
    }

    #[test]
    fn a_view_that_keeps_losing_its_picture_is_repaired_at_a_pace() {
        let t0 = Instant::now();
        let mut canvas = CanvasRepair::default();
        assert!(!canvas.due(false, t0, KEYFRAME_RETRY));
        assert!(canvas.due(true, t0, KEYFRAME_RETRY));
        assert!(!canvas.due(true, t0 + KEYFRAME_RETRY / 2, KEYFRAME_RETRY), "asked just now");
        // Still lost: asked again once it's time, even if not lost again since.
        assert!(canvas.due(false, t0 + KEYFRAME_RETRY, KEYFRAME_RETRY));
        assert!(!canvas.due(false, t0 + KEYFRAME_RETRY * 3, KEYFRAME_RETRY), "repaired");
    }

    #[test]
    fn a_full_frame_this_mac_cant_decode_is_never_waited_for() {
        let t0 = Instant::now();
        let mut needs = KeyframeNeeds::new(t0, Some(STREAM));
        for i in 0..4 {
            needs.got_keyframe(&row(i));
        }
        needs.lost(FULL_FRAME_TILE);
        needs.drop_full();
        assert!(!needs.needs(FULL_FRAME_TILE));
        needs.lost(FULL_FRAME_TILE);
        needs.mark(FULL_FRAME_TILE);
        needs.seen(FULL_FRAME_TILE);
        assert!(!needs.needs(FULL_FRAME_TILE) && needs.seen_mask() & 1 << FULL_FRAME_TILE == 0);
        assert_eq!(requested(needs.due(t0 + KEYFRAME_RETRY)), None);
    }

    #[test]
    fn warnings_are_rate_limited() {
        let last = AtomicU64::new(0);
        assert!(warn_now(&last));
        assert!(!warn_now(&last));
        last.store(clock::now_us().saturating_sub(1_000_001).max(1), Ordering::Relaxed);
        assert!(warn_now(&last));
    }

    #[test]
    fn tiles_must_fit_their_stream() {
        let mut video = VideoFrame {
            codec: Codec::Hevc,
            keyframe: false,
            width: 6144,
            height: 2560,
            tile: TileRect { index: 15, x: 3072, y: 2240, width: 3072, height: 320 },
            update: 1,
            update_mask: 1 << 15,
            previous_mask: 0,
            capture_time_us: 0,
            encode_start_us: 0,
            encoded_time_us: 0,
            param_sets: Vec::new(),
            nal_length_size: 4,
            data: Vec::new(),
            cover: Vec::new(),
        };
        assert!(tile_fits(&video, 15));
        assert!(!tile_fits(&video, 14), "datagrams said another tile");
        video.tile.height = 322;
        assert!(!tile_fits(&video, 15));
        video.tile.height = 320;
        video.tile.x = u32::MAX;
        assert!(!tile_fits(&video, 15), "no overflow");
    }
}
