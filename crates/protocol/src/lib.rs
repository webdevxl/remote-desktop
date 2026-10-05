//! Wire protocol shared by host and client.
//!
//! One QUIC connection per session carries:
//! - a bidirectional **control stream** of length-prefixed [`ClientMsg`] / [`HostMsg`],
//! - unreliable **datagrams** host→client, each a [`VideoPacketHeader`] followed by a chunk of
//!   a postcard-encoded [`VideoFrame`]. The picture is split into tiles (see [`tile_layout`]),
//!   each its own video stream: only tiles that changed are sent, and each one is decoded and
//!   shown as soon as it arrives instead of waiting for the whole frame,
//! - a unidirectional **input stream** client→host of length-prefixed [`InputMsg`], opened by
//!   the client after `Welcome`. It is reliable and ordered, so a button press can never overtake
//!   the move before it and every press reaches the host with its release.
//! - a unidirectional **clipboard stream** per clipboard transfer, either way, while the client
//!   controls the host and shares its clipboard (see [`ClipboardHeader`]). Each is its own stream,
//!   so a big image never holds up control messages or input.
//! - unreliable **datagrams** client→host with the client's microphone, while the host takes it
//!   (see [`ClientMsg::Microphone`] and [`MicPacket`]): 10 ms of audio each, played into the
//!   host's "LanKVM Microphone".
//!
//! Every unidirectional stream starts with one byte saying what it carries ([`STREAM_INPUT`],
//! [`STREAM_CLIPBOARD`]).
//!
//! Compatibility: the version check happens on `Hello`, and a mismatched peer is told so with
//! `Rejected`. Those two messages must keep their exact wire layout forever (see the golden-bytes
//! test), and new variants only go at the end of each enum.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const PROTOCOL_VERSION: u32 = 10;
pub const DEFAULT_PORT: u16 = 47800;
pub const ALPN: &[u8] = b"lankvm/1";
/// Upper bound for a single control message; protects against garbage length prefixes.
pub const MAX_CONTROL_MSG_LEN: usize = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ClientMsg {
    /// First message on the control stream.
    Hello {
        version: u32,
        device_name: String,
        /// Largest frame the client wants, in pixels (usually its screen size).
        max_width: u32,
        max_height: u32,
        fps: u32,
        /// Whether the client has already paired with this host's certificate. If either side
        /// doesn't know the other, the host asks for pairing.
        trusts_host: bool,
    },
    /// SPAKE2 message derived from the PIN the user typed (pairing step 1).
    PairStart { spake: Vec<u8> },
    /// Proof that the client derived the same key (pairing step 3).
    PairConfirm { mac: Vec<u8> },
    /// The client lost a frame and can't decode until the next keyframe.
    RequestKeyframe,
    Ping { client_time_us: u64 },
    /// Asks to control the host (true) or to only view it (false). The host answers with
    /// [`HostMsg::Control`] carrying the same `request` (1, 2, 3... per session), so an answer to
    /// an earlier request can be told apart. Input is ignored until control is granted.
    /// `take_over`: take control even if another device has it.
    SetControl { on: bool, request: u32, take_over: bool },
    /// While controlling: whether the client is forwarding input right now (its window has the
    /// focus). When it isn't, the host draws its cursor into the video again.
    Focus { forwarding: bool },
    /// Which of the host's displays to watch. The host answers with [`HostMsg::Display`] carrying
    /// the same `request` (1, 2, 3... per session, like [`ClientMsg::SetControl`]).
    SetDisplay { request: u32, display: DisplayChoice },
    /// The client lost frames of some tiles (bit `i` is [`TileRect::index`] `i`) and can't decode
    /// them until their next keyframe. [`ClientMsg::RequestKeyframe`] asks for every tile.
    RequestKeyframes { tiles: u64 },
    /// The client can't decode the full-frame stream ([`FULL_FRAME_TILE`]), though it decodes the
    /// tiles (its decoder refuses the whole picture's size): send every change as tiles, for the
    /// rest of the connection.
    NoFullFrame,
    /// Whether the client shares its clipboard with the host while it controls it. Sent after
    /// `Welcome` and whenever it changes. A host shares its clipboard only with the client that
    /// controls it, and only once that client said yes.
    ShareClipboard { on: bool },
    /// Asks the host to play the client's microphone into its "LanKVM Microphone" (true), or to
    /// stop (false). The host answers with [`HostMsg::Microphone`]; the client sends its audio
    /// ([`MicPacket`]s) only while that says it's active.
    Microphone { on: bool },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum HostMsg {
    Welcome {
        device_name: String,
        width: u32,
        height: u32,
        fps: u32,
        codec: Codec,
    },
    Rejected { reason: String },
    /// The devices haven't paired yet: the host is showing a PIN to type on the client.
    PairingRequired,
    /// SPAKE2 reply plus proof of the derived key (pairing step 2).
    PairReply { spake: Vec<u8>, mac: Vec<u8> },
    Pong { client_time_us: u64, host_time_us: u64 },
    /// Whether this client controls the host now, and if not, why (None: it asked to view only).
    Control(ControlState),
    /// A cursor image the client hasn't seen, named `id` from now on. PNG, at 2x the size in
    /// points; the hot spot is in points from the top-left corner.
    CursorShape { id: u32, png: Vec<u8>, width: f32, height: f32, hot_x: f32, hot_y: f32 },
    /// How the client should show the host's cursor while it controls the host.
    Cursor(CursorState),
    /// Input stream progress: `seq` counts [`InputMsg`]s received, from 1. Times are host clock.
    /// Sent at most every [`INPUT_ACK_INTERVAL_US`], for the latency overlay.
    InputAck { seq: u64, received_us: u64, injected_us: u64 },
    /// The display the client watches now, answering [`ClientMsg::SetDisplay`] or because it
    /// changed on the host's side. Frames of the new stream follow.
    Display(DisplayState),
    /// The screen went still after `update`, which sent the tiles in `mask`: nothing follows
    /// until it changes. Sent on the reliable control stream, so a client that lost that update
    /// entirely (no newer one comes to reveal it) still learns which tiles to ask for again.
    VideoIdle { update: u32, mask: u64 },
    /// How this client can reach the host over the internet: its access key for knocking on the
    /// host (32 bytes, the same on every connection) and the addresses the host announces for it
    /// ("203.0.113.7:47800", "home.example.com:47800"), empty while internet access is off. Sent
    /// after the first [`HostMsg::Display`].
    ///
    /// `rendezvous_server` ("host:port") and `rendezvous_id` (16 bytes) say where the host
    /// registers to be introduced to its viewers with no router setup, and under what ID; both
    /// empty while internet access or the server is off.
    InternetAccess { key: Vec<u8>, addresses: Vec<String>, rendezvous_server: String, rendezvous_id: Vec<u8> },
    /// Whether the host plays the client's microphone now: answering [`ClientMsg::Microphone`],
    /// and whenever that changes on the host's side. Also sent as the session starts, to say
    /// whether the client could share its microphone (`reason`).
    Microphone(MicrophoneState),
}

/// Which of the host's displays a client watches.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayChoice {
    /// The host's main display, as on connecting.
    Main,
    /// A display the host creates for the client, which exists only in software.
    Virtual(VirtualDisplaySpec),
}

/// A virtual display a client asks the host for, usually its own screen's size, so the host's
/// desktop fills it pixel for pixel.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualDisplaySpec {
    /// Size in pixels; also the size of the stream.
    pub width: u32,
    pub height: u32,
    /// Drawn at 2x (Retina): the desktop is laid out in half as many points each way, as sharp as
    /// the pixels allow. Otherwise one point is one pixel.
    pub hidpi: bool,
    /// How often the host draws it, per second.
    pub refresh_hz: u32,
    pub arrangement: Arrangement,
}

/// How a virtual display sits among the host's own displays. A number rather than an enum, so a
/// client can ask for one a host doesn't know yet; that host says no.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Arrangement(pub u8);

impl Arrangement {
    /// An extra display next to the host's own.
    pub const EXTEND: Self = Self(0);
    /// Next to them, and the main display: menu bar, Dock and new windows go to it.
    pub const MAIN: Self = Self(1);
    /// The host's own displays mirror it, so every window is on it.
    pub const ONLY: Self = Self(2);

    /// Whether this version knows the arrangement.
    pub fn is_known(self) -> bool {
        self.0 <= 2
    }
}

impl VirtualDisplaySpec {
    pub const MIN_WIDTH: u32 = 640;
    pub const MIN_HEIGHT: u32 = 480;
    /// Largest size per side. Hardware HEVC goes further, but not at 30 fps any more.
    pub const MAX_SIDE: u32 = 8192;
    /// Largest size in all: 8K, the most HEVC (level 6.2) encodes in one picture.
    pub const MAX_PIXELS: u64 = 8192 * 4320;
    /// Widest (or tallest) shape, as width to height.
    pub const MAX_ASPECT: u32 = 4;
    pub const MIN_REFRESH_HZ: u32 = 24;
    pub const MAX_REFRESH_HZ: u32 = 120;

