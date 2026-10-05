//! A small client for the BitTorrent DHT ("Mainline", BEP 5), enough to store and fetch BEP 44
//! mutable items: the millions of nodes torrent clients run hold a Mac's encrypted note for its
//! paired Macs, so no server of LanKVM's has to. It joins the IPv4 DHT only: nodes named at IPv6
//! addresses (`nodes6`, BEP 32) are ignored, as BEP 42 can't vouch for their IDs here.
//!
//! It only asks: it finds nodes close to a target by asking ever closer nodes (iterative lookup),
//! gets and puts items there, and learns its own public address from what the nodes say they saw
//! (BEP 42). It never answers queries (BEP 43 read-only), keeps no Kademlia buckets (a client
//! that doesn't serve needs none: it remembers the nodes that answered and starts each lookup
//! from the closest it knows), and sends nothing on its own between lookups.
//!
//! Datagrams go out through a [`Wire`] (the QUIC socket, so the router's mapping for it is the
//! one the nodes see) and come in through a channel the socket's gate feeds.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout};

use super::bencode::Value;
use super::item::{MutableItem, PublicKey};
use super::krpc::{self, Body, Id, Node, distance};
use crate::knock::random_bytes;

/// The nodes torrent clients join the DHT through. Only used when no node from an earlier run
/// answers. (On 2026-10-04 the first two answered from here; the BitTorrent and uTorrent
/// routers, BEP 5's own, didn't.)
pub const DEFAULT_BOOTSTRAP: &[&str] =
    &["dht.libtorrent.org:25401", "dht.transmissionbt.com:6881", "router.bittorrent.com:6881", "router.utorrent.com:6881"];

/// Kademlia's k: how many nodes a `find_node` answer names.
pub const K: usize = 8;
/// How many of the closest nodes a lookup makes sure it has heard from before it ends. More
/// than k: two Macs' lookups for one target must end at the same nodes, or one stores an item
/// where the other never looks.
const CONVERGE: usize = 16;
/// A node whose ID shares more leading bits than this with the target is a "spy" that made its
/// ID up after seeing the target asked for: with 10–20 million nodes, the closest real one shares
/// about 24, and one in a few hundred targets has a node sharing 32. Skipped.
const SPY_BITS: u32 = 32;
/// How many of the closest nodes that answered a lookup returns, and an item is stored on: more
/// than k, as nodes come and go and two Macs' lookups for one target end at slightly different
/// sets (pkarr stores on 20 too).
pub const STORE_ON: usize = 20;
/// Queries a lookup keeps going at once, not counting slow ones. Most nodes other nodes name
/// close to a target never answer (gone, or behind a router that only lets in nodes they talked
/// to), and each costs a timeout, so a lookup asks many at once.
const ALPHA: usize = 8;
/// A query not answered by then is slow: the lookup asks another node meanwhile, but still takes
/// its answer if it comes before [`LOOKUP_QUERY_TIMEOUT`]. Most nodes answer within a few hundred
/// ms.
const SLOW_AFTER: Duration = Duration::from_millis(400);
/// Then a node counts as gone, for a lookup...
const LOOKUP_QUERY_TIMEOUT: Duration = Duration::from_millis(1200);
/// ...and for a query to a node that answered before (a get or a put).
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// The most a lookup may take, and `find_node` queries it may send (most go to nodes that never
/// answer).
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(8);
const LOOKUP_QUERIES: usize = 300;
/// Nodes remembered. Lookups add the nodes others name, so this bounds memory.
const TABLE_CAPACITY: usize = 2048;
/// How long a node that answered counts as alive.
const NODE_FRESH: Duration = Duration::from_secs(15 * 60);
/// What counts toward this Mac's public address: the last word of each network (/24, or /48 for
/// IPv6) among the latest answers, within this long. Only the latest, so a new address (another
/// network, a router that remapped) takes over within a lookup or a few polls.
const VOTE_WINDOW: Duration = Duration::from_secs(10 * 60);
const RECENT_VOTES: usize = 32;
/// Seeds asked at a start before the bootstrap nodes (the most recent first): when they still
/// answer, the bootstrap nodes are never needed.
const SEEDS_FIRST: usize = 16;
/// How long a failed resolution of the bootstrap nodes' names is kept before trying again.
const RESOLVE_RETRY: Duration = Duration::from_secs(30);
/// Nodes that answer before the client counts as joined.
const JOINED: usize = K;
/// How long resolving a bootstrap node's name may take.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(4);

/// Sends a datagram from the socket the DHT uses.
pub trait Wire: Send + Sync + 'static {
    fn send<'a>(&'a self, to: SocketAddr, datagram: &'a [u8]) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'a>>;
}

impl Wire for tokio::net::UdpSocket {
    fn send<'a>(&'a self, to: SocketAddr, datagram: &'a [u8]) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'a>> {
        Box::pin(async move { self.send_to(datagram, to).await.map(drop) })
    }
}

/// Where a client starts.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Names (`host:port`) of nodes to join through, when the seeds don't answer.
    pub bootstrap: Vec<String>,
    /// Nodes that answered in an earlier run, the most recent first: tried first.
    pub seeds: Vec<SocketAddr>,
    /// Nodes at local addresses (loopback, private, link-local) count: a test DHT. Otherwise a
    /// node naming one is ignored, so no DHT node can make this Mac send to its own network.
    pub allow_local: bool,
}

/// A node that answered a lookup's query, with its answer.
#[derive(Clone, Debug)]
pub struct Found {
    pub node: Node,
    /// What lets this Mac store an item there (from `get` and `get_peers`).
    pub token: Option<Vec<u8>>,
    pub response: Value,
}

