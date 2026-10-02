//! TLS for the network transports: a self-signed host certificate generated
//! once and kept in the user's config directory, and the server/client configs.
//!
//! Trust is by **pinned fingerprint** (SHA-256 of the certificate's DER), not by
//! a CA: the phone learns the fingerprint at pairing (from the QR code, or on
//! first connect together with a PIN proof bound to it) and refuses any other
//! certificate afterwards. See [`super::pairing`] for the pairing exchange.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring as ring_provider, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// SHA-256 of a certificate's DER encoding.
pub type Fingerprint = [u8; 32];

pub fn fingerprint_of(cert_der: &[u8]) -> Fingerprint {
    Sha256::digest(cert_der).into()
}

pub fn fingerprint_hex(fp: &Fingerprint) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses `aa:bb:..` or plain hex (case-insensitive) into a fingerprint.
pub fn parse_fingerprint(text: &str) -> Option<Fingerprint> {
    let digits: String = text.chars().filter(|c| *c != ':' && !c.is_whitespace()).collect();
    if digits.len() != 64 || !digits.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(ring_provider::default_provider())
}

/// The host's certificate and key.
#[derive(Clone)]
pub struct TlsIdentity {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

impl TlsIdentity {
    /// A fresh self-signed certificate (ECDSA P-256, ten years) for `host_name`.
    pub fn generate(host_name: &str) -> io::Result<Self> {
        let other = |e: rcgen::Error| io::Error::new(io::ErrorKind::Other, e.to_string());
        let names = vec![host_name.to_string(), "displayswarm.local".to_string()];
        let mut params = rcgen::CertificateParams::new(names).map_err(other)?;
        params.distinguished_name.push(rcgen::DnType::CommonName, "DisplaySwarm host");
        let now = std::time::SystemTime::now();
        params.not_before = (now - std::time::Duration::from_secs(24 * 3600)).into();
        params.not_after = (now + std::time::Duration::from_secs(3650 * 24 * 3600)).into();
        let key = rcgen::KeyPair::generate().map_err(other)?;
        let cert = params.self_signed(&key).map_err(other)?;
        Ok(Self { cert_der: cert.der().to_vec(), key_der: key.serialize_der() })
    }

    /// Loads `dir/host-cert.der` + `dir/host-key.der`, generating and saving
    /// them (key mode 0600) when missing or unusable.
    pub fn load_or_create(dir: &Path, host_name: &str) -> io::Result<Self> {
        let cert_path = dir.join("host-cert.der");
        let key_path = dir.join("host-key.der");
        if let (Ok(cert_der), Ok(key_der)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            let id = Self { cert_der, key_der };
            if id.server_config().is_ok() {
                return Ok(id);
            }
            log::warn!("The saved TLS identity in {} is unusable; generating a new one", dir.display());
        }
        let id = Self::generate(host_name)?;
        std::fs::create_dir_all(dir)?;
        write_private(&key_path, &id.key_der)?;
        write_private(&cert_path, &id.cert_der)?;
        log::info!("Generated a new TLS identity, fingerprint {}", id.fingerprint_hex());
        Ok(id)
    }

    pub fn cert_der(&self) -> &[u8] {
        &self.cert_der
    }

    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint_of(&self.cert_der)
    }

    pub fn fingerprint_hex(&self) -> String {
        fingerprint_hex(&self.fingerprint())
    }

    fn server_config(&self) -> Result<ServerConfig, rustls::Error> {
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()));
        ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(self.cert_der.clone())], key)
    }

    pub fn acceptor(&self) -> io::Result<TlsAcceptor> {
        let cfg = self.server_config().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(TlsAcceptor::from(Arc::new(cfg)))
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)
}

/// `$XDG_CONFIG_HOME/displayswarm` (fallback `~/.config/displayswarm`).
pub fn default_config_dir() -> Option<PathBuf> {
    crate::devices::DeviceStore::default_path().and_then(|p| p.parent().map(Path::to_path_buf))
}

/// What a client checks the host's certificate against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    /// Accept exactly this certificate.
    Fingerprint(Fingerprint),
    /// First contact: accept any certificate. The caller must bind the pairing
    /// proof to the fingerprint it saw (see `pairing::proof`), which is what
    /// stops a man in the middle from completing a pairing.
    Unpinned,
}

#[derive(Debug)]
struct PinVerifier {
    pin: Pin,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.pin {
            Pin::Unpinned => Ok(ServerCertVerified::assertion()),
            Pin::Fingerprint(fp) if fingerprint_of(end_entity) == fp => Ok(ServerCertVerified::assertion()),
            Pin::Fingerprint(_) => {
                Err(rustls::Error::General("host certificate does not match the pinned fingerprint".into()))
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A TLS connector that enforces [`Pin`]. The host has no hostname a phone could
/// verify, so the fingerprint is the only identity. (The phone app does the
/// same in Kotlin; this one serves the tests and any Rust client.)
pub fn pinned_connector(pin: Pin) -> io::Result<TlsConnector> {
    let provider = provider();
    let cfg = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifier { pin, provider }))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(cfg)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn name() -> ServerName<'static> {
        ServerName::try_from("displayswarm.local").unwrap()
    }

    async fn echo_once(acceptor: TlsAcceptor, listener: TcpListener) {
        let (tcp, _) = listener.accept().await.unwrap();
        if let Ok(mut tls) = acceptor.accept(tcp).await {
            let mut b = [0u8; 4];
            if tls.read_exact(&mut b).await.is_ok() {
                let _ = tls.write_all(&b).await;
                let _ = tls.flush().await;
            }
        }
    }

    #[tokio::test]
    async fn handshake_with_the_right_pin_succeeds_and_carries_data() {
        let id = TlsIdentity::generate("testhost").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(echo_once(id.acceptor().unwrap(), listener));

        let conn = pinned_connector(Pin::Fingerprint(id.fingerprint())).unwrap();
        let mut tls = conn.connect(name(), TcpStream::connect(addr).await.unwrap()).await.unwrap();
        tls.write_all(b"ping").await.unwrap();
        tls.flush().await.unwrap();
        let mut b = [0u8; 4];
        tls.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"ping");
    }

    #[tokio::test]
    async fn a_different_certificate_is_refused_by_the_pin() {
        let real = TlsIdentity::generate("testhost").unwrap();
        let impostor = TlsIdentity::generate("testhost").unwrap();
        assert_ne!(real.fingerprint(), impostor.fingerprint());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(echo_once(impostor.acceptor().unwrap(), listener));

        let conn = pinned_connector(Pin::Fingerprint(real.fingerprint())).unwrap();
        let res = conn.connect(name(), TcpStream::connect(addr).await.unwrap()).await;
        assert!(res.is_err(), "the impostor's certificate must not pass the pin");
    }

    #[test]
    fn identity_is_saved_privately_and_reloaded_unchanged() {
        let dir = std::env::temp_dir().join(format!("displayswarm-tls-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = TlsIdentity::load_or_create(&dir, "h").unwrap();
        let b = TlsIdentity::load_or_create(&dir, "h").unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("host-key.der")).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the private key must not be group/world accessible");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprints_round_trip_through_text() {
        let fp = fingerprint_of(b"x");
        let hex = fingerprint_hex(&fp);
        assert_eq!(parse_fingerprint(&hex), Some(fp));
        let colon: Vec<String> = fp.iter().map(|b| format!("{:02X}", b)).collect();
        assert_eq!(parse_fingerprint(&colon.join(":")), Some(fp));
        assert_eq!(parse_fingerprint("abc"), None);
    }
}
