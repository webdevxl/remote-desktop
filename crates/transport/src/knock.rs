//! Knocks: how a paired viewer gets a host to answer it over the internet.
//!
//! A host open to the internet stays silent to every packet that doesn't carry a knock, so to
//! everyone else its port looks closed. The knock rides in the destination connection ID of the
//! viewer's first QUIC packet, which the client is free to choose (QUIC asks only for 8–20
//! unpredictable bytes): an 8-byte random nonce and a 12-byte tag over it, made with a key that
//! only the host and that one viewer know. The host derives each viewer's key from its own secret
//! and the viewer's fingerprint and hands it over on the local network, so revoking a viewer only
//! means dropping its key from the list the host checks.
//!
//! The tag also covers the current 10-minute period of the Unix clock, so a knock goes stale
//! within 10–30 minutes, and the host remembers each nonce with the address it first came from:
//! a knock copied off the wire and sent from somewhere else is ignored.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

use crate::identity::Fingerprint;

/// A viewer's key for knocking on one host.
pub type AccessKey = [u8; 32];

/// A knock fills a QUIC destination connection ID of the maximum length.
pub const KNOCK_LEN: usize = NONCE_LEN + TAG_LEN;

const ACCESS_LABEL: &[u8] = b"lankvm internet access v1";
const KNOCK_LABEL: &[u8] = b"lankvm knock v1";
const NONCE_LEN: usize = 8;
const TAG_LEN: usize = 12;
/// Knocks are tied to 10-minute periods. The host accepts the period on either side of its own,
/// so the two clocks may disagree by 10 minutes (up to 20, depending on where in the period).
const BUCKET_SECS: u64 = 600;
/// A knock is fresh for at most three periods, so a nonce is never forgotten while it could
/// still be replayed.
const REPLAY_TTL: Duration = Duration::from_secs(30 * 60);
const REPLAY_CAPACITY: usize = 4096;
/// Knock checks per second, and the burst allowed. A check costs a few microseconds of HMAC per
/// paired viewer; the limit keeps a flood of fake knocks from tying up the endpoint driver.
const CHECKS_PER_SEC: f64 = 2000.0;

/// The random half of a knock.
pub type Nonce = [u8; NONCE_LEN];

/// The key the host with `host_secret` gives `viewer` for knocking on it.
pub fn access_key(host_secret: &[u8; 32], viewer: &Fingerprint) -> AccessKey {
    let mut ctx = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, host_secret));
    ctx.update(ACCESS_LABEL);
    ctx.update(viewer);
    ctx.sign().as_ref().try_into().expect("HMAC-SHA256 is 32 bytes")
}

/// Seconds since the Unix epoch, the clock knocks are tied to.
pub fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A fresh knock (random nonce) for the host that gave out `key`, valid around `now_unix`.
pub fn knock_cid(key: &AccessKey, now_unix: u64) -> [u8; KNOCK_LEN] {
    knock_cid_with_nonce(key, now_unix, random_bytes())
}

pub(crate) fn knock_cid_with_nonce(key: &AccessKey, now_unix: u64, nonce: Nonce) -> [u8; KNOCK_LEN] {
    let mut cid = [0; KNOCK_LEN];
    cid[..NONCE_LEN].copy_from_slice(&nonce);
    cid[NONCE_LEN..].copy_from_slice(&tag(&hmac::Key::new(hmac::HMAC_SHA256, key), &nonce, now_unix / BUCKET_SECS));
    cid
}

fn tag(key: &hmac::Key, nonce: &[u8], bucket: u64) -> [u8; TAG_LEN] {
    let mut ctx = hmac::Context::with_key(key);
    ctx.update(KNOCK_LABEL);
    ctx.update(nonce);
    ctx.update(&bucket.to_be_bytes());
    ctx.sign().as_ref()[..TAG_LEN].try_into().expect("HMAC-SHA256 is longer than a tag")
}

/// The access keys of every viewer allowed in over the internet, ready for checking knocks.
pub struct KnockKeys(Vec<(Fingerprint, hmac::Key)>);

impl KnockKeys {
    pub fn new(keys: impl IntoIterator<Item = (Fingerprint, AccessKey)>) -> Self {
        Self(keys.into_iter().map(|(fp, key)| (fp, hmac::Key::new(hmac::HMAC_SHA256, &key))).collect())
    }

    /// The viewer whose key made `dcid` and the knock's nonce, if `dcid` is a knock from the
    /// period of `now_unix` or the one on either side. Checks only the tag: replays and the check
    /// budget are the [`Gate`](crate::gate::Gate)'s business.
    pub fn verify(&self, dcid: &[u8], now_unix: u64) -> Option<(Fingerprint, Nonce)> {
        if dcid.len() != KNOCK_LEN {
            return None;
        }
        let (nonce, knock_tag) = dcid.split_at(NONCE_LEN);
        let bucket = now_unix / BUCKET_SECS;
        let buckets = [Some(bucket), bucket.checked_sub(1), bucket.checked_add(1)];
        for (fp, key) in &self.0 {
            if buckets.iter().flatten().any(|&b| ct_eq(&tag(key, nonce, b), knock_tag)) {
                return Some((*fp, nonce.try_into().expect("nonce length")));
            }
        }
        None
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &(Fingerprint, hmac::Key)> {
        self.0.iter()
    }
}

/// Compares in a time that depends only on the length, so how fast a guess is rejected tells a
/// forger nothing about how close it was.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && std::hint::black_box(a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y))) == 0
}