/// This Mac's public address, as the nodes that answered saw it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observed {
    /// The address most nodes saw, once two agree (or the only one heard).
    pub addr: Option<SocketAddr>,
    /// Nodes it rests on.
    pub agreeing: usize,
    /// Nodes heard from in all.
    pub reporters: usize,
    /// Nodes saw this Mac's public IP at different ports: its router gives each destination
    /// a port of its own ("symmetric" NAT), and the port others see is no use to a peer.
    pub varies: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub queries: u64,
    pub responses: u64,
    pub timeouts: u64,
    /// Nodes known, and those that answered lately.
    pub known: usize,
    pub alive: usize,
}

/// Why a query got no answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Response(Value),
    /// The node's error (BEP 5 and 44 codes: 201 generic, 202 server, 203 protocol, 204 method
    /// unknown, 205 too large, 206 bad signature, 301 CAS mismatch, 302 sequence too low).
    Error(i64, String),
}

/// A DHT client. Cheap to clone; stops when the last clone is dropped.
#[derive(Clone)]
pub struct Dht {
    inner: Arc<Inner>,
}

struct Inner {
    wire: Arc<dyn Wire>,
    id: Id,
    config: Config,
    pending: Mutex<HashMap<[u8; 4], Pending>>,
    table: Mutex<Table>,
    votes: Mutex<VecDeque<Vote>>,
    queries: AtomicU64,
    responses: AtomicU64,
    timeouts: AtomicU64,
    /// The bootstrap nodes' addresses, once resolved, and when (one lookup resolves them, the
    /// others wait), and whether that is under way in the background.
    entry_points: tokio::sync::Mutex<Option<(Instant, Vec<SocketAddr>)>>,
    resolving: AtomicBool,
    /// When a node last answered (wall clock: a Mac asleep hears nothing).
    last_answer: Mutex<Option<std::time::SystemTime>>,
}

struct Pending {
    to: SocketAddr,
    reply: oneshot::Sender<Reply>,
}

struct Vote {
    reporter: IpAddr,
    observed: SocketAddr,
    at: Instant,
}

impl Dht {
    /// Starts a client that reads what the socket received for it from `inbound`. Its node ID is
    /// random: each run is a stranger to the DHT.
    pub fn start(rt: Handle, wire: Arc<dyn Wire>, inbound: mpsc::Receiver<(SocketAddr, Vec<u8>)>, config: Config) -> Self {
        let inner = Arc::new(Inner {
            wire,
            id: random_bytes(),
            config,
            pending: Mutex::default(),
            table: Mutex::default(),
            votes: Mutex::default(),
            queries: AtomicU64::new(0),
            responses: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            entry_points: tokio::sync::Mutex::new(None),
            resolving: AtomicBool::new(false),
            last_answer: Mutex::new(None),
        });
        rt.spawn(receive(Arc::downgrade(&inner), inbound));
        Self { inner }
    }

    pub fn id(&self) -> Id {
        self.inner.id
    }

    pub fn stats(&self) -> Stats {
        let table = self.inner.table.lock().unwrap();
        Stats {
            queries: self.inner.queries.load(Relaxed),
            responses: self.inner.responses.load(Relaxed),
            timeouts: self.inner.timeouts.load(Relaxed),
            known: table.nodes.len(),
            alive: table.alive(Instant::now()),
        }
    }

    /// Nodes that answered lately, the most recent first: seeds for the next run.
    pub fn alive_nodes(&self, n: usize) -> Vec<SocketAddr> {
        self.inner.table.lock().unwrap().recent(n, Instant::now())
    }

    pub fn observed(&self) -> Observed {
        observed(&self.inner.votes.lock().unwrap(), Instant::now())
    }

    /// Joins the DHT, if it hasn't heard from enough nodes lately: a lookup for its own ID,
    /// which fills its table with nodes that answer, so later lookups start close. Not needed
    /// before a lookup (any lookup joins on the way); it warms the client up. The number of nodes
    /// alive after.
    pub async fn bootstrap(&self) -> usize {
        if self.inner.table.lock().unwrap().alive(Instant::now()) < JOINED {
            let closest = self.lookup(self.inner.id).await;
            tracing::debug!(closest = closest.len(), "DHT: joined");
        }
        self.inner.table.lock().unwrap().alive(Instant::now())
    }

    /// The nodes closest to `target` that answer, closest first (at most [`STORE_ON`]).
    pub async fn lookup(&self, target: Id) -> Vec<Node> {
        self.search(target, None).await.closest
    }

    /// Looks up the nodes closest to where items under `key` and `salt` are stored, asking each
    /// of the closest for the item as it finds them. The newest valid item they had, and those
    /// that answered `get` (closest first, at most [`STORE_ON`], with the tokens a put there
    /// needs).
    pub async fn get_mutable(&self, key: &PublicKey, salt: &[u8]) -> (Option<MutableItem>, Vec<Found>) {
        let search = self.search(super::item::target(key, salt), Some(ItemSearch { key, salt, enough: None })).await;
        (search.item, search.storing)
    }

    /// Like [`Self::get_mutable`], but stops as soon as an item `enough` accepts comes: for when
    /// any copy of an item will do, and seconds count. The nodes are those that answered by then.
    pub async fn find_mutable(&self, key: &PublicKey, salt: &[u8], enough: &(dyn Fn(&MutableItem) -> bool + Sync)) -> (Option<MutableItem>, Vec<Found>) {
        let search = self.search(super::item::target(key, salt), Some(ItemSearch { key, salt, enough: Some(enough) })).await;
        (search.item, search.storing)
    }

