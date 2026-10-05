//! BEP 44 mutable items: up to 1000 bytes the DHT keeps for a couple of hours under the SHA-1
//! of an Ed25519 public key and a salt, signed with that key. Nodes check the signature and keep
//! only the highest sequence number, so only the key's holder can change the item, and nobody
//! can roll it back.

use ring::digest;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

use super::bencode::{self, Value};
use super::krpc::Id;

/// BEP 44's limit on a value, bencoded.
pub const MAX_VALUE_LEN: usize = 1000;
/// BEP 44's limit on a salt.
pub const MAX_SALT_LEN: usize = 64;

pub type PublicKey = [u8; 32];
pub type Signature = [u8; 64];

/// A mutable item, signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MutableItem {
    pub key: PublicKey,
    pub salt: Vec<u8>,
    pub seq: i64,
    /// The value, a byte string (bencoded on the wire).
    pub value: Vec<u8>,
    pub signature: Signature,
}

/// Where items signed by `key` with `salt` are stored: SHA-1(key ‖ salt).
pub fn target(key: &PublicKey, salt: &[u8]) -> Id {
    let mut ctx = digest::Context::new(&digest::SHA1_FOR_LEGACY_USE_ONLY);
    ctx.update(key);
    ctx.update(salt);
    ctx.finish().as_ref().try_into().expect("SHA-1 is 20 bytes")
}

/// What the signature covers: the bencoded `salt` (when there is one), `seq` and `v` entries,
/// without the dictionary around them.
pub fn signed_bytes(salt: &[u8], seq: i64, value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    if !salt.is_empty() {
        out.extend_from_slice(b"4:salt");
        bencode::encode_bytes(salt, &mut out);
    }
    out.extend_from_slice(b"3:seqi");
    out.extend_from_slice(seq.to_string().as_bytes());
    out.extend_from_slice(b"e1:v");
    value.encode_into(&mut out);
    out
}

impl MutableItem {
    /// Signs `value` under `salt` with sequence number `seq`. None if it is too large to store.
    pub fn sign(keypair: &Ed25519KeyPair, salt: &[u8], seq: i64, value: Vec<u8>) -> Option<Self> {
        let encoded = Value::Bytes(value);
        if salt.len() > MAX_SALT_LEN || encoded.encode().len() > MAX_VALUE_LEN {
            return None;
        }
        let signature = keypair.sign(&signed_bytes(salt, seq, &encoded)).as_ref().try_into().expect("Ed25519 signatures are 64 bytes");
        let Value::Bytes(value) = encoded else { unreachable!() };
        Some(Self { key: keypair.public_key().as_ref().try_into().expect("Ed25519 keys are 32 bytes"), salt: salt.to_vec(), seq, value, signature })
    }

    pub fn target(&self) -> Id {
        target(&self.key, &self.salt)
    }

    /// Whether the signature checks out.
    pub fn verify(&self) -> bool {
        let signed = signed_bytes(&self.salt, self.seq, &Value::Bytes(self.value.clone()));
        UnparsedPublicKey::new(&ED25519, &self.key).verify(&signed, &self.signature).is_ok()
    }

    /// The item in a `get` response for `key` and `salt` (which the response doesn't repeat),
    /// if it has one, with a byte-string value and a valid signature. Anything else (no item,
    /// another key's, a forged one) is None.
    pub fn from_response(r: &Value, key: &PublicKey, salt: &[u8]) -> Option<Self> {
        if r.bytes_at(b"k")? != key {
            return None;
        }
        let item = Self {
            key: *key,
            salt: salt.to_vec(),
            seq: r.int_at(b"seq")?,
            value: r.bytes_at(b"v")?.to_vec(),
            signature: r.bytes_at(b"sig")?.try_into().ok()?,
        };
        item.verify().then_some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// BEP 44's test vectors: "Hello World!" at seq 1, without and with salt "foobar".
    #[test]
    fn matches_the_bep_44_test_vectors() {
        let key: PublicKey = hex("77ff84905a91936367c01360803104f92432fcd904a43511876df5cdf3e7e548").try_into().unwrap();
        let value = Value::bytes(*b"Hello World!");
        assert_eq!(signed_bytes(b"", 1, &value), b"3:seqi1e1:v12:Hello World!");
        assert_eq!(signed_bytes(b"foobar", 1, &value), b"4:salt6:foobar3:seqi1e1:v12:Hello World!");

        let plain = MutableItem {
            key,
            salt: Vec::new(),
            seq: 1,
            value: b"Hello World!".to_vec(),
            signature: hex(
                "305ac8aeb6c9c151fa120f120ea2cfb923564e11552d06a5d856091e5e853cff1260d3f39e4999684aa92eb73ffd136e6f4f3ecbfda0ce53a1608ecd7ae21f01",
            )
            .try_into()
            .unwrap(),
        };
        assert!(plain.verify());
        assert_eq!(plain.target().to_vec(), hex("4a533d47ec9c7d95b1ad75f576cffc641853b750"));

        let salted = MutableItem {
            salt: b"foobar".to_vec(),
            signature: hex(
                "6834284b6b24c3204eb2fea824d82f88883a3d95e8b4a21b8c0ded553d17d17ddf9a8a7104b1258f30bed3787e6cb896fca78c58f8e03b5f18f14951a87d9a08",
            )
            .try_into()
            .unwrap(),
            ..plain.clone()
        };
        assert!(salted.verify());
        assert_eq!(salted.target().to_vec(), hex("411eba73b6f087ca51a3795d9c8c938d365e32c1"));
    }

    #[test]
    fn signs_and_verifies() {
        let keypair = Ed25519KeyPair::from_seed_unchecked(&[9; 32]).unwrap();
        let item = MutableItem::sign(&keypair, b"salt", 5, b"value".to_vec()).unwrap();
        assert!(item.verify());
        let tampered = MutableItem { seq: 6, ..item.clone() };
        assert!(!tampered.verify(), "the signature covers the sequence number");
        let moved = MutableItem { salt: b"other".to_vec(), ..item };
        assert!(!moved.verify(), "and the salt");
    }

    #[test]
    fn refuses_values_too_large_to_store() {
        let keypair = Ed25519KeyPair::from_seed_unchecked(&[9; 32]).unwrap();
        assert!(MutableItem::sign(&keypair, b"", 1, vec![0; 996]).is_some(), "\"996:\" and the bytes: 1000");
        assert!(MutableItem::sign(&keypair, b"", 1, vec![0; 997]).is_none());
        assert!(MutableItem::sign(&keypair, &[0; 65], 1, Vec::new()).is_none());
    }

    #[test]
    fn reads_items_from_get_responses() {
        let keypair = Ed25519KeyPair::from_seed_unchecked(&[4; 32]).unwrap();
        let item = MutableItem::sign(&keypair, b"s", 7, b"hi".to_vec()).unwrap();
        let r = bencode::dict([
            (b"id", Value::bytes([0u8; 20])),
            (b"k", Value::bytes(item.key)),
            (b"seq", Value::Int(7)),
            (b"sig", Value::bytes(item.signature)),
            (b"v", Value::bytes(*b"hi")),
        ]);
        assert_eq!(MutableItem::from_response(&r, &item.key, b"s"), Some(item.clone()));
        assert_eq!(MutableItem::from_response(&r, &item.key, b"t"), None, "another salt's");
        assert_eq!(MutableItem::from_response(&r, &[1; 32], b"s"), None, "another key's");
    }
}
