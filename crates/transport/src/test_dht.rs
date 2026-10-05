//! A BitTorrent DHT in miniature for tests: nodes on loopback that answer `ping`, `find_node`,
//! `get` and `put` (BEP 5, 44) as libtorrent's do, including the `ip` key (BEP 42). Each node
//! knows every other, so lookups converge in a hop or two. Tests point Macs at these instead of
//! the real DHT, which no test may reach.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use ring::hmac;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::dht::bencode::Value;
use crate::dht::item::{MutableItem, PublicKey};
use crate::dht::krpc::{self, Body, Id, Node, distance};
use crate::knock::random_bytes;

/// Nodes a `find_node` or `get` answer names.
const NAMED: usize = 8;

/// A running test DHT. Dropping it stops every node.
pub struct TestDht {
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

#[derive(Default)]
struct Shared {
    nodes: Mutex<Vec<Node>>,
    /// Each node's items, by target.
    items: Mutex<HashMap<SocketAddr, HashMap<Id, MutableItem>>>,
    secret: Mutex<Option<hmac::Key>>,
    queries: AtomicU64,
    gets: AtomicU64,
    puts: AtomicU64,
    /// Nodes ignore `get` and `put`, as Transmission's do.
    no_items: AtomicBool,
    /// Nodes answer nothing at all.
    silent: AtomicBool,
}

impl TestDht {
    /// Starts `n` nodes on 127.0.0.1.
    pub async fn start(n: usize) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        *shared.secret.lock().unwrap() = Some(hmac::Key::new(hmac::HMAC_SHA256, &random_bytes::<32>()));
        let mut sockets = Vec::new();
        for _ in 0..n {
            let socket = UdpSocket::bind("127.0.0.1:0").await?;
            let node = Node { id: random_bytes(), addr: socket.local_addr()? };
            shared.nodes.lock().unwrap().push(node);
            sockets.push((node, socket));
        }
        let tasks = sockets.into_iter().map(|(node, socket)| tokio::spawn(serve(shared.clone(), node, socket))).collect();
        Ok(Self { shared, tasks })
    }

    pub fn addrs(&self) -> Vec<SocketAddr> {
        self.shared.nodes.lock().unwrap().iter().map(|n| n.addr).collect()
    }

    /// Where a client joins: two of the nodes, as `host:port` names.
    pub fn bootstrap(&self) -> Vec<String> {
        self.addrs().iter().take(2).map(SocketAddr::to_string).collect()
    }

    /// How many nodes hold an item under `key` (no salt).
    pub fn holding(&self, key: &PublicKey) -> usize {
        let target = crate::dht::item::target(key, &[]);
        self.shared.items.lock().unwrap().values().filter(|items| items.contains_key(&target)).count()
    }

    pub fn queries(&self) -> u64 {
        self.shared.queries.load(Relaxed)
    }

    pub fn gets(&self) -> u64 {
        self.shared.gets.load(Relaxed)
    }

    pub fn puts(&self) -> u64 {
        self.shared.puts.load(Relaxed)
    }

    /// Nodes stop knowing `get` and `put` (they answer neither).
    pub fn set_no_items(&self, on: bool) {
        self.shared.no_items.store(on, Relaxed);
    }

    /// Nodes stop answering anything: the DHT is out of reach.
    pub fn set_silent(&self, on: bool) {
        self.shared.silent.store(on, Relaxed);
    }

    /// Forgets every item stored.
    pub fn clear(&self) {
        self.shared.items.lock().unwrap().clear();
    }
}

impl Drop for TestDht {
    fn drop(&mut self) {
        self.tasks.iter().for_each(JoinHandle::abort);
    }
}

async fn serve(shared: Arc<Shared>, me: Node, socket: UdpSocket) {
    let mut buf = vec![0; 2048];
    loop {
        let Ok((len, from)) = socket.recv_from(&mut buf).await else { return };
        if shared.silent.load(Relaxed) {
            continue;
        }
        if let Some(answer) = shared.answer(me, &buf[..len], from) {
            let _ = socket.send_to(&answer, from).await;
        }
    }
}

