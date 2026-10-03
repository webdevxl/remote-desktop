//! One QUIC endpoint per process: it accepts sessions (host role) and opens them (client role)
//! from the same UDP socket and the same device certificate.
//!
//! TLS verifies that each side holds the private key for the certificate it presents, but any
//! certificate is accepted at the TLS layer. Deciding whether a peer is trusted happens after the
//! handshake, by its certificate fingerprint (see [`peer_fingerprint`]).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{AckFrequencyConfig, Connection, Endpoint, EndpointConfig, TransportConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::cc::LanControllerFactory;
use crate::identity::{DeviceIdentity, Fingerprint, fingerprint};

/// Kernel socket buffers. Keyframes arrive as bursts of hundreds of datagrams; the macOS default
/// (~768 KiB) can overflow before the receive task drains it.
const SOCKET_BUFFER: usize = 7 * 1024 * 1024;
/// When nothing has arrived for this long, send a tiny PING (the peer answers it), so each side's
/// Wi-Fi radio hears or sends something every few tens of milliseconds even while the screen is
/// still. A radio left idle may doze, and the next click or frame waits for it to wake. About
/// 50 packets a second each way, a few kbit/s.
const KEEP_ALIVE: Duration = Duration::from_millis(20);

pub fn make_endpoint(bind: SocketAddr, identity: &DeviceIdentity) -> Result<Endpoint> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let mut server_crypto = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(AnyCertVerifier(provider.clone())))
        .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
    server_crypto.alpn_protocols = vec![protocol::ALPN.to_vec()];
    let mut server_config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(server_crypto)?));
    server_config.transport_config(transport_config());

    let mut client_crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertVerifier(provider)))
        .with_client_auth_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
    client_crypto.alpn_protocols = vec![protocol::ALPN.to_vec()];
    let mut client_config =
        quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_crypto)?));
    client_config.transport_config(client_transport_config());

    let socket = bind_socket(bind).with_context(|| format!("bind UDP {bind}"))?;
    let runtime = quinn::default_runtime().context("no async runtime")?;
    let mut endpoint =
        Endpoint::new(EndpointConfig::default(), Some(server_config), socket, runtime)?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

fn bind_socket(bind: SocketAddr) -> Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(bind), Type::DGRAM, Some(Protocol::UDP))?;
    // Best effort: the kernel clamps to kern.ipc.maxsockbuf.
    let _ = socket.set_recv_buffer_size(SOCKET_BUFFER);
    let _ = socket.set_send_buffer_size(SOCKET_BUFFER);
    socket.bind(&bind.into())?;
    Ok(socket.into())
}

/// Hosts accept exactly one control stream and one input stream from each viewer (two input
/// streams while a replaced one winds down) and never read datagrams. Tight limits keep a peer,
/// paired or not, from making the host buffer data it never reads.
fn transport_config() -> Arc<TransportConfig> {
    let mut t = base_transport_config();
    t.max_concurrent_bidi_streams(1u32.into());
    t.max_concurrent_uni_streams(2u32.into());
    t.receive_window((8u32 * 1024 * 1024).into());
    // Not None: quinn then refuses to send datagrams too, and video travels in them.
    t.datagram_receive_buffer_size(Some(64 * 1024));
    Arc::new(t)
}

/// The viewer side sends input, so it asks the host to acknowledge every packet within a
/// millisecond. A lost input packet is then noticed and resent in a few ms instead of waiting
/// out QUIC's default 25 ms ACK delay (measured: ~4 ms instead of ~35 ms on a LAN).
fn client_transport_config() -> Arc<TransportConfig> {
    let mut t = base_transport_config();
    let mut ack = AckFrequencyConfig::default();
    ack.ack_eliciting_threshold(0u32.into())
        .max_ack_delay(Some(Duration::from_millis(1)))
        .reordering_threshold(1u32.into());
    t.ack_frequency_config(Some(ack));
    // Hosts never open streams.
    t.max_concurrent_bidi_streams(0u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    Arc::new(t)
}

fn base_transport_config() -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_idle_timeout(Some(Duration::from_secs(8).try_into().expect("valid idle timeout")));
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.datagram_receive_buffer_size(Some(32 * 1024 * 1024));
    t.datagram_send_buffer_size(16 * 1024 * 1024);
    t.congestion_controller_factory(Arc::new(LanControllerFactory));
    // Ethernet and Wi-Fi both carry 1500-byte frames; MTU discovery can still raise this.
    t.initial_mtu(1400);
    t
}

/// Fingerprint of the certificate the peer presented during the handshake.
pub fn peer_fingerprint(conn: &Connection) -> Option<Fingerprint> {
    let identity = conn.peer_identity()?;
    let certs = identity.downcast_ref::<Vec<CertificateDer<'static>>>()?;
    certs.first().map(|c| fingerprint(c))
}

/// Accepts any certificate but still checks the handshake signature, which proves the peer
/// holds the matching private key. Trust is decided by fingerprint after the handshake.
#[derive(Debug)]
struct AnyCertVerifier(Arc<CryptoProvider>);

impl AnyCertVerifier {
    fn tls12(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }

    fn tls13(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }

    fn schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

impl ServerCertVerifier for AnyCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(m, c, d)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for AnyCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(m, c, d)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.schemes()
    }
}