/// Bytes from the system's secure random number generator.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    SystemRandom::new().fill(&mut bytes).expect("system RNG");
    bytes
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The nonce is new.
    First,
    /// The nonce came before from the same address: QUIC resends its first packet until the
    /// host answers, and the host checks the knock again when it decides to accept.
    Again,
    /// The nonce came before from a different address: someone copied the knock.
    Replayed,
}

/// Nonces of recent valid knocks (or rendezvous tokens, whose nonces are longer) and the address
/// each first came from.
#[derive(Default)]
pub(crate) struct ReplayCache<N = Nonce> {
    first_from: HashMap<N, SocketAddr>,
    /// How many of those nonces each address sent.
    knocks_from: HashMap<SocketAddr, usize>,
    /// Oldest first.
    order: VecDeque<(N, Instant)>,
}

impl<N: Copy + Eq + Hash> ReplayCache<N> {
    pub(crate) fn note(&mut self, nonce: N, remote: SocketAddr, now: Instant) -> Seen {
        self.expire(now);
        if let Some(&first) = self.first_from.get(&nonce) {
            return if first == remote { Seen::Again } else { Seen::Replayed };
        }
        if self.order.len() >= REPLAY_CAPACITY {
            self.forget_oldest();
        }
        self.first_from.insert(nonce, remote);
        *self.knocks_from.entry(remote).or_default() += 1;
        self.order.push_back((nonce, now));
        Seen::First
    }

    /// True if a valid knock first came from `remote` within the last [`REPLAY_TTL`]. A replayed
    /// knock doesn't count for the address it was replayed from.
    pub(crate) fn knocked_from(&mut self, remote: SocketAddr, now: Instant) -> bool {
        self.expire(now);
        self.knocks_from.contains_key(&remote)
    }

    fn expire(&mut self, now: Instant) {
        while let Some(&(_, at)) = self.order.front()
            && now.saturating_duration_since(at) >= REPLAY_TTL
        {
            self.forget_oldest();
        }
    }

    fn forget_oldest(&mut self) {
        let Some((nonce, _)) = self.order.pop_front() else { return };
        let Some(remote) = self.first_from.remove(&nonce) else { return };
        if let Entry::Occupied(mut count) = self.knocks_from.entry(remote) {
            *count.get_mut() -= 1;
            if *count.get() == 0 {
                count.remove();
            }
        }
    }
}

/// A token bucket of knock checks: [`CHECKS_PER_SEC`] a second, with a burst of as many.
pub(crate) struct Budget {
    tokens: f64,
    last: Instant,
}

impl Budget {
    pub(crate) fn new(now: Instant) -> Self {
        Self { tokens: CHECKS_PER_SEC, last: now }
    }

    /// Spends one check. False when the budget is used up.
    pub(crate) fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = self.last.max(now);
        self.tokens = (self.tokens + elapsed * CHECKS_PER_SEC).min(CHECKS_PER_SEC);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWER: Fingerprint = [7; 32];
    const OTHER_VIEWER: Fingerprint = [8; 32];
    const NOW: u64 = 1_790_000_000;

    fn keys_for(secret: &[u8; 32]) -> KnockKeys {
        KnockKeys::new([(VIEWER, access_key(secret, &VIEWER)), (OTHER_VIEWER, access_key(secret, &OTHER_VIEWER))])
    }

    #[test]
    fn access_keys_differ_per_viewer_and_host() {
        let a = access_key(&[1; 32], &VIEWER);
        assert_eq!(a, access_key(&[1; 32], &VIEWER));
        assert_ne!(a, access_key(&[1; 32], &OTHER_VIEWER));
        assert_ne!(a, access_key(&[2; 32], &VIEWER));
    }

    #[test]
    fn knock_round_trip_names_the_viewer() {
        let secret = [1; 32];
        let knock = knock_cid(&access_key(&secret, &OTHER_VIEWER), NOW);
        let (fp, nonce) = keys_for(&secret).verify(&knock, NOW).unwrap();
        assert_eq!(fp, OTHER_VIEWER);
        assert_eq!(nonce, knock[..NONCE_LEN]);
    }

    #[test]
    fn knocks_are_fresh_each_time() {
        let key = access_key(&[1; 32], &VIEWER);
        assert_ne!(knock_cid(&key, NOW), knock_cid(&key, NOW));
    }