    /// The most recent seeds, then the bootstrap nodes' addresses (resolved once; again after
    /// a while if nothing came back), then the other seeds. With seeds to start from, a lookup
    /// doesn't wait for the bootstrap nodes' names: they're resolved meanwhile, for later.
    async fn entry_points(&self) -> Vec<SocketAddr> {
        let allow_local = self.inner.config.allow_local;
        let seeds: Vec<SocketAddr> = self.inner.config.seeds.iter().copied().filter(|a| a.is_ipv4() && (allow_local || routable(*a))).collect();
        let mut resolved = self.inner.entry_points.lock().await;
        let stale = match &*resolved {
            None => true,
            Some((at, found)) => found.is_empty() && at.elapsed() >= RESOLVE_RETRY,
        };
        if stale && seeds.is_empty() {
            *resolved = Some((Instant::now(), resolve_bootstrap(&self.inner.config.bootstrap).await));
        } else if stale && !self.inner.resolving.swap(true, Relaxed) {
            let inner = self.inner.clone();
            tokio::spawn(async move {
                let found = resolve_bootstrap(&inner.config.bootstrap).await;
                *inner.entry_points.lock().await = Some((Instant::now(), found));
                inner.resolving.store(false, Relaxed);
            });
        }
        let first = seeds.len().min(SEEDS_FIRST);
        let mut points = seeds[..first].to_vec();
        points.extend(resolved.iter().flat_map(|(_, found)| found.iter().copied()));
        points.extend_from_slice(&seeds[first..]);
        points
    }

    /// How long ago a node last answered, by the wall clock; None if none has.
    pub fn since_answer(&self) -> Option<Duration> {
        let at = (*self.inner.last_answer.lock().unwrap())?;
        Some(std::time::SystemTime::now().duration_since(at).unwrap_or_default())
    }

    /// Asks `nodes` directly (no lookup) for the item under `key` and `salt`, if newer than
    /// `seq`: how a Mac watches an item it found the nodes for already. With `seq`, the first
    /// newer item any node has comes back at once (nodes only answer with a newer one); without,
    /// the newest of all their answers.
    pub async fn poll_mutable(&self, nodes: &[SocketAddr], key: &PublicKey, salt: &[u8], seq: Option<i64>) -> Option<MutableItem> {
        let target = super::item::target(key, salt);
        let mut asks = JoinSet::new();
        for &addr in nodes {
            let dht = self.clone();
            asks.spawn(async move { dht.query(addr, "get", get_args(&dht.inner.id, &target, seq), QUERY_TIMEOUT).await });
        }
        let mut items = Vec::new();
        while let Some(reply) = asks.join_next().await {
            if let Ok(Some(Reply::Response(r))) = reply
                && let Some(item) = MutableItem::from_response(&r, key, salt)
                && seq.is_none_or(|seq| item.seq > seq)
            {
                if seq.is_some() {
                    return Some(item);
                }
                items.push(item);
            }
        }
        newest(items.into_iter())
    }

    /// Stores `item` on the nodes of a lookup for its target (each with the token it gave). The
    /// number that took it.
    pub async fn put_mutable(&self, item: &MutableItem, at: &[Found]) -> usize {
        let mut puts = JoinSet::new();
        for found in at.iter().take(STORE_ON) {
            let Some(token) = found.token.clone() else { continue };
            let (dht, item, addr) = (self.clone(), item.clone(), found.node.addr);
            puts.spawn(async move {
                let mut args = BTreeMap::new();
                args.insert(b"id".to_vec(), Value::bytes(dht.inner.id));
                args.insert(b"k".to_vec(), Value::bytes(item.key));
                if !item.salt.is_empty() {
                    args.insert(b"salt".to_vec(), Value::bytes(item.salt.clone()));
                }
                args.insert(b"seq".to_vec(), Value::Int(item.seq));
                args.insert(b"sig".to_vec(), Value::bytes(item.signature));
                args.insert(b"token".to_vec(), Value::Bytes(token));
                args.insert(b"v".to_vec(), Value::Bytes(item.value.clone()));
                dht.query(addr, "put", args, QUERY_TIMEOUT).await
            });
        }
        let mut stored = 0;
        while let Some(reply) = puts.join_next().await {
            match reply {
                Ok(Some(Reply::Response(_))) => stored += 1,
                Ok(Some(Reply::Error(code, message))) => tracing::debug!(code, message, "DHT: put refused"),
                _ => {}
            }
        }
        stored
    }

    /// Stores `item` on `nodes` directly (no lookup): asks each for a token, then puts the item
    /// there, each node on its own (a slow one holds up no other). For nodes found earlier, whose
    /// tokens may have gone stale (nodes accept them for 10 minutes or so). The number that took
    /// it.
    pub async fn store_at(&self, item: &MutableItem, nodes: &[SocketAddr]) -> usize {
        let target = item.target();
        let mut stores = JoinSet::new();
        for &addr in nodes {
            let (dht, item) = (self.clone(), item.clone());
            stores.spawn(async move {
                let Some(Reply::Response(r)) = dht.query(addr, "get", get_args(&dht.inner.id, &target, None), QUERY_TIMEOUT).await else {
                    return false;
                };
                let id = r.bytes_at(b"id").and_then(|id| Id::try_from(id).ok()).unwrap_or(UNKNOWN_ID);
                let token = r.bytes_at(b"token").map(<[u8]>::to_vec);
                dht.put_mutable(&item, &[Found { node: Node { id, addr }, token, response: r }]).await == 1
            });
        }
        let mut stored = 0;
        while let Some(done) = stores.join_next().await {
            stored += usize::from(done.unwrap_or(false));
        }
        stored
    }

    /// Sends one query and waits up to `wait` for its answer. None if none came (or it couldn't
    /// be sent).
    pub async fn query(&self, to: SocketAddr, method: &str, args: BTreeMap<Vec<u8>, Value>, wait: Duration) -> Option<Reply> {
        let to = SocketAddr::new(to.ip().to_canonical(), to.port());
        let (tx, rx) = oneshot::channel();
        let tid = {
            let mut pending = self.inner.pending.lock().unwrap();
            let mut tid: [u8; 4] = random_bytes();
            while pending.contains_key(&tid) {
                tid = random_bytes();
            }
            pending.insert(tid, Pending { to, reply: tx });
            tid
        };
        let _forget = Forget { inner: &self.inner, tid };
        self.inner.queries.fetch_add(1, Relaxed);
        if let Err(e) = self.inner.wire.send(to, &krpc::query(&tid, method, args)).await {
            tracing::debug!(%to, "DHT: couldn't send: {e}");
            self.inner.table.lock().unwrap().failed(to);
            return None;
        }
        match timeout(wait, rx).await {
            Ok(Ok(reply)) => Some(reply),
            _ => {
                self.inner.timeouts.fetch_add(1, Relaxed);
                self.inner.table.lock().unwrap().failed(to);
                None
            }
        }
    }