    /// The spec with its refresh rate in range, or why a host can't make it (in words for the
    /// client to show). Sizes must be even: the encoder needs that, and a Retina display's size in
    /// points is half of it.
    pub fn validated(self) -> Result<Self, String> {
        let (w, h) = (self.width, self.height);
        if !(Self::MIN_WIDTH..=Self::MAX_SIDE).contains(&w) || !(Self::MIN_HEIGHT..=Self::MAX_SIDE).contains(&h) {
            return Err(format!(
                "A virtual display can be {}×{} to {max}×{max} pixels, not {w}×{h}.",
                Self::MIN_WIDTH,
                Self::MIN_HEIGHT,
                max = Self::MAX_SIDE
            ));
        }
        if u64::from(w) * u64::from(h) > Self::MAX_PIXELS {
            return Err(format!("A virtual display can have up to 8K pixels (8192×4320), not {w}×{h}."));
        }
        if w > h * Self::MAX_ASPECT || h > w * Self::MAX_ASPECT {
            return Err(format!("A virtual display can be up to {} times as wide as it is high, not {w}×{h}.", Self::MAX_ASPECT));
        }
        if w % 2 != 0 || h % 2 != 0 {
            return Err(format!("A virtual display needs an even number of pixels each way, not {w}×{h}."));
        }
        if !self.arrangement.is_known() {
            return Err("That host doesn't know this way of arranging a display. Update LanKVM there.".into());
        }
        Ok(Self { refresh_hz: self.refresh_hz.clamp(Self::MIN_REFRESH_HZ, Self::MAX_REFRESH_HZ), ..self })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct DisplayState {
    /// The [`ClientMsg::SetDisplay`] request this answers; 0 when the host changed it on its own
    /// (its user removed the virtual display, another device took over the main display...).
    pub request: u32,
    /// What the client watches now (for a virtual display, as it is now).
    pub display: DisplayChoice,
    /// The stream now, as in [`HostMsg::Welcome`].
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Why the request wasn't done, or why the display changed on the host's side
    /// ([`DisplayReason::NONE`] when the request was done).
    pub reason: DisplayReason,
    /// The reason (or news) in words, naming the host, for the client to show. May be set with
    /// [`DisplayReason::NONE`] too (e.g. another device took over the main display).
    pub message: String,
    /// Why the client can't ask for a virtual display now ([`DisplayReason::NONE`] if it can), and
    /// in words, for its menus.
    pub available: DisplayReason,
    pub unavailable: String,
}

/// Why a client doesn't get the display it asked for, or lost it. A number rather than an enum,
/// so a reason added later still decodes; a client shows the message of a code it doesn't know.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DisplayReason(pub u16);

impl DisplayReason {
    pub const NONE: Self = Self(0);
    /// The request asked for something no host makes (size, shape, arrangement).
    pub const INVALID: Self = Self(1);
    /// The host's "Let paired Macs control this Mac" setting is off (or it no longer trusts the
    /// client).
    pub const NOT_ALLOWED: Self = Self(2);
    /// The host's macOS can't make virtual displays.
    pub const UNSUPPORTED: Self = Self(3);
    /// Making or showing the display failed; trying again may work.
    pub const FAILED: Self = Self(4);
    /// The host's user removed the virtual display (or stopped remote use of the Mac).
    pub const REMOVED_BY_HOST: Self = Self(5);
    /// The display went away on its own.
    pub const DISPLAY_GONE: Self = Self(6);
    /// The client runs on the host's own Mac: only an extended display is possible.
    pub const SAME_MAC: Self = Self(7);
    /// Another device controls the host, so the main display isn't the client's to take.
    pub const IN_USE: Self = Self(8);
    /// The host doesn't stream its screen (tests).
    pub const NO_VIDEO: Self = Self(9);
    /// The host has as many virtual displays as it makes.
    pub const TOO_MANY: Self = Self(10);
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MicrophoneState {
    /// The host plays the client's microphone into its "LanKVM Microphone" now.
    pub active: bool,
    /// Why it doesn't, or wouldn't if asked ([`MicrophoneReason::NONE`] when it does, or could).
    pub reason: MicrophoneReason,
    /// The reason in words, naming the host, for the client to show. Empty for `NONE`.
    pub message: String,
}

/// Why a host doesn't play a client's microphone. A number rather than an enum, so a reason added
/// later still decodes; a client shows the message of a code it doesn't know.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MicrophoneReason(pub u16);

impl MicrophoneReason {
    pub const NONE: Self = Self(0);
    /// The host doesn't have the LanKVM Microphone driver (or its audio system hasn't loaded it).
    pub const NOT_INSTALLED: Self = Self(1);
    /// The host's "Let paired Macs control this Mac" setting is off.
    pub const TURNED_OFF: Self = Self(2);
    /// Playing into the driver failed on the host; trying again may work.
    pub const FAILED: Self = Self(3);
    /// The host's user stopped it.
    pub const STOPPED_BY_HOST: Self = Self(4);
    // The client's own reasons, never sent by a host:
    /// The client couldn't open its microphone.
    pub const CAPTURE_FAILED: Self = Self(100);
    /// The client's microphone is its own "LanKVM Microphone": it would send back what other Macs
    /// sent it.
    pub const LOOPBACK: Self = Self(101);
    /// The client has no microphone.
    pub const NO_INPUT: Self = Self(102);
}

/// Sample rate of the microphone audio clients send.
pub const MIC_SAMPLE_RATE: u32 = 48_000;
/// Samples (mono) in one [`MicPacket`]: 10 ms.
pub const MIC_PACKET_SAMPLES: usize = 480;

/// One datagram of a client's microphone: 10 ms of 48 kHz mono audio as little-endian 16-bit
/// samples, after a kind byte ([`MicPacket::KIND`]) and a sequence number (counting the client's
/// packets on the connection, from 0, wrapping), so the host can tell lost packets from late ones.
#[derive(Debug, Clone, PartialEq)]
pub struct MicPacket {
    pub seq: u32,
    pub samples: Vec<i16>,
}

impl MicPacket {
    /// First byte of a microphone datagram: kinds tell client datagrams apart, should others come.
    pub const KIND: u8 = 1;
    pub const LEN: usize = 5 + 2 * MIC_PACKET_SAMPLES;

    pub fn write(seq: u32, samples: &[i16; MIC_PACKET_SAMPLES]) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::LEN);
        out.push(Self::KIND);
        out.extend_from_slice(&seq.to_le_bytes());
        for sample in samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    /// None if it isn't a whole microphone datagram.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() != Self::LEN || buf[0] != Self::KIND {
            return None;
        }
        let seq = u32::from_le_bytes(buf[1..5].try_into().ok()?);
        let samples = buf[5..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        Some(Self { seq, samples })
    }
}

/// Minimum spacing between [`HostMsg::InputAck`]s.
pub const INPUT_ACK_INTERVAL_US: u64 = 100_000;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ControlState {
    /// The [`ClientMsg::SetControl`] request this answers; 0 when the host acted on its own
    /// (took control back, permission lost...).
    pub request: u32,
    pub active: bool,
    /// Why control isn't active ([`ControlReason::NONE`] when it is, or when the client asked to
    /// only view).
    pub reason: ControlReason,
    /// The reason in words, naming the host, for the client to show. Empty for `NONE`.
    pub message: String,
    /// The host's tag for injected events: each one carries it in `kCGEventSourceUserData`, with
    /// bits 24..31 replaced by its relay depth (see [`InputMsg::Relayed`]). A client on the same
    /// Mac drops events matching it with those bits masked, so injected input never loops back
    /// into the session.
    pub injected_tag: i64,
    /// The host's process id: injected events also carry it, as a second way to recognise them.
    pub host_pid: u32,
}

/// Why a client doesn't control the host. A number rather than an enum, so a reason added later
/// still decodes; a client shows the message of a code it doesn't know.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ControlReason(pub u16);

impl ControlReason {
    pub const NONE: Self = Self(0);
    /// The host's "Let paired Macs control this Mac" setting is off.
    pub const TURNED_OFF: Self = Self(1);
    /// The host lacks the Accessibility permission.
    pub const NEEDS_PERMISSION: Self = Self(2);
    /// Another device controls the host (the client may take over).
    pub const IN_USE: Self = Self(3);
    /// Another session took control.
    pub const TAKEN_OVER: Self = Self(4);
    /// The host's user took control back.
    pub const STOPPED_BY_HOST: Self = Self(5);
    /// The host's Accessibility permission was withdrawn.
    pub const PERMISSION_LOST: Self = Self(6);
    /// The client runs on the host's own Mac.
    pub const SAME_MAC: Self = Self(7);
    /// The client is the host itself (same identity).
    pub const SELF_CONNECTION: Self = Self(8);
    /// The input stream was malformed or flooded.
    pub const BAD_INPUT: Self = Self(9);
    /// The streamed display went away.
    pub const DISPLAY_GONE: Self = Self(10);
    /// A test's time limit for control ran out.
    pub const TEST_TTL: Self = Self(11);
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorState {
    /// Show the shape sent as [`HostMsg::CursorShape`] with this id.
    Shape(u32),
    /// The host's cursor is hidden (e.g. while typing).
    Hidden,
    /// The host can't report its cursor; it is drawn into the video.
    InVideo,
}

/// Remote input, client→host on the input stream. Positions are normalized to the streamed
/// display: 0 is the left/top edge, [`POS_MAX`] the right/bottom one.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum InputMsg {
    MouseMove { x: u16, y: u16 },
    /// `button`: 0 left, 1 right, 2 middle, 3+ others. `clicks`: the client's click count, so
    /// double clicks follow the client user's timing and settings.
    MouseButton { button: u8, down: bool, clicks: u8, x: u16, y: u16 },
    Scroll(ScrollInput),
    /// A non-modifier key by macOS virtual key code (physical position); the host's keyboard
    /// layout decides the character. `repeat` marks the client's auto-repeat.
    Key { code: u16, down: bool, repeat: bool },
    /// The modifiers held on the client, as `CGEventFlags` with the device-dependent bits that
    /// tell left from right, plus Caps Lock and fn. Sent whenever they change.
    Modifiers { flags: u32 },
    /// Release every key and button (focus left the remote screen, mode changed...).
    ReleaseAll,
    /// Sent by the client's UI thread every [`HEARTBEAT_INTERVAL_MS`] while it holds anything or
    /// a gesture is in progress. If they stop (the client hung, crashed or lost the network), the
    /// host lets go.
    Heartbeat,
    /// How many LanKVM hosts the input that follows has already passed through: 0 when it was
    /// made on the client's own Mac, n + 1 when a host injected it there at depth n (a chain:
    /// A controls B, whose window controls C). Applies until the next one. Hosts drop input
    /// deeper than [`MAX_RELAY_DEPTH`], so Macs controlling each other in a loop (A → B → A, or
    /// a ring) can't bounce input around forever.
    Relayed { depth: u8 },
    /// A trackpad gesture made on the client, replayed on the host with its phases. The host
    /// treats one in progress like a held button: a release, silence or the end of control ends
    /// it as cancelled.
    Gesture(GestureInput),
    /// Something the client asks for directly (a menu item), rather than input to replay.
    System(SystemAction),
}

/// Deepest relayed input a host still injects: input made on one Mac reaches up to four
/// controlled Macs in a chain (A → B → C → D → E); a fifth drops it. Releases always apply.
pub const MAX_RELAY_DEPTH: u8 = 3;

pub const POS_MAX: u16 = u16::MAX;
/// Largest input message (they are all a few bytes); longer means a broken stream.
pub const MAX_INPUT_MSG_LEN: usize = 256;
pub const HEARTBEAT_INTERVAL_MS: u64 = 250;
/// The host releases held input after this long without any input message.
pub const INPUT_SILENCE_RELEASE_MS: u64 = 1000;

/// One scroll event, field for field as the client's Mac reported it (see
/// `platform_mac::inject::Scroll`). `y` is the vertical axis.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
pub struct ScrollInput {
    pub x: u16,
    pub y: u16,
    pub lines_y: i32,
    pub lines_x: i32,
    pub fixed_y: f32,
    pub fixed_x: f32,
    pub pixels_y: i32,
    pub pixels_x: i32,
    pub continuous: bool,
    /// `CGScrollPhase` (0, 1 began, 2 changed, 4 ended, 8 cancelled, 128 may begin).
    pub phase: u8,
    /// `CGMomentumScrollPhase` (0, 1 begin, 2 continue, 3 end).
    pub momentum: u8,
    pub inverted: bool,
}

impl ScrollInput {
    /// Whether `next` can be folded into this event without losing a gesture boundary: both are
    /// mid-gesture (or mid-momentum) updates of the same kind at the same place.
    pub fn can_merge(&self, next: &ScrollInput) -> bool {
        let mid = |s: &ScrollInput| match (s.phase, s.momentum) {
            (2, 0) | (0, 2) => true,
            // A plain wheel click has neither phase.
            (0, 0) => !s.continuous,
            _ => false,
        };
        mid(self)
            && mid(next)
            && (self.phase, self.momentum, self.continuous, self.inverted, self.x, self.y)
                == (next.phase, next.momentum, next.continuous, next.inverted, next.x, next.y)
    }

