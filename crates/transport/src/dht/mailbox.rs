//! What two paired Macs leave for each other on the DHT, so they can meet with no server of
//! LanKVM's: a mailbox per pair, made of two BEP 44 items.
//!
//! - The host's note: where it can be reached (its public address as the DHT sees it, its
//!   router's mapping, its addresses on its own network), when it last said so, the last viewer
//!   request it answered, and which DHT nodes it watches for requests and keeps the note on (so
//!   the viewer needs one lookup, for the note, and never ends up writing where the host doesn't
//!   look). The host keeps it fresh while internet access is on.
//! - The viewer's note: a request to connect, with where the viewer can be reached and a random
//!   nonce. The host watches for it, punches toward the viewer when one comes (which opens its
//!   router for the viewer's packets), and answers in its own note.
//!
//! Everything comes from the access key the host gave this viewer (see [`crate::knock`]), which
//! only the two of them hold: the keys that sign each note, and the key that seals it
//! (ChaCha20-Poly1305), so the nodes storing a note learn nothing from it, and nobody else can
//! write one the other side accepts. The signing keys change every [`EPOCH_SECS`], so a note's
//! place on the DHT can't be followed from one day to the next.

use std::net::SocketAddr;

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce as AeadNonce, UnboundKey};
use ring::hkdf::{self, HKDF_SHA256, KeyType};
use ring::hmac;
use ring::signature::Ed25519KeyPair;
use serde::{Deserialize, Serialize};

use super::item::{MutableItem, PublicKey};
use super::krpc::Id;
use crate::knock::{AccessKey, random_bytes};

/// How long each epoch's signing keys last.
pub const EPOCH_SECS: u64 = 24 * 60 * 60;
/// How far apart two Macs' clocks may be: near the end of an epoch the host also keeps the next
/// one's mailbox, and near its start the last one's.
pub const SKEW_SECS: u64 = 10 * 60;
/// A request older (or newer) than this, by the host's clock, is ignored.
pub const REQUEST_FRESH_SECS: u64 = SKEW_SECS;
/// Addresses a note carries at most, and DHT nodes of each kind.
pub const MAX_ADDRS: usize = 12;
pub const MAX_NODES: usize = 12;
pub const NONCE_LEN: usize = 16;

const SALT: &[u8] = b"lankvm dht v1";
const LABEL_HOST: &[u8] = b"host note";
const LABEL_VIEWER: &[u8] = b"viewer note";
const LABEL_SEAL: &[u8] = b"seal";
const LABEL_PUNCH: &[u8] = b"punch";
const AEAD_NONCE_LEN: usize = 12;

pub type RequestNonce = [u8; NONCE_LEN];

/// The host's note.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostNote {
    /// When the host wrote it (Unix seconds, its clock).
    pub at: u64,
    /// The viewer request it answered last: it has punched toward that viewer.
    pub answered: Option<RequestNonce>,
    /// Where it can be reached, the likeliest first.
    pub addrs: Vec<SocketAddr>,
    /// The DHT nodes it watches for this viewer's requests, closest first: where to leave one.
    pub watch: Vec<SocketAddr>,
    /// The DHT nodes it keeps this note on: where its answer to a request appears.
    pub stored: Vec<SocketAddr>,
}

/// A viewer's request to connect.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerNote {
    /// When the viewer wrote it (Unix seconds, its clock).
    pub at: u64,
    pub nonce: RequestNonce,
    /// Where it can be reached, the likeliest first.
    pub addrs: Vec<SocketAddr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Host,
    Viewer,
}

impl Side {
    fn label(self) -> &'static [u8] {
        match self {
            Side::Host => LABEL_HOST,
            Side::Viewer => LABEL_VIEWER,
        }
    }
}