    /// The iterative lookup: asks ever closer nodes for the nodes closest to `target`
    /// (`find_node`, which every node answers, unlike `get`), starting from the closest it knows
    /// (and from the seeds and bootstrap nodes too while it knows few that answer). Looking for an
    /// item, it also asks each of the closest that answer for it, as soon as they do.
    async fn search(&self, target: Id, item: Option<ItemSearch<'_>>) -> Search {
        let started = Instant::now();
        let deadline = started + LOOKUP_TIMEOUT;
        let (mut start, alive) = {
            let table = self.inner.table.lock().unwrap();
            let now = Instant::now();
            (table.closest(&target, 3 * K, now), table.alive(now))
        };
        if alive < JOINED {
            start.extend(self.entry_points().await.into_iter().map(unknown));
        }
        let mut candidates: BTreeMap<Id, Candidate> = BTreeMap::new();
        let mut seen: HashSet<SocketAddr> = HashSet::new();
        let (own, allow_local) = (self.inner.id, self.inner.config.allow_local);
        for (i, node) in start.into_iter().enumerate() {
            add_candidate(&mut candidates, &mut seen, &target, node, i, &own, allow_local);
        }
        let mut asks = JoinSet::new();
        let mut sent = 0;
        let mut newest: Option<MutableItem> = None;
        let get_seq = None;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let active = candidates.values().filter(|c| c.active(now)).count();
            // Ask the closest nodes not asked yet, while fewer than ALPHA queries are pending.
            let mut to_ask = ALPHA.saturating_sub(active);
            for (key, candidate) in candidates.iter_mut() {
                if to_ask == 0 || sent >= LOOKUP_QUERIES {
                    break;
                }
                if candidate.state != State::New {
                    continue;
                }
                candidate.state = State::Asked(now);
                to_ask -= 1;
                sent += 1;
                let (dht, key, addr) = (self.clone(), *key, candidate.node.addr);
                asks.spawn(async move {
                    let reply = dht.query(addr, "find_node", find_node_args(&dht.inner.id, &target), LOOKUP_QUERY_TIMEOUT).await;
                    (key, Kind::FindNode, reply)
                });
            }
            // Most of the nodes closest to a target don't keep items (don't know `get`), so the
            // closest that answered are asked for the item, once each, until STORE_ON of them
            // have answered `get`: where items are stored and looked for. Only nodes whose ID
            // follows from their address (BEP 42): one that picked its ID next to a target could
            // otherwise take all the places an item goes, and drop it.
            let mut gets_done = true;
            if item.is_some() {
                let mut storing = 0;
                for (key, candidate) in candidates.iter_mut().filter(|(_, c)| c.state == State::Answered && id_matches_ip(&c.node)) {
                    if storing >= STORE_ON {
                        break;
                    }
                    match candidate.get {
                        Get::NotAsked => {
                            candidate.get = Get::Asked;
                            gets_done = false;
                            let (dht, key, addr) = (self.clone(), *key, candidate.node.addr);
                            asks.spawn(async move {
                                let reply = dht.query(addr, "get", get_args(&dht.inner.id, &target, get_seq), QUERY_TIMEOUT).await;
                                (key, Kind::Get, reply)
                            });
                        }
                        Get::Asked => gets_done = false,
                        Get::Answered(_) => storing += 1,
                        Get::Failed => {}
                    }
                }
            }
            // With nothing in flight, nothing more can come. With only slow queries in flight and
            // nobody left to ask, they are worth waiting for: their answers may name closer nodes.
            if (lookup_done(&candidates) && gets_done) || asks.is_empty() {
                break;
            }
            // Wake for the next answer, for the next query to turn slow, or at the deadline.
            let next_slow = candidates
                .values()
                .filter_map(|c| match c.state {
                    State::Asked(at) if c.active(now) => Some(at + SLOW_AFTER),
                    _ => None,
                })
                .min()
                .unwrap_or(deadline)
                .min(deadline);
            tokio::select! {
                answer = asks.join_next() => {
                    let Some(Ok((key, kind, reply))) = answer else { continue };
                    let Some(candidate) = candidates.get_mut(&key) else { continue };
                    let named = match (kind, reply) {
                        (Kind::FindNode, Some(Reply::Response(r))) => {
                            candidate.state = State::Answered;
                            krpc::nodes(&r)
                        }
                        // An error or no answer.
                        (Kind::FindNode, _) => {
                            candidate.state = State::Failed;
                            Vec::new()
                        }
                        (Kind::Get, Some(Reply::Response(r))) => {
                            let named = krpc::nodes(&r);
                            let found = Found { node: candidate.node, token: r.bytes_at(b"token").map(<[u8]>::to_vec), response: r };
                            if let Some(search) = &item
                                && let Some(got) = MutableItem::from_response(&found.response, search.key, search.salt)
                            {
                                let stop = search.enough.is_some_and(|enough| enough(&got));
                                if newest.as_ref().is_none_or(|n| got.seq > n.seq) {
                                    newest = Some(got);
                                }
                                candidate.get = Get::Answered(Box::new(found));
                                if stop {
                                    break;
                                }
                            } else {
                                candidate.get = Get::Answered(Box::new(found));
                            }
                            named
                        }
                        // A node that doesn't know `get` (or didn't answer this time).
                        (Kind::Get, _) => {
                            candidate.get = Get::Failed;
                            Vec::new()
                        }
                    };
                    let base = candidates.len();
                    for (i, node) in named.into_iter().enumerate() {
                        add_candidate(&mut candidates, &mut seen, &target, node, base + i, &own, allow_local);
                    }
                }
                _ = sleep_until(next_slow) => {}
            }
        }
        if tracing::enabled!(tracing::Level::TRACE) {
            let now = Instant::now();
            let shared = |d: &Id| d.iter().position(|&b| b != 0).map_or(160, |i| i as u32 * 8 + d[i].leading_zeros());
            let states: Vec<String> = candidates
                .iter()
                .take(40)
                .map(|(k, c)| {
                    let find = match c.state {
                        State::New => "N",
                        State::Asked(_) if c.active(now) => "a",
                        State::Asked(_) => "s",
                        State::Answered => "A",
                        State::Failed => "F",
                    };
                    let get = match c.get {
                        Get::NotAsked => "",
                        Get::Asked => "?",
                        Get::Answered(_) => "+",
                        Get::Failed => "-",
                    };
                    format!("{}{find}{get}", shared(k))
                })
                .collect();
            tracing::trace!(sent, ms = started.elapsed().as_millis() as u64, done = lookup_done(&candidates), "DHT: lookup: {}", states.join(" "));
        }
        let mut closest = Vec::new();
        let mut storing = Vec::new();
        for candidate in candidates.into_values() {
            if candidate.state == State::Answered && closest.len() < STORE_ON {
                closest.push(candidate.node);
            }
            if let Get::Answered(found) = candidate.get
                && storing.len() < STORE_ON
            {
                storing.push(*found);
            }
        }
        Search { closest, storing, item: newest }
    }
}