    pub fn merge(&mut self, next: &ScrollInput) {
        self.lines_y = self.lines_y.saturating_add(next.lines_y);
        self.lines_x = self.lines_x.saturating_add(next.lines_x);
        self.fixed_y += next.fixed_y;
        self.fixed_x += next.fixed_x;
        self.pixels_y = self.pixels_y.saturating_add(next.pixels_y);
        self.pixels_x = self.pixels_x.saturating_add(next.pixels_x);
    }
}

/// Where a trackpad gesture is in its life.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GesturePhase {
    Began,
    Changed,
    Ended,
    Cancelled,
}

impl GesturePhase {
    /// From `IOHIDEventPhaseBits` as gesture events carry them (1 began, 2 changed, 4 ended,
    /// 8 cancelled; the same numbers as `CGScrollPhase`, not `NSEvent.Phase`). None otherwise.
    pub fn from_bits(bits: u8) -> Option<Self> {
        Some(match bits {
            1 => Self::Began,
            2 => Self::Changed,
            4 => Self::Ended,
            8 => Self::Cancelled,
            _ => return None,
        })
    }

    pub fn bits(self) -> u8 {
        match self {
            Self::Began => 1,
            Self::Changed => 2,
            Self::Ended => 4,
            Self::Cancelled => 8,
        }
    }

    /// Whether the gesture is over (ended or cancelled).
    pub fn ends(self) -> bool {
        matches!(self, Self::Ended | Self::Cancelled)
    }
}

/// Which way a swipe the Dock acts on goes: the Dock's own motion types (1, 2, 3).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockAxis {
    /// Between Spaces and full-screen apps.
    Horizontal,
    /// Mission Control and App Exposé.
    Vertical,
    /// Thumb and three fingers: Launchpad (Apps on macOS 26) and Show Desktop.
    Pinch,
}

impl DockAxis {
    pub fn from_motion(motion: u8) -> Option<Self> {
        Some(match motion {
            1 => Self::Horizontal,
            2 => Self::Vertical,
            3 => Self::Pinch,
            _ => return None,
        })
    }

    pub fn motion(self) -> u8 {
        match self {
            Self::Horizontal => 1,
            Self::Vertical => 2,
            Self::Pinch => 3,
        }
    }
}

/// A trackpad gesture. Positioned ones act on the window under `x`, `y` (normalized like
/// [`InputMsg::MouseMove`]); Dock swipes act on the whole Mac.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum GestureInput {
    /// A three- or four-finger swipe or pinch that the Dock acts on: Spaces, Mission Control,
    /// App Exposé, Show Desktop, Launchpad/Apps. The values are the client trackpad's own (Dock
    /// gesture fields 124, 129, 130 and 136), with `progress` and the velocities multiplied by
    /// the client's `platform_mac::gesture::dock_direction`, which the host divides out again
    /// for its own macOS version and scrolling setting. So between two Macs set up alike, the
    /// host replays exactly what the trackpad reported.
    DockSwipe {
        axis: DockAxis,
        phase: GesturePhase,
        /// Distance since the gesture began; ±1.0 is about one whole transition (one Space, or
        /// Mission Control fully open).
        progress: f32,
        /// Exit velocity, on ended or cancelled.
        velocity_x: f32,
        velocity_y: f32,
        /// The event's "inverted from device" flag (field 136), replayed as it came.
        inverted: bool,
    },
    /// Pinch to zoom: `delta` as `NSEvent.magnification`, since the previous event (+0.01 is
    /// 1% larger).
    Magnify { x: u16, y: u16, phase: GesturePhase, delta: f32 },
    /// Two-finger rotation: `degrees` as `NSEvent.rotation` (counterclockwise positive), since
    /// the previous event.
    Rotate { x: u16, y: u16, phase: GesturePhase, degrees: f32 },
    /// Two-finger double tap (smart zoom).
    SmartMagnify { x: u16, y: u16 },
    /// Swipe between pages (three fingers, or two on a Magic Mouse): -1, 0 or 1 per axis, as a
    /// swipe event's `deltaX` and `deltaY`.
    NavigationSwipe { x: u16, y: u16, dx: i8, dy: i8 },
}

impl GestureInput {
    /// The phase of a gesture that has phases; None for the one-shot ones (taps, page swipes).
    pub fn phase(&self) -> Option<GesturePhase> {
        match *self {
            GestureInput::DockSwipe { phase, .. } | GestureInput::Magnify { phase, .. } | GestureInput::Rotate { phase, .. } => Some(phase),
            GestureInput::SmartMagnify { .. } | GestureInput::NavigationSwipe { .. } => None,
        }
    }

    /// Where it happens, for gestures that go to the window under the pointer.
    pub fn position(&self) -> Option<(u16, u16)> {
        match *self {
            GestureInput::DockSwipe { .. } => None,
            GestureInput::Magnify { x, y, .. }
            | GestureInput::Rotate { x, y, .. }
            | GestureInput::SmartMagnify { x, y }
            | GestureInput::NavigationSwipe { x, y, .. } => Some((x, y)),
        }
    }