/// The epochs a Mac keeps the mailbox of at `now_unix`: the current one, and the one next to it
/// while within [`SKEW_SECS`] of the boundary.
pub fn epochs_around(now_unix: u64) -> Vec<u64> {
    let mut epochs = vec![now_unix / EPOCH_SECS];
    for edge in [now_unix.saturating_sub(SKEW_SECS) / EPOCH_SECS, (now_unix + SKEW_SECS) / EPOCH_SECS] {
        if !epochs.contains(&edge) {
            epochs.push(edge);
        }
    }
    epochs
}

pub fn epoch_at(now_unix: u64) -> u64 {
    now_unix / EPOCH_SECS
}

/// One pair's mailbox keys, from the host's access key for that viewer.
pub struct Mailbox {
    prk: hkdf::Prk,
    seal: LessSafeKey,
    punch: hmac::Key,
}

/// One note's place on the DHT in one epoch, and the key that signs it there.
pub struct Slot {
    pub epoch: u64,
    pub side: Side,
    keypair: Ed25519KeyPair,
    pub key: PublicKey,
}

impl Slot {
    /// Where the note is stored: SHA-1 of the key (no salt; the key changes every epoch).
    pub fn target(&self) -> Id {
        super::item::target(&self.key, &[])
    }
}

struct Len(usize);

impl KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

impl Mailbox {
    pub fn new(access_key: &AccessKey) -> Self {
        let prk = hkdf::Salt::new(HKDF_SHA256, SALT).extract(access_key);
        let mut seal = [0; 32];
        prk.expand(&[LABEL_SEAL], Len(32)).expect("HKDF length").fill(&mut seal).expect("HKDF fill");
        let seal = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &seal).expect("ChaCha20 key"));
        let mut punch = [0; 32];
        prk.expand(&[LABEL_PUNCH], Len(32)).expect("HKDF length").fill(&mut punch).expect("HKDF fill");
        Self { prk, seal, punch: hmac::Key::new(hmac::HMAC_SHA256, &punch) }
    }

    /// What the host's punches answering request `nonce` carry. The viewer sends `nonce` itself
    /// in the clear (in its own punches), so only the pair can make this from it: a punch with it
    /// comes from the host, and its source is where the host is.
    pub fn punch_nonce(&self, nonce: &RequestNonce) -> RequestNonce {
        hmac::sign(&self.punch, nonce).as_ref()[..NONCE_LEN].try_into().expect("HMAC-SHA256 is longer than a nonce")
    }

    /// The `side`'s note slot in `epoch`.
    pub fn slot(&self, side: Side, epoch: u64) -> Slot {
        let mut seed = [0; 32];
        let epoch_bytes = epoch.to_be_bytes();
        self.prk.expand(&[side.label(), &epoch_bytes], Len(32)).expect("HKDF length").fill(&mut seed).expect("HKDF fill");
        let keypair = Ed25519KeyPair::from_seed_unchecked(&seed).expect("any 32 bytes are an Ed25519 seed");
        let key = ring::signature::KeyPair::public_key(&keypair).as_ref().try_into().expect("Ed25519 keys are 32 bytes");
        Slot { epoch, side, keypair, key }
    }

    /// The host's note as an item for `slot`, at sequence number `seq`.
    pub fn host_item(&self, slot: &Slot, seq: i64, note: &HostNote) -> MutableItem {
        self.item(slot, seq, &trimmed_host(note))
    }

    pub fn viewer_item(&self, slot: &Slot, seq: i64, note: &ViewerNote) -> MutableItem {
        let mut note = note.clone();
        note.addrs.truncate(MAX_ADDRS);
        self.item(slot, seq, &note)
    }

    /// The host's note in `item`, read from `slot`; None if it isn't one sealed for this pair.
    pub fn read_host(&self, slot: &Slot, item: &MutableItem) -> Option<HostNote> {
        self.read(slot, item)
    }

    pub fn read_viewer(&self, slot: &Slot, item: &MutableItem) -> Option<ViewerNote> {
        self.read(slot, item)
    }

    fn item<T: Serialize>(&self, slot: &Slot, seq: i64, note: &T) -> MutableItem {
        let plain = postcard::to_stdvec(note).expect("notes serialize");
        let sealed = self.seal(slot, plain);
        MutableItem::sign(&slot.keypair, &[], seq, sealed).expect("a note with at most MAX_ADDRS addresses fits in an item")
    }

    fn read<T: for<'de> Deserialize<'de>>(&self, slot: &Slot, item: &MutableItem) -> Option<T> {
        if item.key != slot.key || !item.salt.is_empty() {
            return None;
        }
        let plain = self.open(slot, &item.value)?;
        postcard::from_bytes(&plain).ok()
    }

    fn seal(&self, slot: &Slot, mut plain: Vec<u8>) -> Vec<u8> {
        let nonce: [u8; AEAD_NONCE_LEN] = random_bytes();
        self.seal
            .seal_in_place_append_tag(AeadNonce::assume_unique_for_key(nonce), Aad::from(aad(slot)), &mut plain)
            .expect("ChaCha20-Poly1305 seals any note");
        [&nonce[..], &plain].concat()
    }

    fn open(&self, slot: &Slot, sealed: &[u8]) -> Option<Vec<u8>> {
        let (nonce, sealed) = sealed.split_first_chunk::<AEAD_NONCE_LEN>()?;
        let mut buf = sealed.to_vec();
        let plain = self.seal.open_in_place(AeadNonce::assume_unique_for_key(*nonce), Aad::from(aad(slot)), &mut buf).ok()?;
        Some(plain.to_vec())
    }
}