/// What a search looks for besides the closest nodes: items under `key` and `salt`; with
/// `enough`, it stops at the first item that will do.
struct ItemSearch<'a> {
    key: &'a PublicKey,
    salt: &'a [u8],
    enough: Option<&'a (dyn Fn(&MutableItem) -> bool + Sync)>,
}

struct Search {
    closest: Vec<Node>,
    storing: Vec<Found>,
    item: Option<MutableItem>,
}

#[derive(Clone, Copy)]
enum Kind {
    FindNode,
    Get,
}

/// Removes a query's pending entry however it ends.
struct Forget<'a> {
    inner: &'a Inner,
    tid: [u8; 4],
}

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.inner.pending.lock().unwrap().remove(&self.tid);
    }
}

#[derive(Debug)]
struct Candidate {
    node: Node,
    state: State,
    get: Get,
}

impl Candidate {
    /// Asked, and not slow yet.
    fn active(&self, now: Instant) -> bool {
        matches!(self.state, State::Asked(at) if now < at + SLOW_AFTER)
    }
}

/// Where a candidate is with `find_node`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    New,
    Asked(Instant),
    Answered,
    Failed,
}

/// Where a candidate is with `get`, when a search looks for an item.
#[derive(Debug, PartialEq)]
enum Get {
    NotAsked,
    Asked,
    Answered(Box<Found>),
    Failed,
}

impl PartialEq for Found {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node && self.token == other.token && self.response == other.response
    }
}

/// Done when [`CONVERGE`] nodes have answered and no node closer than the farthest of them is
/// still to be asked or may still answer. A slow node doesn't stop the lookup from asking others
/// meanwhile, but its answer is waited for: it may name the closest nodes of all, and two Macs
/// whose lookups gave up on different slow nodes end up in different places.
fn lookup_done(candidates: &BTreeMap<Id, Candidate>) -> bool {
    let mut answered = 0;
    for candidate in candidates.values() {
        match candidate.state {
            State::Answered => {
                answered += 1;
                if answered == CONVERGE {
                    return true;
                }
            }
            State::New | State::Asked(_) => return false,
            State::Failed => {}
        }
    }
    false
}

/// Adds `node` to a lookup's candidates, once per address. A node of unknown ID (a seed, or a
/// bootstrap node) sorts after every real one, in the order given (`i`): asked first only while
/// nothing better is known. Nodes at local addresses are skipped unless `allow_local`.
fn add_candidate(candidates: &mut BTreeMap<Id, Candidate>, seen: &mut HashSet<SocketAddr>, target: &Id, node: Node, i: usize, own: &Id, allow_local: bool) {
    if node.id == *own || !node.addr.is_ipv4() || !(allow_local || routable(node.addr)) || !seen.insert(node.addr) {
        return;
    }
    let shared = |d: Id| d.iter().position(|&b| b != 0).map_or(160, |i| i as u32 * 8 + d[i].leading_zeros());
    if node.id != UNKNOWN_ID && target != own && shared(distance(&node.id, target)) > SPY_BITS {
        return;
    }
    let key = if node.id == UNKNOWN_ID {
        let mut key = [0xff; 20];
        key[12..20].copy_from_slice(&(i as u64).to_be_bytes());
        key
    } else {
        distance(&node.id, target)
    };
    candidates.entry(key).or_insert(Candidate { node, state: State::New, get: Get::NotAsked });
}

/// Whether `addr` is one a DHT node can have: a global unicast address. Not loopback, private,
/// link-local, shared (100.64.0.0/10), multicast, broadcast or reserved.
pub fn routable(addr: SocketAddr) -> bool {
    match addr.ip().to_canonical() {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_multicast()
                || ip.is_broadcast()
                || ip.is_unspecified()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && o[1] & 0xc0 == 64))
        }
        IpAddr::V6(ip) => ip.segments()[0] & 0xe000 == 0x2000,
    }
}

/// Whether `node`'s ID follows BEP 42 (its first 21 bits derive from its IPv4 address), so it
/// can't have picked an ID close to a target it wants to sit next to. Nodes at addresses BEP 42
/// leaves out (local ones, IPv6 here) pass.
pub fn id_matches_ip(node: &Node) -> bool {
    let IpAddr::V4(ip) = node.addr.ip().to_canonical() else { return true };
    if !routable(node.addr) {
        return true;
    }
    let r = u32::from(node.id[19] & 0x07);
    let crc = crc32c(&((u32::from(ip) & 0x030f_3fff) | (r << 29)).to_be_bytes());
    node.id[0] == (crc >> 24) as u8 && node.id[1] == (crc >> 16) as u8 && node.id[2] & 0xf8 == (crc >> 8) as u8 & 0xf8
}

