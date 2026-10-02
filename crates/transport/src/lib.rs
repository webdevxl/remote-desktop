//! Networking for lankvm: QUIC endpoint setup, device identity, control-message framing
//! and the video packetizer/reassembler.

pub mod cc;
pub mod endpoint;
pub mod framing;
pub mod identity;
pub mod net;
pub mod pairing;
pub mod video;
