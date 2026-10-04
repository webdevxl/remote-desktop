//! Connection IDs the endpoint can recognise as its own.
//!
//! Every connection ID this endpoint hands out, as host or as viewer, is a 3-byte random nonce
//! and a 5-byte tag over it, keyed with a secret made fresh for each process. The socket filter
//! (see [`crate::gate`]) lets packets from the internet through when they carry one, since they
//! continue a handshake or session this endpoint is part of, and quinn drops short-header packets
//! for unknown connections without answering when the tag doesn't check out. Eight bytes, like
//! quinn's default, so packet sizes stay the same.

use std::sync::Arc;
use std::time::Duration;

use quinn::{ConnectionId, ConnectionIdGenerator};
use quinn_proto::InvalidCid;
use ring::hmac;

use crate::knock::{ct_eq, random_bytes};

pub const CID_LEN: usize = NONCE_LEN + TAG_LEN;

const LABEL: &[u8] = b"lankvm cid v1";
const NONCE_LEN: usize = 3;
const TAG_LEN: usize = 5;

/// The per-process key behind the endpoint's connection IDs.
#[derive(Debug)]
pub struct CidKey(hmac::Key);

impl CidKey {
    pub fn random() -> Self {
        Self(hmac::Key::new(hmac::HMAC_SHA256, &random_bytes::<32>()))
    }

    /// True if `cid` is one this key made (or one of the 1 in 2^40 random guesses that pass).
    pub fn is_ours(&self, cid: &[u8]) -> bool {
        cid.len() == CID_LEN && ct_eq(&self.tag(&cid[..NONCE_LEN]), &cid[NONCE_LEN..])
    }

    fn generate(&self) -> [u8; CID_LEN] {
        let nonce: [u8; NONCE_LEN] = random_bytes();
        let mut cid = [0; CID_LEN];
        cid[..NONCE_LEN].copy_from_slice(&nonce);
        cid[NONCE_LEN..].copy_from_slice(&self.tag(&nonce));
        cid
    }

    fn tag(&self, nonce: &[u8]) -> [u8; TAG_LEN] {
        let mut ctx = hmac::Context::with_key(&self.0);
        ctx.update(LABEL);
        ctx.update(nonce);
        ctx.sign().as_ref()[..TAG_LEN].try_into().expect("HMAC-SHA256 is longer than a tag")
    }
}

/// quinn's connection ID generator for the endpoint, sharing its key with the socket filter.
pub struct MacCidGenerator {
    key: Arc<CidKey>,
}

impl MacCidGenerator {
    pub fn new(key: Arc<CidKey>) -> Self {
        Self { key }
    }
}

impl ConnectionIdGenerator for MacCidGenerator {
    fn generate_cid(&mut self) -> ConnectionId {
        ConnectionId::new(&self.key.generate())
    }

    fn validate(&self, cid: &ConnectionId) -> Result<(), InvalidCid> {
        if self.key.is_ours(cid) { Ok(()) } else { Err(InvalidCid) }
    }

    fn cid_len(&self) -> usize {
        CID_LEN
    }

    fn cid_lifetime(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_validate_and_look_random() {
        let key = Arc::new(CidKey::random());
        let mut generator = MacCidGenerator::new(key.clone());
        let a = generator.generate_cid();
        let b = generator.generate_cid();
        assert_eq!(a.len(), generator.cid_len());
        assert_ne!(a, b);
        assert!(generator.validate(&a).is_ok() && generator.validate(&b).is_ok());
        assert!(key.is_ours(&a));
    }

    #[test]
    fn forged_or_foreign_ids_fail() {
        let key = Arc::new(CidKey::random());
        let generator = MacCidGenerator::new(key.clone());
        let theirs = CidKey::random().generate();
        assert!(generator.validate(&ConnectionId::new(&theirs)).is_err());

        let mut forged = key.generate();
        forged[CID_LEN - 1] ^= 1;
        assert!(!key.is_ours(&forged));
        forged = key.generate();
        forged[0] ^= 1;
        assert!(!key.is_ours(&forged));

        let ours = key.generate();
        assert!(!key.is_ours(&ours[..CID_LEN - 1]));
        assert!(!key.is_ours(&[ours.as_slice(), &[0]].concat()));
        assert!(!key.is_ours(&[]));
        // Random guesses: a 40-bit tag lets about one in a trillion through.
        let rejected = (0..10_000).filter(|_| !key.is_ours(&random_bytes::<CID_LEN>())).count();
        assert_eq!(rejected, 10_000);
    }
}