/// CRC-32C (Castagnoli), which BEP 42 derives node IDs with.
fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0x82f6_3b78 } else { crc >> 1 };
        }
    }
    !crc
}

/// Stands for the ID of a node known only by its address.
const UNKNOWN_ID: Id = [0; 20];

fn unknown(addr: SocketAddr) -> Node {
    Node { id: UNKNOWN_ID, addr }
}

fn find_node_args(id: &Id, target: &Id) -> BTreeMap<Vec<u8>, Value> {
    BTreeMap::from([(b"id".to_vec(), Value::bytes(*id)), (b"target".to_vec(), Value::bytes(*target))])
}

fn get_args(id: &Id, target: &Id, seq: Option<i64>) -> BTreeMap<Vec<u8>, Value> {
    let mut args = BTreeMap::from([(b"id".to_vec(), Value::bytes(*id)), (b"target".to_vec(), Value::bytes(*target))]);
    if let Some(seq) = seq {
        args.insert(b"seq".to_vec(), Value::Int(seq));
    }
    args
}

fn newest(items: impl Iterator<Item = MutableItem>) -> Option<MutableItem> {
    items.max_by_key(|item| item.seq)
}

/// Hands each answer to the query that waits for it; drops everything else, queries included.
async fn receive(inner: Weak<Inner>, mut inbound: mpsc::Receiver<(SocketAddr, Vec<u8>)>) {
    while let Some((from, datagram)) = inbound.recv().await {
        let Some(inner) = inner.upgrade() else { return };
        let Some(message) = krpc::decode(&datagram) else { continue };
        let reply = match message.body {
            Body::Response(r) => Reply::Response(r),
            Body::Error { code, message } => Reply::Error(code, message),
            Body::Query { .. } => continue,
        };
        let from = SocketAddr::new(from.ip().to_canonical(), from.port());
        let Ok(tid) = <[u8; 4]>::try_from(message.tid.as_slice()) else { continue };
        let pending = {
            let mut pending = inner.pending.lock().unwrap();
            // Only from where the query went: anyone can send a datagram claiming to answer.
            match pending.get(&tid) {
                Some(p) if p.to == from => pending.remove(&tid),
                _ => None,
            }
        };
        let Some(pending) = pending else { continue };
        inner.responses.fetch_add(1, Relaxed);
        *inner.last_answer.lock().unwrap() = Some(std::time::SystemTime::now());
        if let Reply::Response(r) = &reply {
            let now = Instant::now();
            let mut table = inner.table.lock().unwrap();
            if let Some(id) = r.bytes_at(b"id").and_then(|id| Id::try_from(id).ok()) {
                table.answered(Node { id, addr: from }, now);
            }
            for node in krpc::nodes(r).into_iter().filter(|n| n.addr.is_ipv4() && (inner.config.allow_local || routable(n.addr))) {
                table.named(node);
            }
            drop(table);
            // The IPv4 address, from IPv4 nodes: the one the IPv4 DHT can vouch for.
            if let Some(observed) = message.observed.filter(|o| o.is_ipv4() && from.is_ipv4()) {
                let mut votes = inner.votes.lock().unwrap();
                votes.push_back(Vote { reporter: from.ip(), observed, at: now });
                while votes.len() > RECENT_VOTES {
                    votes.pop_front();
                }
            }
        }
        let _ = pending.reply.send(reply);
    }
}

fn observed(votes: &VecDeque<Vote>, now: Instant) -> Observed {
    // Each network's latest word counts once: many nodes on one network (a Sybil's) are one vote.
    let mut latest: HashMap<IpAddr, (SocketAddr, usize)> = HashMap::new();
    for (i, vote) in votes.iter().enumerate().filter(|(_, v)| now.saturating_duration_since(v.at) < VOTE_WINDOW) {
        latest.insert(network_of(vote.reporter), (vote.observed, i));
    }
    // An address's votes, and when it was last said: a tie goes to the newer word.
    let mut tally: HashMap<SocketAddr, (usize, usize)> = HashMap::new();
    for &(addr, i) in latest.values() {
        let entry = tally.entry(addr).or_default();
        entry.0 += 1;
        entry.1 = entry.1.max(i);
    }
    let reporters = latest.len();
    let Some((&best, &(agreeing, _))) = tally.iter().max_by_key(|(_, count)| **count) else {
        return Observed::default();
    };
    let ports: HashSet<u16> = latest.values().filter(|(a, _)| a.ip() == best.ip()).map(|(a, _)| a.port()).collect();
    // Most nodes agreeing on one port, with an odd one out, is a mapping that changed (or a node
    // that is wrong); no two agreeing on any is a router that picks a port per destination.
    let varies = ports.len() > 1 && agreeing * 2 <= reporters;
    let addr = (agreeing >= 2 || reporters == 1).then_some(best);
    Observed { addr, agreeing, reporters, varies }
}

/// The network an address is on, for counting votes: its /24, or /48 for IPv6.
fn network_of(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V4(v4) => IpAddr::V4((u32::from(v4) & 0xffff_ff00).into()),
        IpAddr::V6(v6) => IpAddr::V6((u128::from(v6) & !((1u128 << 80) - 1)).into()),
    }
}

async fn resolve_bootstrap(names: &[String]) -> Vec<SocketAddr> {
    let found = resolve_all(names).await;
    if found.is_empty() && !names.is_empty() {
        tracing::info!("DHT: no bootstrap node could be found");
    }
    found
}

