//! Per-device identity: a self-signed certificate generated on first launch and reused after.
//! Its SHA-256 fingerprint is what peers pin once paired.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub type Fingerprint = [u8; 32];

pub struct DeviceIdentity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    pub fingerprint: Fingerprint,
}

impl DeviceIdentity {
    /// Loads `identity-cert.der` / `identity-key.der` from `dir`, creating them if missing.
    pub fn load_or_create(dir: &Path) -> Result<Self> {
        let cert_path = dir.join("identity-cert.der");
        let key_path = dir.join("identity-key.der");
        if let (Ok(cert), Ok(key)) = (fs::read(&cert_path), fs::read(&key_path)) {
            return Ok(Self::from_der(cert, key));
        }

        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let generated = rcgen::generate_simple_self_signed(vec!["lankvm".to_string()])?;
        let cert = generated.cert.der().to_vec();
        let key = generated.signing_key.serialize_der();
        fs::write(&cert_path, &cert)?;
        write_private(&key_path, &key)?;
        tracing::info!("created device identity in {}", dir.display());
        Ok(Self::from_der(cert, key))
    }

    fn from_der(cert: Vec<u8>, key: Vec<u8>) -> Self {
        let fingerprint = fingerprint(&cert);
        Self {
            cert: CertificateDer::from(cert),
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
            fingerprint,
        }
    }
}

pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    let digest = ring::digest::digest(&ring::digest::SHA256, cert_der);
    digest.as_ref().try_into().expect("SHA-256 is 32 bytes")
}

/// Short human-readable form, e.g. `3fa1:9c0e:77b2:d401`.
pub fn short_hex(fp: &Fingerprint) -> String {
    let mut s = String::new();
    for (i, pair) in fp[..8].chunks(2).enumerate() {
        if i > 0 {
            s.push(':');
        }
        let _ = write!(s, "{:02x}{:02x}", pair[0], pair[1]);
    }
    s
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true).mode(0o600);
    std::io::Write::write_all(&mut opts.open(path)?, bytes)?;
    Ok(())
}
