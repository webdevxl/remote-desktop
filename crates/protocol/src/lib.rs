//! Wire protocol shared by host and client.
//!
//! One QUIC connection per session carries:
//! - a bidirectional **control stream** of length-prefixed [`ClientMsg`] / [`HostMsg`],
//! - unreliable **datagrams** host→client, each a [`VideoPacketHeader`] followed by a chunk of
//!   a postcard-encoded [`VideoFrame`].

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const PROTOCOL_VERSION: u32 = 1;
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