async fn resolve_all(names: &[String]) -> Vec<SocketAddr> {
    let mut lookups = JoinSet::new();
    for name in names.iter().cloned() {
        lookups.spawn(async move {
            match timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host(name.clone())).await {
                // The DHT this client joins is the IPv4 one.
                Ok(Ok(addrs)) => addrs.filter(SocketAddr::is_ipv4).collect::<Vec<_>>(),
                Ok(Err(e)) => {
                    tracing::debug!(name, "DHT: couldn't resolve a bootstrap node: {e}");
                    Vec::new()
                }
                Err(_) => Vec::new(),
            }
        });
    }
    let mut all = Vec::new();
    while let Some(addrs) = lookups.join_next().await {
        all.extend(addrs.unwrap_or_default());
    }
    all
}

/// The nodes this client knows: those that answered, and those others named.
#[derive(Default)]
struct Table {
    nodes: HashMap<SocketAddr, Entry>,
}

#[derive(Clone, Copy)]
struct Entry {
    id: Id,
    answered: Option<Instant>,
    failures: u32,
}

impl Table {
    fn answered(&mut self, node: Node, now: Instant) {
        self.make_room();
        let entry = self.nodes.entry(node.addr).or_insert(Entry { id: node.id, answered: None, failures: 0 });
        *entry = Entry { id: node.id, answered: Some(now), failures: 0 };
    }

    fn named(&mut self, node: Node) {
        if self.nodes.contains_key(&node.addr) {
            return;
        }
        self.make_room();
        self.nodes.insert(node.addr, Entry { id: node.id, answered: None, failures: 0 });
    }

    fn failed(&mut self, addr: SocketAddr) {
        if let Some(entry) = self.nodes.get_mut(&addr) {
            entry.failures += 1;
            if entry.failures >= 2 {
                self.nodes.remove(&addr);
            }
        }
    }

    fn alive(&self, now: Instant) -> usize {
        self.nodes.values().filter(|e| e.answered.is_some_and(|at| now.saturating_duration_since(at) < NODE_FRESH)).count()
    }

    fn recent(&self, n: usize, now: Instant) -> Vec<SocketAddr> {
        let mut alive: Vec<(Instant, SocketAddr)> = self
            .nodes
            .iter()
            .filter_map(|(addr, e)| e.answered.filter(|at| now.saturating_duration_since(*at) < NODE_FRESH).map(|at| (at, *addr)))
            .collect();
        alive.sort_by(|a, b| b.0.cmp(&a.0));
        alive.into_iter().take(n).map(|(_, addr)| addr).collect()
    }

    /// The `n` nodes closest to `target`, those that answered lately before those only named.
    fn closest(&self, target: &Id, n: usize, now: Instant) -> Vec<Node> {
        let fresh = |e: &Entry| e.answered.is_some_and(|at| now.saturating_duration_since(at) < NODE_FRESH);
        let mut nodes: Vec<(bool, Id, Node)> =
            self.nodes.iter().map(|(addr, e)| (!fresh(e), distance(&e.id, target), Node { id: e.id, addr: *addr })).collect();
        nodes.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        nodes.into_iter().take(n).map(|(_, _, node)| node).collect()
    }

