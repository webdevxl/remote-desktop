//! Wire protocol shared by host and client.
//!
//! One QUIC connection per session carries:
//! - a bidirectional **control stream** of length-prefixed [`ClientMsg`] / [`HostMsg`],
//! - unreliable **datagrams** host→client, each a [`VideoPacketHeader`] followed by a chunk of
//!   a postcard-encoded [`VideoFrame`],
//! - a unidirectional **input stream** client→host of length-prefixed [`InputMsg`], opened by
//!   the client after `Welcome`. It is reliable and ordered, so a button press can never overtake
//!   the move before it and every press reaches the host with its release.
//!
//! Compatibility: the version check happens on `Hello`, and a mismatched peer is told so with
//! `Rejected`. Those two messages must keep their exact wire layout forever (see the golden-bytes
//! test), and new variants only go at the end of each enum.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const PROTOCOL_VERSION: u32 = 3;
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
    /// Every event the host injects carries this in `kCGEventSourceUserData`. A client on the
    /// same Mac drops events with it, so injected input never loops back into the session.
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
    /// Sent by the client's UI thread every [`HEARTBEAT_INTERVAL_MS`] while it holds anything.
    /// If they stop (the client hung, crashed or lost the network), the host lets go.
    Heartbeat,
    /// How many LanKVM hosts the input that follows has already passed through: 0 when it was
    /// made on the client's own Mac, n + 1 when a host injected it there at depth n (a chain:
    /// A controls B, whose window controls C). Applies until the next one. Hosts drop input
    /// deeper than [`MAX_RELAY_DEPTH`], so Macs controlling each other in a loop (A → B → A, or
    /// a ring) can't bounce input around forever.
    Relayed { depth: u8 },
}

/// Deepest relayed input a host still injects: chains of up to four Macs.
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
            other => other,
        })
    }

    /// Folds `next` into `self` if nothing is lost by doing so: consecutive moves (the newest
    /// position wins) and consecutive mid-gesture scrolls (deltas add up). Never merges across a
    /// button, key or gesture boundary, so order is preserved. Returns whether it merged.
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
            _ => false,
        }
    }
}

/// One encoded video frame, split across datagrams by the packetizer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct VideoFrame {
    pub codec: Codec,
    pub keyframe: bool,
    pub width: u32,
    pub height: u32,
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
}

/// Header in front of every video datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoPacketHeader {
    pub frame_id: u32,
    pub index: u16,
    pub count: u16,
    /// Total length of the serialized frame; chunk offsets derive from it.
    pub total_len: u32,
}

impl VideoPacketHeader {
    pub const LEN: usize = 12;

    pub fn write(&self, out: &mut Vec<u8>) {
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
            frame_id: u32::from_le_bytes(buf[0..4].try_into().ok()?),
            index: u16::from_le_bytes(buf[4..6].try_into().ok()?),
            count: u16::from_le_bytes(buf[6..8].try_into().ok()?),
            total_len: u32::from_le_bytes(buf[8..12].try_into().ok()?),
        };
        if header.count == 0 || header.index >= header.count {
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
        let h = VideoPacketHeader { frame_id: 7, index: 3, count: 9, total_len: 12_345 };
        let mut buf = Vec::new();
        h.write(&mut buf);
        buf.extend_from_slice(b"payload");
        let (parsed, rest) = VideoPacketHeader::parse(&buf).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(rest, b"payload");
    }

    #[test]
    fn header_rejects_bad_index() {
        let h = VideoPacketHeader { frame_id: 1, index: 4, count: 4, total_len: 10 };
        let mut buf = Vec::new();
        h.write(&mut buf);
        assert!(VideoPacketHeader::parse(&buf).is_none());
        assert!(VideoPacketHeader::parse(&buf[..5]).is_none());
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
        assert_eq!(encode(&InputMsg::MouseMove { x: 1, y: 2 }).unwrap(), [0, 1, 2]);
        assert_eq!(encode(&InputMsg::Key { code: 0, down: true, repeat: false }).unwrap(), [3, 0, 1, 0]);
        assert_eq!(encode(&InputMsg::ReleaseAll).unwrap(), [5]);
        assert_eq!(encode(&InputMsg::Heartbeat).unwrap(), [6]);
        assert_eq!(encode(&InputMsg::Relayed { depth: 2 }).unwrap(), [7, 2]);
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
