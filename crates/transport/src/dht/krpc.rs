//! KRPC, the BitTorrent DHT's messages (BEP 5): one bencoded dictionary per UDP datagram, a
//! query (`y` = `q`), its response (`r`) or an error (`e`), matched by the transaction ID `t` the
//! querier picked.
//!
//! LanKVM only asks (BEP 43 read-only: `ro` = 1, so nodes never put it in their routing tables
//! or query it) and never answers: a Mac's port stays silent to strangers. Its queries carry no
//! client version (`v`, optional in BEP 5), so the nodes it asks can't tell a LanKVM Mac from
//! any torrent app. Responses carry the address the node saw the query come from (`ip`, BEP 42),
//! which is how a Mac learns its public address without a server of its own.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};

use super::bencode::{self, Value, dict};

pub const ID_LEN: usize = 20;
/// A node's or a key's place on the DHT: SHA-1 sized.
pub type Id = [u8; ID_LEN];

/// Compact node info: a 20-byte ID, a 4-byte IPv4 address and a 2-byte port.
const COMPACT_NODE_V4: usize = ID_LEN + 6;
const COMPACT_NODE_V6: usize = ID_LEN + 18;

/// A node: its ID (as claimed) and where it listens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Node {
    pub id: Id,
    pub addr: SocketAddr,
}

/// What came in: a query (which LanKVM ignores), a response or an error.
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Query { method: Vec<u8>, args: Value },
    Response(Value),
    Error { code: i64, message: String },
}

/// A decoded KRPC message.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub tid: Vec<u8>,
    pub body: Body,
    /// BEP 42: the sender's view of where this message's recipient is.
    pub observed: Option<SocketAddr>,
}

/// Whether `datagram` can be a KRPC message at all, cheaply: a bencoded dictionary whose first
/// key is one KRPC messages start with (`a`, `e`, `ip`, `r`, `t`; keys come sorted). Used on
/// every datagram the QUIC socket gets while the DHT runs, so it must be cheap; [`decode`] is
/// the real check.
pub fn looks_like_krpc(datagram: &[u8]) -> bool {
    datagram.len() >= 12
        && datagram.last() == Some(&b'e')
        && (datagram.starts_with(b"d1:")
            && matches!(datagram[3], b'a' | b'e' | b'r' | b't' | b'v' | b'y')
            || datagram.starts_with(b"d2:ip"))
}

/// Decodes a KRPC datagram; None if it isn't one.
pub fn decode(datagram: &[u8]) -> Option<Message> {
    let value = bencode::decode(datagram)?;
    let tid = value.bytes_at(b"t")?.to_vec();
    let observed = value.bytes_at(b"ip").and_then(compact_addr);
    let body = match value.bytes_at(b"y")? {
        b"q" => Body::Query { method: value.bytes_at(b"q")?.to_vec(), args: value.get(b"a")?.clone() },
        b"r" => {
            let r = value.get(b"r")?;
            r.as_dict()?;
            Body::Response(r.clone())
        }
        b"e" => {
            let e = value.get(b"e")?.as_list()?;
            let code = e.first().and_then(Value::as_int).unwrap_or(0);
            let message = e.get(1).and_then(Value::as_bytes).map(|m| String::from_utf8_lossy(m).into_owned()).unwrap_or_default();
            Body::Error { code, message }
        }
        _ => return None,
    };
    Some(Message { tid, body, observed })
}

/// A query: `method` with `args` (which must include our `id`), as a read-only node.
pub fn query(tid: &[u8], method: &str, args: BTreeMap<Vec<u8>, Value>) -> Vec<u8> {
    dict([
        (b"a", Value::Dict(args)),
        (b"q", Value::bytes(method.as_bytes())),
        (b"ro", Value::Int(1)),
        (b"t", Value::bytes(tid)),
        (b"y", Value::bytes(*b"q")),
    ])
    .encode()
}

/// A response, for the test network's nodes: `r` must include the node's `id`.
pub fn response(tid: &[u8], r: BTreeMap<Vec<u8>, Value>, observed: SocketAddr) -> Vec<u8> {
    dict([(b"ip", Value::bytes(compact(observed))), (b"r", Value::Dict(r)), (b"t", Value::bytes(tid)), (b"y", Value::bytes(*b"r"))]).encode()
}

/// An error answer, for the test network's nodes.
pub fn error(tid: &[u8], code: i64, message: &str) -> Vec<u8> {
    dict([
        (b"e", Value::List(vec![Value::Int(code), Value::bytes(message.as_bytes())])),
        (b"t", Value::bytes(tid)),
        (b"y", Value::bytes(*b"e")),
    ])
    .encode()
}

/// The `nodes` (IPv4) and `nodes6` (IPv6) of a response.
pub fn nodes(r: &Value) -> Vec<Node> {
    let mut found = Vec::new();
    if let Some(compact) = r.bytes_at(b"nodes") {
        found.extend(compact.chunks_exact(COMPACT_NODE_V4).filter_map(compact_node));
    }
    if let Some(compact) = r.bytes_at(b"nodes6") {
        found.extend(compact.chunks_exact(COMPACT_NODE_V6).filter_map(compact_node));
    }
    found
}

/// Compact node info for `nodes`/`nodes6` (IPv4 nodes in one, IPv6 in the other).
pub fn compact_nodes(nodes: &[Node], v6: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for node in nodes.iter().filter(|n| n.addr.is_ipv6() == v6) {
        out.extend_from_slice(&node.id);
        out.extend_from_slice(&compact(node.addr));
    }
    out
}