    /// Forgets nodes only named, then the stalest, when full.
    fn make_room(&mut self) {
        if self.nodes.len() < TABLE_CAPACITY {
            return;
        }
        let victim = self
            .nodes
            .iter()
            .min_by_key(|(_, e)| (e.answered.is_some(), e.answered))
            .map(|(addr, _)| *addr);
        if let Some(addr) = victim {
            self.nodes.remove(&addr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vote(reporter: &str, observed: &str) -> Vote {
        Vote { reporter: reporter.parse().unwrap(), observed: observed.parse().unwrap(), at: Instant::now() }
    }

    #[test]
    fn public_address_needs_two_nodes_to_agree() {
        let now = Instant::now();
        assert_eq!(observed(&VecDeque::new(), now), Observed::default());
        let one = VecDeque::from([vote("198.51.100.1", "203.0.113.5:47800")]);
        assert_eq!(observed(&one, now).addr, Some("203.0.113.5:47800".parse().unwrap()), "the only word there is");
        let disagree = VecDeque::from([vote("198.51.100.1", "203.0.113.5:47800"), vote("198.51.101.2", "203.0.113.5:61000")]);
        let o = observed(&disagree, now);
        assert_eq!(o.addr, None);
        assert!(o.varies);
        let agree = VecDeque::from([
            vote("198.51.100.1", "203.0.113.5:47800"),
            vote("198.51.101.2", "203.0.113.5:47800"),
            vote("198.51.102.3", "203.0.113.5:47800"),
            vote("198.51.103.4", "203.0.113.5:1234"),
        ]);
        let o = observed(&agree, now);
        assert_eq!(o.addr, Some("203.0.113.5:47800".parse().unwrap()));
        assert_eq!((o.agreeing, o.reporters, o.varies), (3, 4, false), "one odd node out isn't a symmetric NAT");
    }

    #[test]
    fn each_network_votes_once_with_its_latest_word() {
        let now = Instant::now();
        let votes = VecDeque::from([
            vote("198.51.100.1", "203.0.113.5:1"),
            vote("198.51.100.1", "203.0.113.5:2"),
            vote("198.51.100.1", "203.0.113.5:47800"),
            vote("198.51.101.2", "203.0.113.5:47800"),
        ]);
        let o = observed(&votes, now);
        assert_eq!((o.addr, o.agreeing, o.reporters), (Some("203.0.113.5:47800".parse().unwrap()), 2, 2));
        // Many nodes in one /24 are one voice.
        let sybils: VecDeque<Vote> = (1..=20).map(|i| vote(&format!("192.0.2.{i}"), "198.18.0.1:666")).chain([vote("198.51.100.1", "203.0.113.5:47800")]).collect();
        let o = observed(&sybils, now);
        assert_eq!((o.addr, o.reporters), (None, 2));
    }

    #[test]
    fn a_tie_goes_to_the_newer_address() {
        let now = Instant::now();
        let mut votes: VecDeque<Vote> = (1..=3).map(|i| vote(&format!("198.51.{i}.1"), "192.0.2.1:1000")).collect();
        votes.extend((4..=6).map(|i| vote(&format!("198.51.{i}.1"), "203.0.113.9:2000")));
        assert_eq!(observed(&votes, now).addr, Some("203.0.113.9:2000".parse().unwrap()));
    }

    #[test]
    fn lookups_end_when_the_closest_have_answered() {
        let node = |b: u8| Node { id: [b; 20], addr: SocketAddr::from(([10, 0, 0, b], 6881)) };
        let answered = || State::Answered;
        let now = Instant::now();
        let mut candidates = BTreeMap::new();
        let n = CONVERGE as u8;
        for b in 1..=n + 3 {
            candidates.insert([b; 20], Candidate { node: node(b), state: if b <= n { answered() } else { State::New }, get: Get::NotAsked });
        }
        assert!(lookup_done(&candidates));
        candidates.get_mut(&[3; 20]).unwrap().state = State::Failed;
        assert!(!lookup_done(&candidates), "the next closest is in the running now");
        candidates.get_mut(&[n + 1; 20]).unwrap().state = State::Asked(now);
        assert!(!lookup_done(&candidates), "and may answer, slow or not");
        candidates.get_mut(&[n + 1; 20]).unwrap().state = State::Failed;
        candidates.get_mut(&[n + 2; 20]).unwrap().state = answered();
        assert!(lookup_done(&candidates));
        assert!(!lookup_done(&BTreeMap::new()));
    }

    #[test]
    fn unknown_nodes_sort_after_real_ones_in_the_order_given() {
        let (mut candidates, mut seen) = (BTreeMap::new(), HashSet::new());
        let target = [0u8; 20];
        let mut add = |node: Node, i: usize| add_candidate(&mut candidates, &mut seen, &target, node, i, &[7; 20], false);
        add(unknown("198.51.100.1:6881".parse().unwrap()), 0);
        add(unknown("198.51.100.5:6881".parse().unwrap()), 1);
        add(Node { id: [0xfe; 20], addr: "198.51.100.2:6881".parse().unwrap() }, 2);
        add(unknown("198.51.100.1:6881".parse().unwrap()), 3);
        add(Node { id: [7; 20], addr: "198.51.100.3:6881".parse().unwrap() }, 4);
        let mut spy = [0u8; 20];
        spy[19] = 1;
        add(Node { id: spy, addr: "198.51.100.4:6881".parse().unwrap() }, 5);
        add(Node { id: [0x10; 20], addr: "127.0.0.1:631".parse().unwrap() }, 6);
        add(Node { id: [0x10; 20], addr: "192.168.1.1:53".parse().unwrap() }, 7);
        add(Node { id: [0x10; 20], addr: "[2001:db8::66]:6881".parse().unwrap() }, 8);
        let order: Vec<_> = candidates.values().map(|c| c.node.addr.to_string()).collect();
        assert_eq!(
            order,
            ["198.51.100.2:6881", "198.51.100.1:6881", "198.51.100.5:6881"],
            "once per address, never itself, a spy, a local address or an IPv6 one"
        );
    }

    #[test]
    fn only_global_addresses_are_routable() {
        for ok in ["8.8.8.8:1", "198.51.100.1:6881", "[2001:4860::8888]:1"] {
            assert!(routable(ok.parse().unwrap()), "{ok}");
        }
        for bad in ["127.0.0.1:1", "10.0.0.1:1", "192.168.1.1:1", "169.254.1.1:1", "100.64.0.1:1", "224.0.0.251:5353", "255.255.255.255:1", "0.1.2.3:1", "240.0.0.1:1", "[::1]:1", "[fe80::1]:1", "[fd00::1]:1", "[::ffff:10.0.0.1]:1"] {
            assert!(!routable(bad.parse().unwrap()), "{bad}");
        }
    }

    #[test]
    fn crc32c_matches_the_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    /// BEP 42's examples: IP, the random byte, and the ID's first three bytes.
    #[test]
    fn node_ids_follow_bep_42() {
        for (ip, rand, prefix) in [
            ("124.31.75.21", 1u8, [0x5f, 0xbf, 0xbf]),
            ("21.75.31.124", 86, [0x5a, 0x3c, 0xe9]),
            ("65.23.51.170", 22, [0xa5, 0xd4, 0x32]),
            ("84.124.73.14", 65, [0x1b, 0x03, 0x21]),
            ("43.213.53.83", 90, [0xe5, 0x6f, 0x6c]),
        ] {
            let mut id = [0u8; 20];
            id[..3].copy_from_slice(&prefix);
            id[19] = rand;
            let node = Node { id, addr: format!("{ip}:6881").parse().unwrap() };
            assert!(id_matches_ip(&node), "{ip}");
            let mut wrong = node;
            wrong.id[0] ^= 0x80;
            assert!(!id_matches_ip(&wrong), "{ip}");
        }
        assert!(id_matches_ip(&Node { id: [0; 20], addr: "192.168.1.5:6881".parse().unwrap() }), "local addresses are exempt");
    }

    #[test]
    fn the_table_keeps_answering_nodes_over_named_ones() {
        let mut table = Table::default();
        let now = Instant::now();
        for i in 0..TABLE_CAPACITY as u32 {
            table.named(Node { id: [1; 20], addr: SocketAddr::from((i.to_be_bytes(), 6881)) });
        }
        let good = Node { id: [2; 20], addr: "192.0.2.200:6881".parse().unwrap() };
        table.answered(good, now);
        assert_eq!(table.nodes.len(), TABLE_CAPACITY);
        assert_eq!(table.closest(&[0; 20], 1, now), vec![good]);
        assert_eq!(table.recent(5, now), vec![good.addr]);
        table.failed(good.addr);
        table.failed(good.addr);
        assert!(table.recent(5, now).is_empty());
    }
}