    /// Whether this ends a gesture in progress (ended or cancelled).
    pub fn ends(&self) -> bool {
        self.phase().is_some_and(GesturePhase::ends)
    }

    /// Whether `next` can be folded into this update without losing a phase boundary: both are
    /// mid-gesture updates of the same gesture.
    pub fn can_merge(&self, next: &GestureInput) -> bool {
        use GestureInput::*;
        use GesturePhase::Changed;
        match (*self, *next) {
            (DockSwipe { axis, phase: Changed, inverted, .. }, DockSwipe { axis: a, phase: Changed, inverted: i, .. }) => (axis, inverted) == (a, i),
            (Magnify { x, y, phase: Changed, .. }, Magnify { x: nx, y: ny, phase: Changed, .. })
            | (Rotate { x, y, phase: Changed, .. }, Rotate { x: nx, y: ny, phase: Changed, .. }) => (x, y) == (nx, ny),
            _ => false,
        }
    }

    /// Folds `next` in (see [`can_merge`](Self::can_merge)): a Dock swipe's progress is
    /// cumulative, so the newest wins; magnifications compose; rotations add up.
    pub fn merge(&mut self, next: &GestureInput) {
        match (self, *next) {
            (
                GestureInput::DockSwipe { progress, velocity_x, velocity_y, .. },
                GestureInput::DockSwipe { progress: p, velocity_x: vx, velocity_y: vy, .. },
            ) => (*progress, *velocity_x, *velocity_y) = (p, vx, vy),
            // (1 + a)(1 + b) - 1: two zoom steps in a row.
            (GestureInput::Magnify { delta, .. }, GestureInput::Magnify { delta: d, .. }) => *delta += d + *delta * d,
            (GestureInput::Rotate { degrees, .. }, GestureInput::Rotate { degrees: d, .. }) => *degrees += d,
            _ => {}
        }
    }
}

/// Something to do on the host's Mac as a whole. A number rather than an enum, so a client can
/// ask for one a host doesn't know yet; that host ignores it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SystemAction(pub u16);

impl SystemAction {
    pub const MISSION_CONTROL: Self = Self(1);
    /// The front app's windows.
    pub const APP_EXPOSE: Self = Self(2);
    pub const SHOW_DESKTOP: Self = Self(3);
    /// Launchpad; Apps on macOS 26 and later.
    pub const LAUNCHPAD: Self = Self(4);
    /// One Space (or full-screen app) to the left.
    pub const PREVIOUS_SPACE: Self = Self(5);
    /// One Space (or full-screen app) to the right.
    pub const NEXT_SPACE: Self = Self(6);

    /// Whether this version knows the action.
    pub fn is_known(self) -> bool {
        (1..=6).contains(&self.0)
    }
}

impl InputMsg {
    /// The message with every value in range, or None if it can't be meaningful. The host
    /// applies this to everything it receives before injecting it.
    pub fn sanitized(self) -> Option<InputMsg> {
        Some(match self {
            InputMsg::MouseButton { button, .. } if button >= 32 => return None,
            InputMsg::MouseButton { button, down, clicks, x, y } => InputMsg::MouseButton { button, down, clicks: clicks.max(1), x, y },
            InputMsg::Key { code, .. } if code > 0x7F => return None,
            InputMsg::Scroll(s) => {
                let fixed = |v: f32| if v.is_finite() { v.clamp(-10_000.0, 10_000.0) } else { 0.0 };
                InputMsg::Scroll(ScrollInput {
                    fixed_y: fixed(s.fixed_y),
                    fixed_x: fixed(s.fixed_x),
                    pixels_y: s.pixels_y.clamp(-32_767, 32_767),
                    pixels_x: s.pixels_x.clamp(-32_767, 32_767),
                    lines_y: s.lines_y.clamp(-1_000, 1_000),
                    lines_x: s.lines_x.clamp(-1_000, 1_000),
                    phase: if matches!(s.phase, 0 | 1 | 2 | 4 | 8 | 128) { s.phase } else { 0 },
                    momentum: if s.momentum <= 3 { s.momentum } else { 0 },
                    ..s
                })
            }
            InputMsg::Gesture(g) => {
                let clamp = |v: f32, max: f32| if v.is_finite() { v.clamp(-max, max) } else { 0.0 };
                InputMsg::Gesture(match g {
                    GestureInput::DockSwipe { axis, phase, progress, velocity_x, velocity_y, inverted } => GestureInput::DockSwipe {
                        axis,
                        phase,
                        progress: clamp(progress, 16.0),
                        velocity_x: clamp(velocity_x, 10_000.0),
                        velocity_y: clamp(velocity_y, 10_000.0),
                        inverted,
                    },
                    // Real events stay far below these (a few hundredths, a few degrees).
                    GestureInput::Magnify { x, y, phase, delta } => GestureInput::Magnify { x, y, phase, delta: clamp(delta, 1.0) },
                    GestureInput::Rotate { x, y, phase, degrees } => GestureInput::Rotate { x, y, phase, degrees: clamp(degrees, 90.0) },
                    GestureInput::NavigationSwipe { dx: 0, dy: 0, .. } => return None,
                    GestureInput::NavigationSwipe { x, y, dx, dy } => GestureInput::NavigationSwipe { x, y, dx: dx.signum(), dy: dy.signum() },
                    tap @ GestureInput::SmartMagnify { .. } => tap,
                })
            }
            InputMsg::System(action) if !action.is_known() => return None,
            other => other,
        })
    }

    /// Folds `next` into `self` if nothing is lost by doing so: consecutive moves (the newest
    /// position wins), consecutive mid-gesture scrolls (deltas add up) and consecutive
    /// mid-gesture trackpad updates. Never merges across a button, key or gesture boundary, so
    /// order is preserved. Returns whether it merged.
    pub fn coalesce(&mut self, next: &InputMsg) -> bool {
        match (self, next) {
            (InputMsg::MouseMove { x, y }, InputMsg::MouseMove { x: nx, y: ny }) => {
                (*x, *y) = (*nx, *ny);
                true
            }
            (InputMsg::Scroll(a), InputMsg::Scroll(b)) if a.can_merge(b) => {
                a.merge(b);
                true
            }
            (InputMsg::Gesture(a), InputMsg::Gesture(b)) if a.can_merge(b) => {
                a.merge(b);
                true
            }
            _ => false,
        }
    }
}

/// First byte of the client's input stream.
pub const STREAM_INPUT: u8 = 0;
/// First byte of a clipboard transfer's stream, either way.
pub const STREAM_CLIPBOARD: u8 = 1;

/// The clipboard types (UTIs) Macs share. Anything else (files, app-private types) stays on its
/// Mac; an image copied as TIFF only goes as PNG. The `org.nspasteboard.*` markers
/// (nspasteboard.org) go along, so the other Mac's clipboard managers also leave out a password
/// copied from a password manager.
pub const CLIPBOARD_TYPES: &[&str] = &[
    "public.utf8-plain-text",
    "public.rtf",
    // Rich text with its pictures (TextEdit, Notes).
    "com.apple.flat-rtfd",
    "public.html",
    "public.url",
    "public.url-name",
    "public.png",
    "com.adobe.pdf",
    "org.nspasteboard.ConcealedType",
    "org.nspasteboard.TransientType",
    "org.nspasteboard.AutoGeneratedType",
    "org.nspasteboard.source",
];

/// Largest clipboard a Mac shares over the local network, every type together. A bigger one isn't
/// sent: the other Mac's clipboard is emptied instead (see [`ClipboardHeader::too_large`]), so it
/// never pastes something older than what was copied.
pub const MAX_CLIPBOARD_BYTES: u64 = 64 * 1024 * 1024;
/// The same over the internet, where it shares a slower path with the video.
pub const MAX_CLIPBOARD_BYTES_INTERNET: u64 = 8 * 1024 * 1024;
/// Longest [`ClipboardHeader`]: it only names a few types.
pub const MAX_CLIPBOARD_HEADER_LEN: usize = 4096;
/// Most types one clipboard transfer carries.
pub const MAX_CLIPBOARD_ITEMS: usize = 16;

/// One clipboard transfer, on a unidirectional stream of its own: after [`STREAM_CLIPBOARD`], this
/// header (length-prefixed, like control messages), then the data of each item in order, then the
/// end of the stream. No items: the clipboard has nothing the Macs share (a copied file, say), and
/// the receiver empties its own.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ClipboardHeader {
    /// Counts the sender's transfers on this connection, from 1: one that finishes after a newer
    /// one was taken is dropped.
    pub seq: u64,
    /// The copy this is, the same on every Mac it reaches (a Mac passing it on keeps it), so it
    /// never comes back to one that has it.
    pub id: u64,
    /// When it was copied, by the clock of the Mac where it was (µs since 1970).
    pub copied_us: u64,
    /// The copy the sender had before this one: if the receiver has that one too, this is newer
    /// than what it has. Otherwise the two Macs changed their clipboards at the same time, and the
    /// later copy ([`copied_us`](Self::copied_us), then [`id`](Self::id)) wins on both.
    pub prior: u64,
    /// Sent as sharing starts, by both Macs: taken only if it is the later copy, and not empty. The
    /// receiver may stop the stream after the header.
    pub offer: bool,
    /// Not 0: the sender's clipboard has this many bytes, more than it shares on this connection.
    /// No items come; the receiver empties its clipboard (and may say why).
    pub too_large: u64,
    pub items: Vec<ClipboardItem>,
}