fn compact_node(chunk: &[u8]) -> Option<Node> {
    let id = chunk[..ID_LEN].try_into().ok()?;
    let addr = compact_addr(&chunk[ID_LEN..])?;
    // Port 0 can't be reached, and nodes that say so are broken or lying.
    (addr.port() != 0 && !addr.ip().is_unspecified()).then_some(Node { id, addr })
}

/// An address in compact form: 4 or 16 bytes of IP, then the port, big-endian.
pub fn compact(addr: SocketAddr) -> Vec<u8> {
    let mut out = match addr.ip().to_canonical() {
        IpAddr::V4(ip) => ip.octets().to_vec(),
        IpAddr::V6(ip) => ip.octets().to_vec(),
    };
    out.extend_from_slice(&addr.port().to_be_bytes());
    out
}

pub fn compact_addr(bytes: &[u8]) -> Option<SocketAddr> {
    match bytes.len() {
        6 => {
            let ip = Ipv4Addr::from(<[u8; 4]>::try_from(&bytes[..4]).ok()?);
            Some(SocketAddrV4::new(ip, u16::from_be_bytes([bytes[4], bytes[5]])).into())
        }
        18 => {
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&bytes[..16]).ok()?);
            Some(SocketAddr::new(IpAddr::V6(ip).to_canonical(), u16::from_be_bytes([bytes[16], bytes[17]])))
        }
        _ => None,
    }
}

/// XOR distance between two IDs, comparable as big-endian bytes: smaller is closer.
pub fn distance(a: &Id, b: &Id) -> Id {
    std::array::from_fn(|i| a[i] ^ b[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(entries: &[(&[u8], Value)]) -> BTreeMap<Vec<u8>, Value> {
        entries.iter().map(|(k, v)| (k.to_vec(), v.clone())).collect()
    }

    #[test]
    fn queries_are_read_only_and_decode() {
        let q = query(b"ab", "get", args(&[(b"id", Value::bytes([1u8; 20])), (b"target", Value::bytes([2u8; 20]))]));
        assert!(looks_like_krpc(&q));
        let m = decode(&q).unwrap();
        assert_eq!(m.tid, b"ab");
        let Body::Query { method, args } = m.body else { panic!() };
        assert_eq!(method, b"get");
        assert_eq!(args.bytes_at(b"target"), Some(&[2u8; 20][..]));
        assert_eq!(bencode::decode(&q).unwrap().int_at(b"ro"), Some(1));
        assert_eq!(bencode::decode(&q).unwrap().get(b"v"), None, "nothing says it's LanKVM");
    }

    #[test]
    fn responses_carry_the_observed_address() {
        let observed: SocketAddr = "203.0.113.7:47800".parse().unwrap();
        let r = response(b"xy", args(&[(b"id", Value::bytes([3u8; 20]))]), observed);
        assert!(looks_like_krpc(&r), "BEP 42 puts `ip` first");
        let m = decode(&r).unwrap();
        assert_eq!(m.observed, Some(observed));
        assert!(matches!(m.body, Body::Response(ref r) if r.bytes_at(b"id") == Some(&[3u8; 20][..])));

        let v6: SocketAddr = "[2001:db8::7]:47800".parse().unwrap();
        assert_eq!(decode(&response(b"xy", args(&[(b"id", Value::bytes([3u8; 20]))]), v6)).unwrap().observed, Some(v6));
    }

    #[test]
    fn errors_decode() {
        let m = decode(&error(b"zz", 302, "sequence number less than current")).unwrap();
        assert_eq!(m.body, Body::Error { code: 302, message: "sequence number less than current".into() });
    }

    #[test]
    fn compact_nodes_round_trip() {
        let list = [
            Node { id: [1; 20], addr: "198.51.100.1:6881".parse().unwrap() },
            Node { id: [2; 20], addr: "198.51.100.2:51413".parse().unwrap() },
            Node { id: [3; 20], addr: "[2001:db8::3]:6881".parse().unwrap() },
        ];
        let r = dict([(b"nodes", Value::bytes(compact_nodes(&list, false))), (b"nodes6", Value::bytes(compact_nodes(&list, true)))]);
        assert_eq!(nodes(&r), list);
    }

    #[test]
    fn nodes_at_port_zero_are_skipped() {
        let list = [Node { id: [1; 20], addr: "198.51.100.1:0".parse().unwrap() }];
        assert!(nodes(&dict([(b"nodes", Value::bytes(compact_nodes(&list, false)))])).is_empty());
    }

    #[test]
    fn quic_packets_dont_look_like_krpc() {
        // A short header whose first byte happens to be 'd' still needs "1:" and a key letter
        // right after, and a trailing 'e'.
        let mut short = vec![0x64, 0x12, 0x34];
        short.extend_from_slice(&[0xab; 40]);
        assert!(!looks_like_krpc(&short));
        assert!(!looks_like_krpc(b"d1:ae"), "too short");
    }

    #[test]
    fn distance_orders_by_xor() {
        let target = [0u8; 20];
        let mut near = [0u8; 20];
        near[19] = 1;
        let mut far = [0u8; 20];
        far[0] = 1;
        assert!(distance(&near, &target) < distance(&far, &target));
    }
}