impl Shared {
    fn answer(&self, me: Node, datagram: &[u8], from: SocketAddr) -> Option<Vec<u8>> {
        let message = krpc::decode(datagram)?;
        let Body::Query { method, args } = message.body else { return None };
        self.queries.fetch_add(1, Relaxed);
        let tid = message.tid;
        let mut r: BTreeMap<Vec<u8>, Value> = BTreeMap::from([(b"id".to_vec(), Value::bytes(me.id))]);
        match method.as_slice() {
            b"ping" => {}
            b"find_node" => {
                let target: Id = args.bytes_at(b"target")?.try_into().ok()?;
                r.insert(b"nodes".to_vec(), Value::bytes(krpc::compact_nodes(&self.closest(&target, me), false)));
            }
            b"get" if !self.no_items.load(Relaxed) => {
                self.gets.fetch_add(1, Relaxed);
                let target: Id = args.bytes_at(b"target")?.try_into().ok()?;
                r.insert(b"nodes".to_vec(), Value::bytes(krpc::compact_nodes(&self.closest(&target, me), false)));
                r.insert(b"token".to_vec(), Value::bytes(self.token(from)));
                let newer_than = args.int_at(b"seq");
                if let Some(item) = self.items.lock().unwrap().get(&me.addr).and_then(|items| items.get(&target))
                    && newer_than.is_none_or(|seq| item.seq > seq)
                {
                    r.insert(b"k".to_vec(), Value::bytes(item.key));
                    r.insert(b"seq".to_vec(), Value::Int(item.seq));
                    r.insert(b"sig".to_vec(), Value::bytes(item.signature));
                    r.insert(b"v".to_vec(), Value::bytes(item.value.clone()));
                }
            }
            b"put" if !self.no_items.load(Relaxed) => {
                self.puts.fetch_add(1, Relaxed);
                if args.bytes_at(b"token")? != self.token(from) {
                    return Some(krpc::error(&tid, 203, "bad token"));
                }
                let item = MutableItem {
                    key: args.bytes_at(b"k")?.try_into().ok()?,
                    salt: args.bytes_at(b"salt").unwrap_or_default().to_vec(),
                    seq: args.int_at(b"seq")?,
                    value: args.bytes_at(b"v")?.to_vec(),
                    signature: args.bytes_at(b"sig")?.try_into().ok()?,
                };
                if !item.verify() {
                    return Some(krpc::error(&tid, 206, "invalid signature"));
                }
                let mut items = self.items.lock().unwrap();
                let stored = items.entry(me.addr).or_default();
                if stored.get(&item.target()).is_some_and(|old| old.seq > item.seq) {
                    return Some(krpc::error(&tid, 302, "sequence number less than current"));
                }
                stored.insert(item.target(), item);
            }
            _ => return None,
        }
        Some(krpc::response(&tid, r, from))
    }

    fn closest(&self, target: &Id, me: Node) -> Vec<Node> {
        let mut nodes: Vec<Node> = self.nodes.lock().unwrap().iter().copied().filter(|n| *n != me).collect();
        nodes.sort_by_key(|n| distance(&n.id, target));
        nodes.truncate(NAMED);
        nodes
    }

