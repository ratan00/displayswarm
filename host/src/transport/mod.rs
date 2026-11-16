pub mod aoa;
pub mod audio_udp;
pub mod bitrate;
pub mod firewall;
pub mod mdns;
pub mod pairing;
pub mod tls;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

pub use aoa::{
    connect_aoa_device, AoaConfig, AoaDeviceIdentity, AoaReader, AoaResetHandle, AoaStream, AoaWriter,
};

type TlsServerStream = tokio_rustls::server::TlsStream<TcpStream>;

/// Unified reader half for TCP, TLS-over-TCP or Native USB AOA transport
pub enum TransportReader {
    Tcp(OwnedReadHalf),
    Tls(tokio::io::ReadHalf<TlsServerStream>),
    Aoa(AoaReader),
}

impl AsyncRead for TransportReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TransportReader::Tcp(tcp) => Pin::new(tcp).poll_read(cx, buf),
            TransportReader::Tls(tls) => Pin::new(tls).poll_read(cx, buf),
            TransportReader::Aoa(aoa) => Pin::new(aoa).poll_read(cx, buf),
        }
    }
}

/// Unified writer half for either TCP or Native USB AOA transport
pub enum TransportWriter {
    Tcp(OwnedWriteHalf),
    Tls(tokio::io::WriteHalf<TlsServerStream>),
    Aoa(AoaWriter),
}

impl AsyncWrite for TransportWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            TransportWriter::Tcp(tcp) => Pin::new(tcp).poll_write(cx, buf),
            TransportWriter::Tls(tls) => Pin::new(tls).poll_write(cx, buf),
            TransportWriter::Aoa(aoa) => Pin::new(aoa).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TransportWriter::Tcp(tcp) => Pin::new(tcp).poll_flush(cx),
            TransportWriter::Tls(tls) => Pin::new(tls).poll_flush(cx),
            TransportWriter::Aoa(aoa) => Pin::new(aoa).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TransportWriter::Tcp(tcp) => Pin::new(tcp).poll_shutdown(cx),
            TransportWriter::Tls(tls) => Pin::new(tls).poll_shutdown(cx),
            TransportWriter::Aoa(aoa) => Pin::new(aoa).poll_shutdown(cx),
        }
    }
}

/// Unified bidirectional stream representation
pub enum TransportStream {
    Tcp(TcpStream),
    Aoa(AoaStream),
}

impl TransportStream {
    pub fn into_split(self) -> (TransportReader, TransportWriter) {
        match self {
            TransportStream::Tcp(tcp) => {
                let (r, w) = tcp.into_split();
                (TransportReader::Tcp(r), TransportWriter::Tcp(w))
            }
            TransportStream::Aoa(aoa) => {
                let (r, w) = aoa.into_split();
                (TransportReader::Aoa(r), TransportWriter::Aoa(w))
            }
        }
    }
}

/// How the network (TCP) transport is secured and advertised.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    /// Development only: plain, unauthenticated TCP (no TLS, no pairing).
    /// Set by `DISPLAYSWARM_INSECURE_TCP=1`. Off by default.
    pub insecure_tcp: bool,
    /// Advertise `_displayswarm._tcp` over mDNS.
    pub advertise_mdns: bool,
    /// Adapt the video bitrate to the link on network transports.
    pub adaptive_bitrate: bool,
    /// Where the certificate lives; `None` = `$XDG_CONFIG_HOME/displayswarm`.
    pub config_dir: Option<PathBuf>,
}

pub const INSECURE_TCP_ENV: &str = "DISPLAYSWARM_INSECURE_TCP";

impl Default for NetworkConfig {
    fn default() -> Self {
        let insecure = std::env::var(INSECURE_TCP_ENV)
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        Self { insecure_tcp: insecure, advertise_mdns: true, adaptive_bitrate: true, config_dir: None }
    }
}

/// Time a phone gets for the TLS handshake.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Connections still in the TLS handshake or pairing, at most, in total.
pub const MAX_UNAUTHENTICATED: u32 = 8;
/// ... and from one address.
pub const MAX_UNAUTHENTICATED_PER_ADDRESS: u32 = 2;

/// Counts connections that have not authenticated yet, so a device on the
/// network cannot hold every slot (each may otherwise wait out the pairing
/// timeout) while the user's phone is turned away.
#[derive(Default)]
pub struct PendingLimit {
    by_address: std::sync::Mutex<std::collections::HashMap<Option<std::net::IpAddr>, u32>>,
}

/// One claimed slot; given back on drop.
pub struct PendingSlot<'a> {
    limit: &'a PendingLimit,
    address: Option<std::net::IpAddr>,
}