/// One type of a clipboard transfer: its data (`len` bytes) follows the header, after the items
/// before it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ClipboardItem {
    /// A UTI, such as "public.utf8-plain-text". Receivers skip types they don't share.
    pub kind: String,
    pub len: u64,
}

impl ClipboardHeader {
    /// Bytes of item data after the header.
    pub fn body_len(&self) -> u64 {
        self.items.iter().fold(0u64, |sum, item| sum.saturating_add(item.len))
    }

    /// Whether a receiver can take it at all: a few items, at most [`MAX_CLIPBOARD_BYTES`].
    pub fn check(&self) -> Result<(), String> {
        if self.items.len() > MAX_CLIPBOARD_ITEMS {
            return Err(format!("{} clipboard types, more than {MAX_CLIPBOARD_ITEMS}", self.items.len()));
        }
        if self.body_len() > MAX_CLIPBOARD_BYTES {
            return Err(format!("{} bytes of clipboard, more than {MAX_CLIPBOARD_BYTES}", self.body_len()));
        }
        if self.too_large > 0 && !self.items.is_empty() {
            return Err("a clipboard too large to share, with data".into());
        }
        Ok(())
    }
}

/// Tile indexes a stream uses, the full-frame stream's included: tile masks are a `u64`.
pub const MAX_TILES: usize = 64;

/// The index of the stream that carries the whole picture as one tile. When most of the screen
/// changes at once (scrolling, a new window), one encoder for the whole picture is faster than
/// one per tile, so the host sends that frame there instead. Each tile stream and this one keep
/// their own references, so the client just shows, for every part of the picture, the newest image
/// that covers it. [`tile_layout`] never uses this index.
pub const FULL_FRAME_TILE: u8 = (MAX_TILES - 1) as u8;

/// The index of the stream that carries the whole picture at a lower resolution, while big
/// changes go on frame after frame (scrolling, a moving window): a big picture takes the hardware
/// encoder longer than a frame lasts at 120 Hz, a smaller one doesn't. Its [`TileRect`] is the
/// whole stream, but its pictures are smaller: the client scales them up to fill it. Like
/// [`FULL_FRAME_TILE`] it covers every tile, and the host sends the picture at full resolution
/// again once the motion stops. [`tile_layout`] never uses this index.
pub const MOTION_FRAME_TILE: u8 = (MAX_TILES - 2) as u8;

/// Whether `index` is one of the streams that carry the whole picture ([`FULL_FRAME_TILE`],
/// [`MOTION_FRAME_TILE`]) rather than a tile of it.
pub fn covers_all(index: u8) -> bool {
    index == FULL_FRAME_TILE || index == MOTION_FRAME_TILE
}

/// Tiles [`tile_layout`] makes at most: the last indexes are the whole-picture streams'.
pub const MAX_LAYOUT_TILES: usize = MAX_TILES - 2;

/// One tile of the picture: a rectangle of the stream, in pixels, encoded as its own video stream
/// (its own encoder on the host, its own decoder on the client).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileRect {
    /// Position in the stream's tile list, below [`MAX_TILES`].
    pub index: u8,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl TileRect {
    /// Bit of this tile in a tile mask.
    pub fn bit(&self) -> u64 {
        1u64 << self.index
    }
}

/// Tile edges fall on multiples of this (one HEVC coding tree unit), except the stream's own
/// right and bottom edges.
pub const TILE_ALIGN: u32 = 64;
/// Tiles are about this tall: small enough that typing or a menu re-encodes a sliver of the
/// screen, big enough that a full-screen change doesn't pay per-tile overhead too many times.
pub const TILE_TARGET_HEIGHT: u32 = 320;
/// And at most this wide.
pub const TILE_MAX_WIDTH: u32 = 4096;

/// Splits a `width`×`height` stream into tiles: rows about [`TILE_TARGET_HEIGHT`] tall and
/// columns at most [`TILE_MAX_WIDTH`] wide, edges on [`TILE_ALIGN`]. Row-major, top-left first.
/// `grid` forces `(columns, rows)` instead (clamped so tiles stay at least [`TILE_ALIGN`] and
/// there are at most [`MAX_LAYOUT_TILES`]: the last indexes are [`MOTION_FRAME_TILE`] and
/// [`FULL_FRAME_TILE`]); `(1, 1)` is one tile for the whole picture.
pub fn tile_layout(width: u32, height: u32, grid: Option<(u32, u32)>) -> Vec<TileRect> {
    let (width, height) = (width.max(2), height.max(2));
    let (cols, rows) = grid.unwrap_or_else(|| {
        (width.div_ceil(TILE_MAX_WIDTH), ((height + TILE_TARGET_HEIGHT / 2) / TILE_TARGET_HEIGHT).max(1))
    });
    // Each tile at least one alignment unit each way, and not more tiles than a mask holds.
    let cols = cols.clamp(1, (width / TILE_ALIGN).clamp(1, MAX_LAYOUT_TILES as u32));
    let rows = rows.clamp(1, (height / TILE_ALIGN).max(1)).min((MAX_LAYOUT_TILES as u32 / cols).max(1));
    let xs = edges(width, cols);
    let ys = edges(height, rows);
    let mut tiles = Vec::with_capacity(xs.len() * ys.len());
    for y in ys.windows(2) {
        for x in xs.windows(2) {
            tiles.push(TileRect { index: tiles.len() as u8, x: x[0], y: y[0], width: x[1] - x[0], height: y[1] - y[0] });
        }
    }
    tiles
}

/// `n` spans covering `0..len`, aligned to [`TILE_ALIGN`], as their `n + 1` edges. A remainder
/// too small to be a span of its own joins the one before it.
fn edges(len: u32, n: u32) -> Vec<u32> {
    let step = len.div_ceil(n).div_ceil(TILE_ALIGN).max(1) * TILE_ALIGN;
    let mut edges = vec![0];
    let mut at = step;
    while at < len {
        edges.push(at);
        at += step;
    }
    if edges.len() > 1 && len - edges[edges.len() - 1] < TILE_ALIGN {
        edges.pop();
    }
    edges.push(len);
    edges
}

/// One encoded update of one tile, split across datagrams by the packetizer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct VideoFrame {
    pub codec: Codec,
    pub keyframe: bool,
    /// Size of the whole stream; the tile is a part of it.
    pub width: u32,
    pub height: u32,
    pub tile: TileRect,
    /// Counts the host's captured frames that sent something, per connection (it keeps counting
    /// across display switches) and wrapping. Every tile of one captured frame carries the same
    /// number, so the client can show them together.
    pub update: u32,
    /// Which tiles that captured frame sent ([`TileRect::bit`]), this one included: the client
    /// knows when it has all of them, and which one is missing if one never comes.
    pub update_mask: u64,
    /// `update_mask` of the update before (`update - 1`), so a client that lost every datagram of
    /// that one still knows which tiles it missed (0 if unknown).
    pub previous_mask: u64,
    /// Host clock (µs) when the frame was composited on the host display.
    pub capture_time_us: u64,
    /// Host clock (µs) when the frame went into the encoder.
    pub encode_start_us: u64,
    /// Host clock (µs) when the encoder emitted the frame.
    pub encoded_time_us: u64,
    /// VPS/SPS/PPS (HEVC) or SPS/PPS (H.264). Present on keyframes only.
    pub param_sets: Vec<Vec<u8>>,
    /// Size of the big-endian length prefix before each NAL unit in `data`.
    pub nal_length_size: u8,
    /// Length-prefixed (AVCC/HVCC style) NAL units.
    pub data: Vec<u8>,
    /// For a stream that carries the whole picture ([`covers_all`]): the tiles it paints (where
    /// the picture changed); the others keep what they show. Empty: all of it. Always empty for
    /// tiles.
    pub cover: Vec<TileRect>,
}

/// Header in front of every video datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoPacketHeader {
    /// [`TileRect::index`] of the tile this datagram belongs to. Each tile counts its own
    /// `frame_id`s and is reassembled on its own.
    pub tile: u8,
    pub frame_id: u32,
    pub index: u16,
    pub count: u16,
    /// Total length of the serialized frame; chunk offsets derive from it.
    pub total_len: u32,
}