/// What a note's seal also covers: which side's note, for which epoch. A note can't be moved to
/// the other side's slot or another epoch's.
fn aad(slot: &Slot) -> Vec<u8> {
    [SALT, slot.side.label(), &slot.epoch.to_be_bytes()].concat()
}

fn trimmed_host(note: &HostNote) -> HostNote {
    let mut note = note.clone();
    note.addrs.truncate(MAX_ADDRS);
    note.watch.truncate(MAX_NODES);
    note.stored.truncate(MAX_NODES);
    note
}

/// The sequence number for an item written now: milliseconds since the Unix epoch, and always
/// above the last one this Mac wrote there (a clock that steps back must not make the DHT keep
/// the older note).
pub fn next_seq(last: Option<i64>, now_ms: i64) -> i64 {
    match last {
        Some(last) if last >= now_ms => last + 1,
        _ => now_ms,
    }
}

/// Unix time in milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// A fresh request nonce.
pub fn request_nonce() -> RequestNonce {
    random_bytes()
}

/// Whether a request written at `at` (the viewer's clock) is fresh at `now` (the host's).
pub fn fresh(at: u64, now: u64) -> bool {
    at.abs_diff(now) <= REQUEST_FRESH_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note() -> HostNote {
        HostNote {
            at: 1_700_000_000,
            answered: Some([5; NONCE_LEN]),
            addrs: vec!["203.0.113.5:47800".parse().unwrap(), "[2001:db8::5]:47800".parse().unwrap(), "192.168.1.20:47800".parse().unwrap()],
            watch: vec!["198.51.100.1:6881".parse().unwrap()],
            stored: vec!["198.51.100.2:51413".parse().unwrap()],
        }
    }

    #[test]
    fn notes_round_trip_sealed_and_signed() {
        let mailbox = Mailbox::new(&[7; 32]);
        let slot = mailbox.slot(Side::Host, 19_000);
        let item = mailbox.host_item(&slot, 42, &note());
        assert!(item.verify());
        assert_eq!(item.target(), slot.target());
        assert!(!item.value.windows(4).any(|w| w == [203, 0, 113, 5]), "addresses are sealed");
        assert_eq!(mailbox.read_host(&slot, &item), Some(note()));
    }

    #[test]
    fn only_the_pair_can_read_or_write() {
        let ours = Mailbox::new(&[7; 32]);
        let theirs = Mailbox::new(&[8; 32]);
        let slot = ours.slot(Side::Host, 19_000);
        let item = ours.host_item(&slot, 1, &note());
        assert_eq!(theirs.read_host(&theirs.slot(Side::Host, 19_000), &item), None);
        // A stranger can sign items for its own key, never for this pair's slot.
        let forged = theirs.host_item(&theirs.slot(Side::Host, 19_000), 2, &note());
        assert_eq!(ours.read_host(&slot, &forged), None);
    }

    #[test]
    fn notes_stay_in_their_slot() {
        let mailbox = Mailbox::new(&[7; 32]);
        let host = mailbox.slot(Side::Host, 19_000);
        let viewer = mailbox.slot(Side::Viewer, 19_000);
        assert_ne!(host.target(), viewer.target());
        assert_ne!(host.target(), mailbox.slot(Side::Host, 19_001).target(), "the slot moves every epoch");
        assert_eq!(host.key, mailbox.slot(Side::Host, 19_000).key, "and is the same for both Macs");

        let item = mailbox.host_item(&host, 1, &note());
        // Signed again for the viewer's slot, the seal still names the host's.
        let moved = MutableItem::sign(&viewer.keypair, &[], 1, item.value.clone()).unwrap();
        assert_eq!(mailbox.read_viewer(&viewer, &moved), None);
    }

    #[test]
    fn a_full_note_fits_in_an_item() {
        let mailbox = Mailbox::new(&[7; 32]);
        let v6: SocketAddr = "[2001:db8:ffff:ffff:ffff:ffff:ffff:ffff]:65535".parse().unwrap();
        let full = HostNote { at: u64::MAX, answered: Some([0xff; NONCE_LEN]), addrs: vec![v6; 40], watch: vec![v6; 40], stored: vec![v6; 40] };
        let slot = mailbox.slot(Side::Host, u64::MAX / EPOCH_SECS);
        let item = mailbox.host_item(&slot, i64::MAX, &full);
        let read = mailbox.read_host(&slot, &item).unwrap();
        assert_eq!((read.addrs.len(), read.watch.len(), read.stored.len()), (MAX_ADDRS, MAX_NODES, MAX_NODES));
    }

    #[test]
    fn only_the_pair_can_make_the_hosts_punch() {
        let ours = Mailbox::new(&[7; 32]);
        let nonce = [3; NONCE_LEN];
        assert_ne!(ours.punch_nonce(&nonce), nonce);
        assert_eq!(ours.punch_nonce(&nonce), Mailbox::new(&[7; 32]).punch_nonce(&nonce), "the same on both Macs");
        assert_ne!(ours.punch_nonce(&nonce), Mailbox::new(&[8; 32]).punch_nonce(&nonce));
    }

    #[test]
    fn epochs_overlap_near_their_boundary() {
        let start = 20_000 * EPOCH_SECS;
        assert_eq!(epochs_around(start + EPOCH_SECS / 2), vec![20_000]);
        assert_eq!(epochs_around(start + 60), vec![20_000, 19_999]);
        assert_eq!(epochs_around(start - 60), vec![19_999, 20_000]);
    }

    #[test]
    fn sequence_numbers_only_go_up() {
        assert_eq!(next_seq(None, 1000), 1000);
        assert_eq!(next_seq(Some(500), 1000), 1000);
        assert_eq!(next_seq(Some(1000), 1000), 1001);
        assert_eq!(next_seq(Some(5000), 1000), 5001, "the clock stepped back");
    }

    #[test]
    fn requests_are_fresh_within_the_skew() {
        assert!(fresh(1000, 1000 + REQUEST_FRESH_SECS));
        assert!(fresh(1000 + REQUEST_FRESH_SECS, 1000));
        assert!(!fresh(1000, 1001 + REQUEST_FRESH_SECS));
    }
}