impl PendingLimit {
    /// A slot for `address`, or `None` when its share or the total is used up.
    pub fn claim(&self, address: Option<std::net::IpAddr>) -> Option<PendingSlot<'_>> {
        let mut map = self.by_address.lock().unwrap_or_else(|e| e.into_inner());
        let total: u32 = map.values().sum();
        let mine = map.get(&address).copied().unwrap_or(0);
        if total >= MAX_UNAUTHENTICATED || mine >= MAX_UNAUTHENTICATED_PER_ADDRESS {
            return None;
        }
        map.insert(address, mine + 1);
        Some(PendingSlot { limit: self, address })
    }
}

impl Drop for PendingSlot<'_> {
    fn drop(&mut self) {
        let mut map = self.limit.by_address.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = map.get_mut(&self.address) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.address);
            }
        }
    }
}

/// Turns accepted TCP connections into authenticated, encrypted streams:
/// TLS handshake, then the pairing / trusted-token exchange.
pub struct NetGate {
    pub identity: tls::TlsIdentity,
    pub pairing: Arc<pairing::PairingManager>,
    acceptor: tokio_rustls::TlsAcceptor,
    pending: PendingLimit,
}

impl NetGate {
    pub fn new(identity: tls::TlsIdentity, pairing: Arc<pairing::PairingManager>) -> io::Result<Self> {
        let acceptor = identity.acceptor()?;
        Ok(Self { identity, pairing, acceptor, pending: PendingLimit::default() })
    }

    /// Runs on the connection's own task. `Err` means the phone was refused
    /// (it has already been told, when it got far enough to be told).
    /// Returns the halves positioned at the phone's `Hello`, and the paired device id.
    pub async fn accept(&self, tcp: TcpStream) -> io::Result<(TransportReader, TransportWriter, String)> {
        let peer = tcp.peer_addr().ok().map(|a| a.ip());
        let _slot = self.pending.claim(peer).ok_or_else(|| {
            io::Error::new(io::ErrorKind::ConnectionRefused, format!("too many unauthenticated connections ({peer:?})"))
        })?;
        let mut tls = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, self.acceptor.accept(tcp))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS handshake timed out"))??;
        let fp = self.identity.fingerprint();
        let device_id = self.pairing.negotiate(&mut tls, &fp, peer).await?;
        let (r, w) = tokio::io::split(tls);
        Ok((TransportReader::Tls(r), TransportWriter::Tls(w), device_id))
    }
}

/// Whether this PC can host Wi-Fi Direct (P2P) connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiDirect {
    Supported,
    /// Not usable; the string says why, for the UI.
    Unsupported(String),
}

/// Reads `nmcli -t -f DEVICE,TYPE device` output: a `wifi-p2p` device means
/// NetworkManager can do P2P.
pub fn parse_nmcli_devices(out: &str) -> WifiDirect {
    if out.lines().any(|l| l.rsplit(':').next().map(str::trim) == Some("wifi-p2p")) {
        WifiDirect::Supported
    } else {
        WifiDirect::Unsupported("no Wi-Fi Direct capable adapter (NetworkManager lists no wifi-p2p device)".into())
    }
}

/// Feature detection only; a P2P connection flow is not implemented.
pub fn wifi_direct_status() -> WifiDirect {
    match std::process::Command::new("nmcli").args(["-t", "-f", "DEVICE,TYPE", "device"]).output() {
        Ok(o) if o.status.success() => parse_nmcli_devices(&String::from_utf8_lossy(&o.stdout)),
        Ok(_) => WifiDirect::Unsupported("NetworkManager did not answer".into()),
        Err(_) => WifiDirect::Unsupported("NetworkManager (nmcli) is not installed".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauthenticated_slots_are_limited_per_address_and_in_total() {
        let limit = PendingLimit::default();
        let a = Some(std::net::IpAddr::from([10, 0, 0, 1]));
        let first = limit.claim(a).unwrap();
        let _second = limit.claim(a).unwrap();
        assert!(limit.claim(a).is_none(), "one address gets two");
        drop(first);
        let _again = limit.claim(a).expect("a slot comes back when its connection ends");
        let others: Vec<_> = (2..=7).map(|i| limit.claim(Some(std::net::IpAddr::from([10, 0, 0, i]))).unwrap()).collect();
        assert_eq!(others.len(), 6);
        assert!(limit.claim(Some(std::net::IpAddr::from([10, 0, 0, 99]))).is_none(), "eight in total");
    }

    #[test]
    fn wifi_direct_needs_a_p2p_device() {
        assert!(matches!(parse_nmcli_devices("wlan0:wifi\nlo:loopback\n"), WifiDirect::Unsupported(_)));
        assert_eq!(parse_nmcli_devices("wlan0:wifi\np2p-dev-wlan0:wifi-p2p\n"), WifiDirect::Supported);
    }
}
