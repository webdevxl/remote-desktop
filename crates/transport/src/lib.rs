//! Networking for lankvm: QUIC endpoint setup, the gate that keeps it silent to the internet
//! except for paired viewers, rendezvous through a public server (no router setup), device
//! identity, control-message framing and the video packetizer/reassembler.

pub mod cc;
pub mod cid;
pub mod endpoint;
pub mod framing;
pub mod gate;
pub mod identity;
pub mod knock;
pub mod net;
pub mod pairing;
pub mod rendezvous;
#[doc(hidden)]
pub mod test_server;
pub mod video;
