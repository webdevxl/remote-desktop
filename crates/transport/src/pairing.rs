//! First-connection pairing with a 6-digit PIN shown on the host.
//!
//! SPAKE2 turns the short PIN into a strong shared key without revealing it, so a passive
//! eavesdropper learns nothing and an active attacker gets a single guess per PIN. Both
//! certificate fingerprints are bound in as SPAKE2 identities: a man-in-the-middle presents
//! different certificates to each side, so the two sides' keys differ and confirmation fails.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::identity::Fingerprint;

const HOST_LABEL: &[u8] = b"lankvm pairing: host confirms";
const CLIENT_LABEL: &[u8] = b"lankvm pairing: client confirms";

/// Six random decimal digits.
pub fn generate_pin() -> String {
    let mut bytes = [0u8; 4];
    SystemRandom::new().fill(&mut bytes).expect("system RNG");
    format!("{:06}", u32::from_le_bytes(bytes) % 1_000_000)
}

/// Normalizes user input like "123 456" to "123456".
pub fn normalize_pin(input: &str) -> String {
    input.chars().filter(char::is_ascii_digit).collect()
}

fn ids(client: &Fingerprint, host: &Fingerprint) -> (Identity, Identity) {
    (Identity::new(client), Identity::new(host))
}

fn mac(key: &[u8], label: &[u8]) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), label).as_ref().to_vec()
}

/// Client side, step 1: send the returned message to the host.
pub struct ClientPairing(Spake2<Ed25519Group>);

pub fn client_start(pin: &str, client: &Fingerprint, host: &Fingerprint) -> (ClientPairing, Vec<u8>) {
    let (id_a, id_b) = ids(client, host);
    let (state, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(normalize_pin(pin).as_bytes()), &id_a, &id_b);
    (ClientPairing(state), msg)
}

impl ClientPairing {
    /// Step 3: checks the host's confirmation and returns ours. Fails on a wrong PIN.
    pub fn finish(self, host_msg: &[u8], host_mac: &[u8]) -> Result<Vec<u8>> {
        let key = self.0.finish(host_msg).map_err(|e| anyhow!("pairing: {e:?}"))?;
        hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, &key), HOST_LABEL, host_mac)
            .map_err(|_| anyhow!("wrong code"))?;
        Ok(mac(&key, CLIENT_LABEL))
    }
}

/// Host side after step 2: holds the key until the client's confirmation arrives.
pub struct HostPairing(Vec<u8>);

/// Step 2: answers the client's message. Returns our SPAKE2 message and confirmation.
pub fn host_respond(pin: &str, client: &Fingerprint, host: &Fingerprint, client_msg: &[u8]) -> Result<(HostPairing, Vec<u8>, Vec<u8>)> {
    let (id_a, id_b) = ids(client, host);
    let (state, msg) = Spake2::<Ed25519Group>::start_b(&Password::new(pin.as_bytes()), &id_a, &id_b);
    let key = state.finish(client_msg).map_err(|e| anyhow!("pairing: {e:?}"))?;
    let host_mac = mac(&key, HOST_LABEL);
    Ok((HostPairing(key), msg, host_mac))
}

impl HostPairing {
    /// Step 4: true if the client proved it knew the PIN.
    pub fn verify(&self, client_mac: &[u8]) -> bool {
        hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, &self.0), CLIENT_LABEL, client_mac).is_ok()
    }
}

/// Paired devices, stored as `<hex fingerprint> <name>` lines.
pub struct TrustStore {
    path: PathBuf,
    entries: Vec<(Fingerprint, String)>,
}

impl TrustStore {
    pub fn load(path: &Path) -> Self {
        let entries = fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| {
                let (hex, name) = line.split_once(' ').unwrap_or((line, ""));
                Some((from_hex(hex)?, name.to_string()))
            })
            .collect();
        Self { path: path.to_path_buf(), entries }
    }

    pub fn contains(&self, fp: &Fingerprint) -> bool {
        self.entries.iter().any(|(f, _)| f == fp)
    }

    pub fn entries(&self) -> &[(Fingerprint, String)] {
        &self.entries
    }

    pub fn add(&mut self, fp: Fingerprint, name: &str) -> Result<()> {
        self.entries.retain(|(f, _)| *f != fp);
        self.entries.push((fp, name.replace('\n', " ")));
        self.save()
    }

    pub fn remove(&mut self, fp: &Fingerprint) -> Result<()> {
        self.entries.retain(|(f, _)| f != fp);
        self.save()
    }

    fn save(&self) -> Result<()> {
        let mut out = String::new();
        for (fp, name) in &self.entries {
            let _ = writeln!(out, "{} {name}", to_hex(fp));
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&self.path, out)?;
        Ok(())
    }
}

fn to_hex(fp: &Fingerprint) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Fingerprint> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    /// Runs both sides in-process.
    fn pair_locally(client_pin: &str, host_pin: &str, client_fp: &Fingerprint, host_fp_seen_by_client: &Fingerprint, host_fp: &Fingerprint, client_fp_seen_by_host: &Fingerprint) -> Result<()> {
    let (client, msg_a) = client_start(client_pin, client_fp, host_fp_seen_by_client);
    let (host, msg_b, host_mac) = host_respond(host_pin, client_fp_seen_by_host, host_fp, &msg_a)?;
    let client_mac = client.finish(&msg_b, &host_mac)?;
    if !host.verify(&client_mac) {
        bail!("host rejected confirmation");
    }
    Ok(())
    }

    const C: Fingerprint = [1; 32];
    const H: Fingerprint = [2; 32];
    const M: Fingerprint = [9; 32];

    #[test]
    fn same_pin_pairs() {
        pair_locally("123456", "123456", &C, &H, &H, &C).unwrap();
        pair_locally("123 456", "123456", &C, &H, &H, &C).unwrap();
    }

    #[test]
    fn wrong_pin_fails() {
        assert!(pair_locally("123457", "123456", &C, &H, &H, &C).is_err());
    }

    #[test]
    fn man_in_the_middle_fails() {
        // The attacker shows its own certificate M to both sides, so each side binds a
        // different identity pair even with the right PIN.
        assert!(pair_locally("123456", "123456", &C, &M, &H, &M).is_err());
    }

    #[test]
    fn pins_are_six_digits() {
        for _ in 0..50 {
            let pin = generate_pin();
            assert_eq!(pin.len(), 6);
            assert!(pin.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn trust_store_round_trip() {
        let path = std::env::temp_dir().join(format!("lankvm-trust-{}.txt", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut store = TrustStore::load(&path);
        store.add(C, "Alex's MacBook").unwrap();
        store.add(H, "Studio").unwrap();
        store.remove(&H).unwrap();
        let reloaded = TrustStore::load(&path);
        assert!(reloaded.contains(&C) && !reloaded.contains(&H));
        assert_eq!(reloaded.entries()[0].1, "Alex's MacBook");
        let _ = fs::remove_file(&path);
    }
}