    fn token(&self, from: SocketAddr) -> Vec<u8> {
        let secret = self.secret.lock().unwrap();
        hmac::sign(secret.as_ref().expect("secret"), from.to_string().as_bytes()).as_ref()[..8].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use ring::signature::Ed25519KeyPair;
    use tokio::sync::mpsc;

    use super::*;
    use crate::dht::{Config, Dht};

    /// A client on a plain UDP socket.
    async fn client(dht: &TestDht) -> (Dht, SocketAddr) {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = socket.local_addr().unwrap();
        let (tx, rx) = mpsc::channel(256);
        let reader = socket.clone();
        tokio::spawn(async move {
            let mut buf = vec![0; 2048];
            while let Ok((len, from)) = reader.recv_from(&mut buf).await {
                if tx.send((from, buf[..len].to_vec())).await.is_err() {
                    return;
                }
            }
        });
        let config = Config { bootstrap: dht.bootstrap(), seeds: Vec::new(), allow_local: true };
        (Dht::start(tokio::runtime::Handle::current(), socket, rx, config), addr)
    }

    #[tokio::test]
    async fn items_go_round_through_the_test_network() {
        let net = TestDht::start(30).await.unwrap();
        let (writer, writer_addr) = client(&net).await;
        let (reader, _) = client(&net).await;

        assert!(writer.bootstrap().await >= crate::dht::client::K);
        assert_eq!(writer.observed().addr, Some(writer_addr), "the nodes say where they saw it");

        let keypair = Ed25519KeyPair::from_seed_unchecked(&[3; 32]).unwrap();
        let item = MutableItem::sign(&keypair, &[], 10, b"hello".to_vec()).unwrap();
        let (none, closest) = writer.get_mutable(&item.key, &[]).await;
        assert_eq!(none, None);
        assert!(closest.len() >= crate::dht::client::K);
        assert_eq!(writer.put_mutable(&item, &closest).await, closest.len());
        assert_eq!(net.holding(&item.key), closest.len());

        let (got, found) = reader.get_mutable(&item.key, &[]).await;
        assert_eq!(got, Some(item.clone()));
        let nodes: Vec<SocketAddr> = found.iter().map(|f| f.node.addr).collect();
        assert_eq!(reader.poll_mutable(&nodes, &item.key, &[], Some(10)).await, None, "nothing newer");
        let newer = MutableItem::sign(&keypair, &[], 11, b"again".to_vec()).unwrap();
        writer.put_mutable(&newer, &closest).await;
        assert_eq!(reader.poll_mutable(&nodes, &item.key, &[], Some(10)).await, Some(newer));
    }

    #[tokio::test]
    async fn items_are_stored_at_nodes_found_before() {
        let net = TestDht::start(20).await.unwrap();
        let (writer, _) = client(&net).await;
        let keypair = Ed25519KeyPair::from_seed_unchecked(&[5; 32]).unwrap();
        let first = MutableItem::sign(&keypair, &[], 1, b"one".to_vec()).unwrap();
        let (_, storing) = writer.get_mutable(&first.key, &[]).await;
        let nodes: Vec<SocketAddr> = storing.iter().map(|f| f.node.addr).collect();
        assert_eq!(writer.store_at(&first, &nodes).await, nodes.len(), "a token from each, then the item");
        let (reader, _) = client(&net).await;
        assert_eq!(reader.poll_mutable(&nodes, &first.key, &[], None).await, Some(first));
    }

    #[tokio::test]
    async fn older_items_are_refused() {
        let net = TestDht::start(12).await.unwrap();
        let (writer, _) = client(&net).await;
        let keypair = Ed25519KeyPair::from_seed_unchecked(&[4; 32]).unwrap();
        let newer = MutableItem::sign(&keypair, &[], 5, b"new".to_vec()).unwrap();
        let (_, closest) = writer.get_mutable(&newer.key, &[]).await;
        assert!(writer.put_mutable(&newer, &closest).await > 0);
        let older = MutableItem::sign(&keypair, &[], 4, b"old".to_vec()).unwrap();
        assert_eq!(writer.put_mutable(&older, &closest).await, 0);
    }

    #[tokio::test]
    async fn a_silent_network_ends_lookups_in_time() {
        let net = TestDht::start(10).await.unwrap();
        net.set_silent(true);
        let (client, _) = client(&net).await;
        let started = tokio::time::Instant::now();
        let (item, found) = client.get_mutable(&[1; 32], &[]).await;
        assert!(item.is_none() && found.is_empty());
        assert!(started.elapsed() < Duration::from_secs(12), "{:?}", started.elapsed());
    }
}