    #[test]
    fn wrong_key_or_tampered_knock_fails() {
        let keys = keys_for(&[1; 32]);
        let knock = knock_cid(&access_key(&[2; 32], &VIEWER), NOW);
        assert_eq!(keys.verify(&knock, NOW), None);

        let mut tampered = knock_cid(&access_key(&[1; 32], &VIEWER), NOW);
        assert!(keys.verify(&tampered, NOW).is_some());
        tampered[0] ^= 1;
        assert_eq!(keys.verify(&tampered, NOW), None);
        assert_eq!(keys.verify(&tampered[..KNOCK_LEN - 1], NOW), None);
        assert_eq!(KnockKeys::new([]).verify(&knock, NOW), None);
    }

    #[test]
    fn neighbouring_periods_are_accepted_older_ones_are_not() {
        let keys = keys_for(&[1; 32]);
        let knock = knock_cid_with_nonce(&access_key(&[1; 32], &VIEWER), NOW, [3; NONCE_LEN]);
        for skew in [-(BUCKET_SECS as i64), 0, BUCKET_SECS as i64] {
            let now = NOW.checked_add_signed(skew).unwrap();
            assert_eq!(keys.verify(&knock, now).map(|(fp, _)| fp), Some(VIEWER), "skew {skew} s");
        }
        for skew in [-2 * BUCKET_SECS as i64, 2 * BUCKET_SECS as i64, 86_400] {
            let now = NOW.checked_add_signed(skew).unwrap();
            assert_eq!(keys.verify(&knock, now), None, "skew {skew} s");
        }
    }

    #[test]
    fn knock_at_the_start_of_time_still_verifies() {
        let knock = knock_cid(&access_key(&[1; 32], &VIEWER), 0);
        assert!(keys_for(&[1; 32]).verify(&knock, 0).is_some());
    }

    #[test]
    fn replay_cache_tells_retransmits_from_replays() {
        let mut cache = ReplayCache::default();
        let (a, b): (SocketAddr, SocketAddr) = ("203.0.113.7:5000".parse().unwrap(), "198.51.100.9:5000".parse().unwrap());
        let t0 = Instant::now();
        assert_eq!(cache.note([1; NONCE_LEN], a, t0), Seen::First);
        assert_eq!(cache.note([1; NONCE_LEN], a, t0), Seen::Again);
        assert_eq!(cache.note([1; NONCE_LEN], b, t0), Seen::Replayed);
        assert_eq!(cache.note([2; NONCE_LEN], b, t0), Seen::First);
        // Forgotten once no knock with that nonce can still be fresh.
        assert_eq!(cache.note([1; NONCE_LEN], b, t0 + REPLAY_TTL), Seen::First);
    }

    #[test]
    fn replay_cache_remembers_who_knocked() {
        let mut cache = ReplayCache::default();
        let (a, b): (SocketAddr, SocketAddr) = ("203.0.113.7:5000".parse().unwrap(), "198.51.100.9:5000".parse().unwrap());
        let t0 = Instant::now();
        assert!(!cache.knocked_from(a, t0));
        cache.note([1; NONCE_LEN], a, t0);
        cache.note([2; NONCE_LEN], a, t0 + Duration::from_secs(60));
        assert_eq!(cache.note([1; NONCE_LEN], b, t0), Seen::Replayed);
        assert!(cache.knocked_from(a, t0) && !cache.knocked_from(b, t0));
        // Until its last knock is forgotten.
        assert!(cache.knocked_from(a, t0 + REPLAY_TTL));
        assert!(!cache.knocked_from(a, t0 + REPLAY_TTL + Duration::from_secs(60)));
        assert!(cache.knocks_from.is_empty());
    }

    #[test]
    fn replay_cache_evicts_the_oldest_when_full() {
        let mut cache = ReplayCache::default();
        let addr: SocketAddr = "203.0.113.7:5000".parse().unwrap();
        let t0 = Instant::now();
        for i in 0..=REPLAY_CAPACITY as u64 {
            assert_eq!(cache.note(i.to_be_bytes(), addr, t0), Seen::First);
        }
        assert_eq!(cache.order.len(), REPLAY_CAPACITY);
        assert_eq!(cache.first_from.len(), REPLAY_CAPACITY);
        assert_eq!(cache.knocks_from[&addr], REPLAY_CAPACITY);
        assert_eq!(cache.note(0u64.to_be_bytes(), addr, t0), Seen::First);
        assert_eq!(cache.note((REPLAY_CAPACITY as u64).to_be_bytes(), addr, t0), Seen::Again);
    }

    #[test]
    fn budget_runs_out_and_refills() {
        let t0 = Instant::now();
        let mut budget = Budget::new(t0);
        for _ in 0..CHECKS_PER_SEC as usize {
            assert!(budget.take(t0));
        }
        assert!(!budget.take(t0));
        // 2000 a second: one more every half millisecond.
        assert!(budget.take(t0 + Duration::from_micros(500)));
        assert!(!budget.take(t0 + Duration::from_micros(500)));
        // Never more than the burst.
        let later = t0 + Duration::from_secs(60);
        for _ in 0..CHECKS_PER_SEC as usize {
            assert!(budget.take(later));
        }
        assert!(!budget.take(later));
    }
}