impl VideoPacketHeader {
    pub const LEN: usize = 13;

    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(self.tile);
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&self.index.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.total_len.to_le_bytes());
    }

    pub fn parse(buf: &[u8]) -> Option<(Self, &[u8])> {
        if buf.len() < Self::LEN {
            return None;
        }
        let header = Self {
            tile: buf[0],
            frame_id: u32::from_le_bytes(buf[1..5].try_into().ok()?),
            index: u16::from_le_bytes(buf[5..7].try_into().ok()?),
            count: u16::from_le_bytes(buf[7..9].try_into().ok()?),
            total_len: u32::from_le_bytes(buf[9..13].try_into().ok()?),
        };
        if header.count == 0 || header.index >= header.count || usize::from(header.tile) >= MAX_TILES {
            return None;
        }
        Some((header, &buf[Self::LEN..]))
    }

    /// Every chunk except the last has this length.
    pub fn chunk_len(total_len: u32, count: u16) -> usize {
        (total_len as usize).div_ceil(count as usize)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("serialization: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("control message too large: {0} bytes")]
    TooLarge(usize),
}

/// Serializes `msg` with a little-endian `u32` length prefix.
pub fn encode_framed<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_CONTROL_MSG_LEN {
        return Err(ProtocolError::TooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, ProtocolError> {
    Ok(postcard::from_bytes(body)?)
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    Ok(postcard::to_stdvec(msg)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_round_trip() {
        let h = VideoPacketHeader { tile: 5, frame_id: 7, index: 3, count: 9, total_len: 12_345 };
        let mut buf = Vec::new();
        h.write(&mut buf);
        buf.extend_from_slice(b"payload");
        let (parsed, rest) = VideoPacketHeader::parse(&buf).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(rest, b"payload");
    }

    #[test]
    fn header_rejects_bad_index() {
        let h = VideoPacketHeader { tile: 0, frame_id: 1, index: 4, count: 4, total_len: 10 };
        let mut buf = Vec::new();
        h.write(&mut buf);
        assert!(VideoPacketHeader::parse(&buf).is_none());
        assert!(VideoPacketHeader::parse(&buf[..5]).is_none());
        let mut buf = Vec::new();
        VideoPacketHeader { tile: MAX_TILES as u8, frame_id: 1, index: 0, count: 4, total_len: 10 }.write(&mut buf);
        assert!(VideoPacketHeader::parse(&buf).is_none(), "tile out of range");
    }

    fn covers_exactly(width: u32, height: u32, tiles: &[TileRect]) {
        let mut area = 0u64;
        for (i, t) in tiles.iter().enumerate() {
            assert_eq!(usize::from(t.index), i);
            assert!(t.width >= 2 && t.height >= 2 && t.width % 2 == 0 && t.height % 2 == 0, "{t:?}");
            assert!(t.x + t.width <= width && t.y + t.height <= height, "{t:?}");
            area += u64::from(t.width) * u64::from(t.height);
            for u in &tiles[..i] {
                let apart = t.x >= u.x + u.width || u.x >= t.x + t.width || t.y >= u.y + u.height || u.y >= t.y + t.height;
                assert!(apart, "{t:?} overlaps {u:?}");
            }
        }
        assert_eq!(area, u64::from(width) * u64::from(height));
        assert!(tiles.len() <= MAX_LAYOUT_TILES, "{} tiles: the last indexes are the whole picture's", tiles.len());
    }

    #[test]
    fn tile_layouts_cover_the_stream() {
        let wide = tile_layout(6144, 2560, None);
        assert_eq!(wide.len(), 16, "2 columns × 8 rows");
        assert_eq!((wide[0].width, wide[0].height), (3072, 320));
        assert_eq!((wide[15].x, wide[15].y), (3072, 2240));
        covers_exactly(6144, 2560, &wide);
        for (w, h) in [(4112, 2658), (3456, 2234), (2560, 1654), (1920, 1080), (1670, 1080), (640, 480), (8192, 4320), (64, 64), (2, 2), (130, 66)] {
            covers_exactly(w, h, &tile_layout(w, h, None));
        }
        assert_eq!(tile_layout(1920, 1080, Some((1, 1))), vec![TileRect { index: 0, x: 0, y: 0, width: 1920, height: 1080 }]);
        covers_exactly(8192, 4320, &tile_layout(8192, 4320, Some((64, 64))));
        // No sliver tiles: a remainder too small for a row of its own joins the row above.
        for h in (64..1200).step_by(2) {
            for rows in 1..20 {
                let tiles = tile_layout(256, h, Some((1, rows)));
                covers_exactly(256, h, &tiles);
                assert!(tiles.iter().all(|t| t.height >= TILE_ALIGN), "{h} px in {rows} rows: {tiles:?}");
            }
        }
    }

    /// `Hello` and `Rejected` are how mismatched versions find out about each other, so their
    /// encoding must never change.
    #[test]
    fn version_handshake_layout_is_frozen() {
        let hello = ClientMsg::Hello {
            version: 2,
            device_name: "a".into(),
            max_width: 1,
            max_height: 2,
            fps: 3,
            trusts_host: true,
        };
        assert_eq!(encode(&hello).unwrap(), [0, 2, 1, 97, 1, 2, 3, 1]);
        assert_eq!(encode(&HostMsg::Rejected { reason: "x".into() }).unwrap(), [1, 1, 120]);
    }

    /// Variant positions are the wire format: appending is fine, reordering is not.
    #[test]
    fn wire_indices() {
        assert_eq!(encode(&ClientMsg::SetControl { on: true, request: 1, take_over: false }).unwrap()[0], 5);
        assert_eq!(encode(&ClientMsg::Focus { forwarding: true }).unwrap()[0], 6);
        let state = ControlState { request: 0, active: false, reason: ControlReason::NONE, message: String::new(), injected_tag: 0, host_pid: 0 };
        assert_eq!(encode(&HostMsg::Control(state)).unwrap()[0], 5);
        assert_eq!(encode(&HostMsg::InputAck { seq: 0, received_us: 0, injected_us: 0 }).unwrap()[0], 8);
        assert_eq!(encode(&ClientMsg::SetDisplay { request: 1, display: DisplayChoice::Main }).unwrap(), [7, 1, 0]);
        assert_eq!(encode(&ClientMsg::RequestKeyframes { tiles: 5 }).unwrap(), [8, 5]);
        assert_eq!(encode(&ClientMsg::NoFullFrame).unwrap(), [9]);
        assert_eq!(encode(&ClientMsg::ShareClipboard { on: true }).unwrap(), [10, 1]);
        assert_eq!(encode(&ClientMsg::Microphone { on: true }).unwrap(), [11, 1]);
        let mic = MicrophoneState { active: false, reason: MicrophoneReason::NOT_INSTALLED, message: "x".into() };
        assert_eq!(encode(&HostMsg::Microphone(mic)).unwrap(), [12, 0, 1, 1, 120]);
        assert_eq!(encode(&HostMsg::VideoIdle { update: 1, mask: 2 }).unwrap(), [10, 1, 2]);
        let access = HostMsg::InternetAccess { key: vec![7], addresses: vec!["a".into()], rendezvous_server: "b".into(), rendezvous_id: vec![9] };
        assert_eq!(encode(&access).unwrap(), [11, 1, 7, 1, 1, 97, 1, 98, 1, 9]);
        let state = DisplayState {
            request: 0,
            display: DisplayChoice::Main,
            width: 0,
            height: 0,
            fps: 0,
            reason: DisplayReason::NONE,
            message: String::new(),
            available: DisplayReason::NONE,
            unavailable: String::new(),
        };
        assert_eq!(encode(&HostMsg::Display(state)).unwrap()[0], 9);
        // Field order is the wire format too: varint sizes, then flags and the arrangement.
        let spec = VirtualDisplaySpec { width: 6144, height: 2560, hidpi: true, refresh_hz: 60, arrangement: Arrangement::ONLY };
        assert_eq!(
            encode(&ClientMsg::SetDisplay { request: 1, display: DisplayChoice::Virtual(spec) }).unwrap(),
            [7, 1, 1, 0x80, 0x30, 0x80, 0x14, 1, 60, 2]
        );
        assert_eq!(encode(&InputMsg::MouseMove { x: 1, y: 2 }).unwrap(), [0, 1, 2]);
        assert_eq!(encode(&InputMsg::Key { code: 0, down: true, repeat: false }).unwrap(), [3, 0, 1, 0]);
        assert_eq!(encode(&InputMsg::ReleaseAll).unwrap(), [5]);
        assert_eq!(encode(&InputMsg::Heartbeat).unwrap(), [6]);
        assert_eq!(encode(&InputMsg::Relayed { depth: 2 }).unwrap(), [7, 2]);
        assert_eq!(encode(&InputMsg::Gesture(GestureInput::SmartMagnify { x: 1, y: 2 })).unwrap(), [8, 3, 1, 2]);
        assert_eq!(encode(&InputMsg::System(SystemAction::MISSION_CONTROL)).unwrap(), [9, 1]);
    }

    #[test]
    fn gestures_round_trip_small() {
        let dock = |phase| GestureInput::DockSwipe { axis: DockAxis::Vertical, phase, progress: -0.4, velocity_x: 0.0, velocity_y: -3.5, inverted: true };
        let msgs = [
            InputMsg::Gesture(dock(GesturePhase::Began)),
            InputMsg::Gesture(dock(GesturePhase::Cancelled)),
            InputMsg::Gesture(GestureInput::Magnify { x: 9, y: POS_MAX, phase: GesturePhase::Changed, delta: 0.02 }),
            InputMsg::Gesture(GestureInput::Rotate { x: 9, y: 10, phase: GesturePhase::Ended, degrees: -3.5 }),
            InputMsg::Gesture(GestureInput::SmartMagnify { x: 0, y: 0 }),
            InputMsg::Gesture(GestureInput::NavigationSwipe { x: 1, y: 2, dx: -1, dy: 0 }),
            InputMsg::System(SystemAction::NEXT_SPACE),
        ];
        for m in msgs {
            let bytes = encode(&m).unwrap();
            assert!(bytes.len() <= 24, "{m:?} takes {} bytes", bytes.len());
            assert_eq!(decode::<InputMsg>(&bytes).unwrap(), m);
        }
        for bits in [1, 2, 4, 8] {
            assert_eq!(GesturePhase::from_bits(bits).unwrap().bits(), bits);
        }
        assert_eq!(GesturePhase::from_bits(16), None);
        assert_eq!(DockAxis::from_motion(2), Some(DockAxis::Vertical));
        assert_eq!(DockAxis::from_motion(0), None);
        assert!(dock(GesturePhase::Ended).ends() && !dock(GesturePhase::Changed).ends());
        assert_eq!(GestureInput::SmartMagnify { x: 3, y: 4 }.position(), Some((3, 4)));
        assert_eq!(dock(GesturePhase::Began).position(), None);
    }

    /// A host one version behind can't read a gesture: that is why they bumped the version.
    #[test]
    fn an_unknown_input_variant_does_not_decode() {
        assert!(decode::<InputMsg>(&[10, 0]).is_err());
    }

    #[test]
    fn sanitizing_gestures() {
        let wild = GestureInput::DockSwipe {
            axis: DockAxis::Horizontal,
            phase: GesturePhase::Changed,
            progress: f32::NAN,
            velocity_x: 1e9,
            velocity_y: f32::NEG_INFINITY,
            inverted: false,
        };
        let Some(InputMsg::Gesture(GestureInput::DockSwipe { progress, velocity_x, velocity_y, .. })) = InputMsg::Gesture(wild).sanitized() else {
            panic!()
        };
        assert_eq!((progress, velocity_x, velocity_y), (0.0, 10_000.0, 0.0));
        let zoom = InputMsg::Gesture(GestureInput::Magnify { x: 0, y: 0, phase: GesturePhase::Changed, delta: 40.0 }).sanitized();
        assert!(matches!(zoom, Some(InputMsg::Gesture(GestureInput::Magnify { delta: 1.0, .. }))));
        let turn = InputMsg::Gesture(GestureInput::Rotate { x: 0, y: 0, phase: GesturePhase::Changed, degrees: -500.0 }).sanitized();
        assert!(matches!(turn, Some(InputMsg::Gesture(GestureInput::Rotate { degrees: -90.0, .. }))));
        let swipe = InputMsg::Gesture(GestureInput::NavigationSwipe { x: 0, y: 0, dx: -7, dy: 3 }).sanitized();
        assert!(matches!(swipe, Some(InputMsg::Gesture(GestureInput::NavigationSwipe { dx: -1, dy: 1, .. }))));
        assert_eq!(InputMsg::Gesture(GestureInput::NavigationSwipe { x: 0, y: 0, dx: 0, dy: 0 }).sanitized(), None);
        assert_eq!(InputMsg::System(SystemAction(999)).sanitized(), None);
        assert_eq!(InputMsg::System(SystemAction::SHOW_DESKTOP).sanitized(), Some(InputMsg::System(SystemAction::SHOW_DESKTOP)));
    }

    #[test]
    fn gesture_updates_coalesce_only_mid_gesture() {
        let dock = |phase, progress| InputMsg::Gesture(GestureInput::DockSwipe {
            axis: DockAxis::Horizontal,
            phase,
            progress,
            velocity_x: 0.0,
            velocity_y: 0.0,
            inverted: false,
        });
        let mut m = dock(GesturePhase::Changed, 0.2);
        assert!(m.coalesce(&dock(GesturePhase::Changed, 0.5)));
        assert_eq!(m, dock(GesturePhase::Changed, 0.5), "progress is cumulative: the newest wins");
        assert!(!m.coalesce(&dock(GesturePhase::Ended, 0.6)), "an end is a boundary");
        assert!(!dock(GesturePhase::Began, 0.0).coalesce(&dock(GesturePhase::Changed, 0.1)));
        let vertical = InputMsg::Gesture(GestureInput::DockSwipe {
            axis: DockAxis::Vertical,
            phase: GesturePhase::Changed,
            progress: 0.1,
            velocity_x: 0.0,
            velocity_y: 0.0,
            inverted: false,
        });
        assert!(!dock(GesturePhase::Changed, 0.1).coalesce(&vertical));

        let zoom = |delta| InputMsg::Gesture(GestureInput::Magnify { x: 5, y: 5, phase: GesturePhase::Changed, delta });
        let mut z = zoom(0.5);
        assert!(z.coalesce(&zoom(0.5)));
        assert_eq!(z, zoom(1.25), "1.5 × 1.5 = 2.25");
        let turn = |degrees| InputMsg::Gesture(GestureInput::Rotate { x: 5, y: 5, phase: GesturePhase::Changed, degrees });
        let mut t = turn(2.0);
        assert!(t.coalesce(&turn(3.0)));
        assert_eq!(t, turn(5.0));
        assert!(!turn(1.0).coalesce(&zoom(0.1)));
        let tap = InputMsg::Gesture(GestureInput::SmartMagnify { x: 5, y: 5 });
        let mut again = tap;
        assert!(!again.coalesce(&tap), "taps are never merged");
    }

    #[test]
    fn sanitizing_input() {
        assert_eq!(InputMsg::MouseButton { button: 40, down: true, clicks: 1, x: 0, y: 0 }.sanitized(), None);
        assert_eq!(
            InputMsg::MouseButton { button: 1, down: true, clicks: 0, x: 0, y: 0 }.sanitized(),
            Some(InputMsg::MouseButton { button: 1, down: true, clicks: 1, x: 0, y: 0 })
        );
        assert_eq!(InputMsg::Key { code: 200, down: true, repeat: false }.sanitized(), None);
        let wild = ScrollInput { fixed_y: f32::NAN, fixed_x: f32::INFINITY, pixels_y: i32::MAX, lines_x: -5000, phase: 3, momentum: 9, ..Default::default() };
        let Some(InputMsg::Scroll(s)) = InputMsg::Scroll(wild).sanitized() else { panic!() };
        assert_eq!((s.fixed_y, s.fixed_x, s.pixels_y, s.lines_x, s.phase, s.momentum), (0.0, 0.0, 32_767, -1_000, 0, 0));
        assert_eq!(InputMsg::Heartbeat.sanitized(), Some(InputMsg::Heartbeat));
    }

    #[test]
    fn new_messages_round_trip() {
        let msgs = [
            HostMsg::Control(ControlState {
                request: 3,
                active: false,
                reason: ControlReason::IN_USE,
                message: "Studio is controlling Mini".into(),
                injected_tag: -5,
                host_pid: 4242,
            }),
            HostMsg::CursorShape { id: 3, png: vec![1, 2, 3], width: 28.0, height: 40.0, hot_x: 4.5, hot_y: 4.0 },
            HostMsg::Cursor(CursorState::Shape(3)),
            HostMsg::Cursor(CursorState::Hidden),
            HostMsg::InputAck { seq: 9, received_us: 10, injected_us: 11 },
        ];
        for m in msgs {
            assert_eq!(decode::<HostMsg>(&encode(&m).unwrap()).unwrap(), m);
        }
        let input = [
            InputMsg::MouseMove { x: 0, y: POS_MAX },
            InputMsg::MouseButton { button: 1, down: true, clicks: 2, x: 7, y: 8 },
            InputMsg::Scroll(ScrollInput { pixels_y: -3, fixed_y: -0.3, continuous: true, phase: 1, ..Default::default() }),
            InputMsg::Key { code: 0x7e, down: false, repeat: true },
            InputMsg::Modifiers { flags: 0x0010_0008 },
            InputMsg::ReleaseAll,
            InputMsg::Heartbeat,
        ];
        for m in input {
            assert_eq!(decode::<InputMsg>(&encode(&m).unwrap()).unwrap(), m);
        }
        for m in [ClientMsg::SetControl { on: true, request: 7, take_over: true }, ClientMsg::Focus { forwarding: false }] {
            assert_eq!(decode::<ClientMsg>(&encode(&m).unwrap()).unwrap(), m);
        }
        // Input is small: a move is a few bytes.
        assert!(encode(&InputMsg::MouseMove { x: 40000, y: 30000 }).unwrap().len() <= 7);
    }

    #[test]
    fn internet_access_round_trips() {
        for m in [
            HostMsg::InternetAccess {
                key: (0..32).collect(),
                addresses: vec!["203.0.113.7:47800".into(), "home.example.com:47801".into()],
                rendezvous_server: "178.156.129.211:3478".into(),
                rendezvous_id: (100..116).collect(),
            },
            HostMsg::InternetAccess { key: vec![0xff; 32], addresses: Vec::new(), rendezvous_server: String::new(), rendezvous_id: Vec::new() },
        ] {
            assert_eq!(decode::<HostMsg>(&encode(&m).unwrap()).unwrap(), m);
        }
    }

    #[test]
    fn coalesces_only_without_losing_anything() {
        let mut m = InputMsg::MouseMove { x: 1, y: 1 };
        assert!(m.coalesce(&InputMsg::MouseMove { x: 5, y: 6 }));
        assert_eq!(m, InputMsg::MouseMove { x: 5, y: 6 });
        assert!(!m.coalesce(&InputMsg::MouseButton { button: 0, down: true, clicks: 1, x: 5, y: 6 }));
        assert!(!InputMsg::Key { code: 0, down: true, repeat: false }.coalesce(&InputMsg::Key { code: 0, down: true, repeat: true }));

        let changed = ScrollInput { pixels_y: 2, fixed_y: 0.2, continuous: true, phase: 2, x: 9, y: 9, ..Default::default() };
        let mut s = InputMsg::Scroll(changed);
        assert!(s.coalesce(&InputMsg::Scroll(changed)));
        assert!(matches!(s, InputMsg::Scroll(ScrollInput { pixels_y: 4, .. })));
        // Gesture boundaries are kept.
        let ended = ScrollInput { phase: 4, ..changed };
        assert!(!s.coalesce(&InputMsg::Scroll(ended)));
        let began = ScrollInput { phase: 1, ..changed };
        assert!(!InputMsg::Scroll(began).coalesce(&InputMsg::Scroll(changed)));
        // Momentum doesn't merge into the finger phase.
        let momentum = ScrollInput { phase: 0, momentum: 2, ..changed };
        assert!(!InputMsg::Scroll(changed).coalesce(&InputMsg::Scroll(momentum)));
        assert!(InputMsg::Scroll(momentum).coalesce(&InputMsg::Scroll(momentum)));
        // Wheel clicks merge; a scroll elsewhere doesn't.
        let wheel = ScrollInput { lines_y: 1, fixed_y: 1.0, ..Default::default() };
        assert!(InputMsg::Scroll(wheel).coalesce(&InputMsg::Scroll(wheel)));
        assert!(!InputMsg::Scroll(wheel).coalesce(&InputMsg::Scroll(ScrollInput { x: 1, ..wheel })));
    }

    fn ultrawide() -> VirtualDisplaySpec {
        VirtualDisplaySpec { width: 6144, height: 2560, hidpi: true, refresh_hz: 60, arrangement: Arrangement::ONLY }
    }

    #[test]
    fn display_messages_round_trip() {
        let virt = DisplayChoice::Virtual(ultrawide());
        for m in [ClientMsg::SetDisplay { request: 3, display: virt }, ClientMsg::SetDisplay { request: 4, display: DisplayChoice::Main }] {
            assert_eq!(decode::<ClientMsg>(&encode(&m).unwrap()).unwrap(), m);
        }
        let state = HostMsg::Display(DisplayState {
            request: 3,
            display: virt,
            width: 6144,
            height: 2560,
            fps: 60,
            reason: DisplayReason::FAILED,
            message: "Studio couldn't make the display.".into(),
            available: DisplayReason::NOT_ALLOWED,
            unavailable: "Studio doesn't let paired Macs control it.".into(),
        });
        assert_eq!(decode::<HostMsg>(&encode(&state).unwrap()).unwrap(), state);
        for arrangement in [Arrangement::EXTEND, Arrangement::MAIN, Arrangement::ONLY, Arrangement(9)] {
            let m = ClientMsg::SetDisplay { request: 1, display: DisplayChoice::Virtual(VirtualDisplaySpec { arrangement, ..ultrawide() }) };
            assert_eq!(decode::<ClientMsg>(&encode(&m).unwrap()).unwrap(), m);
        }
    }

    #[test]
    fn virtual_display_specs_are_checked() {
        assert_eq!(ultrawide().validated(), Ok(ultrawide()));
        let fast = VirtualDisplaySpec { refresh_hz: 500, ..ultrawide() }.validated().unwrap();
        assert_eq!(fast.refresh_hz, 120);
        assert_eq!(VirtualDisplaySpec { refresh_hz: 0, ..ultrawide() }.validated().unwrap().refresh_hz, 24);
        for (width, height) in [(0, 0), (639, 480), (640, 479), (8194, 2560), (6144, u32::MAX), (6143, 2560), (6144, 2561)] {
            let err = VirtualDisplaySpec { width, height, ..ultrawide() }.validated().unwrap_err();
            assert!(err.contains(&format!("{width}×{height}")), "{err}");
        }
        assert!(VirtualDisplaySpec { width: 8192, height: 4320, ..ultrawide() }.validated().is_ok());
        assert!(VirtualDisplaySpec { width: 7680, height: 2160, ..ultrawide() }.validated().is_ok(), "32:9");
        assert!(VirtualDisplaySpec { width: 640, height: 480, hidpi: false, ..ultrawide() }.validated().is_ok());
        assert!(VirtualDisplaySpec { width: 8192, height: 8192, ..ultrawide() }.validated().unwrap_err().contains("8K"));
        assert!(VirtualDisplaySpec { width: 4096, height: 1000, ..ultrawide() }.validated().unwrap_err().contains("4 times"));
        assert!(VirtualDisplaySpec { width: 1000, height: 4096, ..ultrawide() }.validated().unwrap_err().contains("4 times"));
        assert!(VirtualDisplaySpec { arrangement: Arrangement(3), ..ultrawide() }.validated().unwrap_err().contains("Update LanKVM"));
    }

    #[test]
    fn clipboard_headers_round_trip_and_are_checked() {
        let header = ClipboardHeader {
            seq: 3,
            id: u64::MAX - 7,
            copied_us: 1_790_000_000_000_000,
            prior: 9,
            offer: true,
            too_large: 0,
            items: vec![
                ClipboardItem { kind: "public.utf8-plain-text".into(), len: 5 },
                ClipboardItem { kind: "public.png".into(), len: 1 << 20 },
            ],
        };
        let framed = encode_framed(&header).unwrap();
        assert!(framed.len() < 100, "{} bytes", framed.len());
        assert_eq!(decode::<ClipboardHeader>(&framed[4..]).unwrap(), header);
        assert_eq!(header.body_len(), 5 + (1 << 20));
        assert_eq!(header.check(), Ok(()));

        let item = |len| ClipboardItem { kind: "public.png".into(), len };
        let big = ClipboardHeader { items: vec![item(MAX_CLIPBOARD_BYTES), item(1)], ..header.clone() };
        assert!(big.check().unwrap_err().contains("bytes"));
        let overflow = ClipboardHeader { items: vec![item(u64::MAX), item(u64::MAX)], ..header.clone() };
        assert_eq!(overflow.body_len(), u64::MAX, "saturates");
        assert!(overflow.check().is_err());
        let many = ClipboardHeader { items: vec![item(0); MAX_CLIPBOARD_ITEMS + 1], ..header.clone() };
        assert!(many.check().unwrap_err().contains("types"));
        let skipped = ClipboardHeader { too_large: 100 << 20, items: Vec::new(), ..header.clone() };
        assert_eq!(skipped.check(), Ok(()));
        assert!(ClipboardHeader { too_large: 1, ..header }.check().is_err(), "too large comes without data");
        assert!(MAX_CLIPBOARD_BYTES_INTERNET < MAX_CLIPBOARD_BYTES);
        assert_ne!(STREAM_INPUT, STREAM_CLIPBOARD);
    }

    #[test]
    fn mic_packets_round_trip_and_are_checked() {
        let mut samples = [0i16; MIC_PACKET_SAMPLES];
        for (i, s) in samples.iter_mut().enumerate() {
            *s = (i as i16 - 240) * 100;
        }
        samples[0] = i16::MIN;
        samples[1] = i16::MAX;
        let bytes = MicPacket::write(u32::MAX - 1, &samples);
        assert_eq!(bytes.len(), MicPacket::LEN);
        assert!(MicPacket::LEN < 1200 - 60, "fits a datagram on any path QUIC runs on");
        let packet = MicPacket::parse(&bytes).unwrap();
        assert_eq!(packet.seq, u32::MAX - 1);
        assert_eq!(packet.samples, samples);
        assert!(MicPacket::parse(&bytes[..bytes.len() - 1]).is_none(), "short");
        let mut other = bytes.clone();
        other[0] = 0;
        assert!(MicPacket::parse(&other).is_none(), "another kind");
        assert_eq!(MIC_SAMPLE_RATE as usize / MIC_PACKET_SAMPLES, 100, "10 ms each");
    }

    #[test]
    fn control_round_trip() {
        let msg = ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            device_name: "mac".into(),
            max_width: 3456,
            max_height: 2234,
            fps: 60,
            trusts_host: false,
        };
        let framed = encode_framed(&msg).unwrap();
        let len = u32::from_le_bytes(framed[..4].try_into().unwrap()) as usize;
        assert_eq!(len, framed.len() - 4);
        assert_eq!(decode::<ClientMsg>(&framed[4..]).unwrap(), msg);
    }
}
