//! Meeting paired Macs through the BitTorrent DHT, the way torrent clients find each other with
//! no tracker: no server of LanKVM's in between.
//!
//! - [`bencode`] and [`krpc`]: the DHT's wire format and messages (BEP 3, 5).
//! - [`item`]: signed mutable items (BEP 44), small values nodes store for a few hours.
//! - [`client`]: a read-only DHT client (BEP 43) that finds nodes, gets and puts items, and
//!   learns this Mac's public address from what nodes saw (BEP 42).
//! - [`mailbox`]: the two items through which a host and one of its viewers exchange where they
//!   are, sealed with their shared access key.
//!
//! The client runs on the QUIC socket (the gate passes it what looks like KRPC), so the public
//! address the nodes report is the one QUIC uses, and the punches that follow open the routers for
//! exactly that.

pub mod bencode;
pub mod client;
pub mod item;
pub mod krpc;
pub mod mailbox;

pub use client::{Config, DEFAULT_BOOTSTRAP, Dht, Found, Observed, Reply, Stats, Wire};
pub use item::MutableItem;
pub use mailbox::{HostNote, Mailbox, Side, Slot, ViewerNote};
