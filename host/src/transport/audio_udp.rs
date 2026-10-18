//! The host's audio over UDP, for network sessions.
//!
//! Over the TLS/TCP session every audio frame queued behind whatever was in
//! flight, and one lost Wi-Fi packet held everything back until its
//! retransmission: on a weak link (a phone's hotspot) sound stuttered and fell
//! behind. Audio on UDP is never held: a late or lost frame is concealed on
//! the phone, and each datagram also carries the previous frame, so a single
//! loss costs nothing at all. Video, input and control stay on TCP.
//!
//! Set-up runs over the existing TLS session: the host binds a socket and
//! sends [`Message::AudioUdpOffer`] (port and a fresh 256-bit key). The phone
//! binds its own socket and names its port ([`Message::AudioUdpAnswer`]); the
//! host sends to it first ([`AudioUdp::knock`]), so a stateful firewall on the
//! host (ufw, firewalld) treats the phone's traffic as a reply and lets it in
//! without any rule. The phone then sends authenticated probes, which tell the
//! host where to send and prove it holds the key; probes are only accepted
//! from the TCP peer's address. While probes keep coming (every second) audio goes over
//! UDP; after [`PROBE_TIMEOUT`] without one it falls back to TCP by itself.
//!
//! Datagram: `"DA"`, version, an 8-byte counter, then AES-256-GCM ciphertext
//! and tag. The header is the associated data; the nonce is a per-direction
//! prefix and the counter, so the two sides never share a nonce under the key.
//! Plaintext: a kind byte, then for audio a count and that many frames
//! (seq u32, pts u64, channels u8, length u16, data).

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};

use crate::protocol::wire::{AudioPacket, Message};

pub const MAGIC: [u8; 2] = *b"DA";
pub const VERSION: u8 = 1;
const HEADER: usize = 2 + 1 + 8;
pub const KIND_AUDIO: u8 = 1;
pub const KIND_PROBE: u8 = 2;
/// Host -> phone, before any audio: opens the host firewall's return path.
pub const KIND_KNOCK: u8 = 3;
/// Nonce prefixes per direction.
pub const DIR_HOST: [u8; 4] = *b"HOST";
pub const DIR_PHONE: [u8; 4] = *b"PHON";
/// No probe for this long: the path is considered gone and audio uses TCP.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Largest datagram accepted.
const MAX_DATAGRAM: usize = 4096;

/// The session key.
pub struct Crypto {
    key: LessSafeKey,
}

impl Crypto {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { key: LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).expect("32-byte key")) }
    }

    fn nonce(dir: [u8; 4], counter: u64) -> Nonce {
        let mut n = [0u8; 12];
        n[..4].copy_from_slice(&dir);
        n[4..].copy_from_slice(&counter.to_be_bytes());
        Nonce::assume_unique_for_key(n)
    }

    /// A datagram carrying `plaintext`, sent in direction `dir` with `counter`.
    pub fn seal(&self, dir: [u8; 4], counter: u64, plaintext: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + plaintext.len() + 16);
        out.extend_from_slice(&MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&counter.to_be_bytes());
        let mut body = plaintext.to_vec();
        let header: [u8; HEADER] = out[..HEADER].try_into().unwrap();
        self.key
            .seal_in_place_append_tag(Self::nonce(dir, counter), Aad::from(header), &mut body)
            .expect("seal");
        out.extend_from_slice(&body);
        out
    }

    /// The counter and plaintext of a datagram sent in direction `dir`, or
    /// `None` if it is not ours or was tampered with.
    pub fn open(&self, dir: [u8; 4], datagram: &[u8]) -> Option<(u64, Vec<u8>)> {
        if datagram.len() < HEADER + 16 || datagram[..2] != MAGIC || datagram[2] != VERSION {
            return None;
        }
        let counter = u64::from_be_bytes(datagram[3..HEADER].try_into().ok()?);
        let header: [u8; HEADER] = datagram[..HEADER].try_into().ok()?;
        let mut body = datagram[HEADER..].to_vec();
        let plain = self.key.open_in_place(Self::nonce(dir, counter), Aad::from(header), &mut body).ok()?;
        Some((counter, plain.to_vec()))
    }
}

/// Plaintext of an audio datagram holding `frames` (oldest first).
pub fn encode_audio(frames: &[&AudioPacket]) -> Vec<u8> {
    let mut w = vec![KIND_AUDIO, frames.len() as u8];
    for p in frames {
        w.extend_from_slice(&p.seq.to_be_bytes());
        w.extend_from_slice(&p.pts_us.to_be_bytes());
        w.push(p.channels);
        w.extend_from_slice(&(p.data.len() as u16).to_be_bytes());
        w.extend_from_slice(&p.data);
    }
    w
}

/// The frames of an audio plaintext, or `None` if it is malformed.
pub fn decode_audio(plain: &[u8]) -> Option<Vec<AudioPacket>> {
    if plain.len() < 2 || plain[0] != KIND_AUDIO {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 2;
    for _ in 0..plain[1] {
        let h = plain.get(i..i + 15)?;
        let len = u16::from_be_bytes([h[13], h[14]]) as usize;
        let data = plain.get(i + 15..i + 15 + len)?.to_vec();
        out.push(AudioPacket {
            seq: u32::from_be_bytes(h[0..4].try_into().ok()?),
            pts_us: u64::from_be_bytes(h[4..12].try_into().ok()?),
            channels: h[12],
            data,
        });
        i += 15 + len;
    }
    Some(out)
}

struct Path {
    peer: Option<SocketAddr>,
    last_probe: Option<Instant>,
    last_probe_counter: Option<u64>,
}

struct Inner {
    socket: UdpSocket,
    crypto: Crypto,
    allowed_ip: IpAddr,
    path: Mutex<Path>,
    counter: AtomicU64,
    /// The last frame sent, repeated in the next datagram.
    prev: Mutex<Option<AudioPacket>>,
}

/// One session's UDP audio path. Dropping it closes the socket.
pub struct AudioUdp {
    inner: Arc<Inner>,
    key: [u8; 32],
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioUdp {
    /// Binds a socket on `local_ip` for the phone at `peer_ip` (the TCP
    /// session's two ends) and starts listening for its probes.
    pub fn bind(local_ip: IpAddr, peer_ip: IpAddr) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0))?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        // Ask for low-latency treatment on the way out (DSCP EF); best effort.
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let tos: libc::c_int = 0xB8;
            // Safety: a valid socket fd and an int option.
            unsafe {
                libc::setsockopt(
                    socket.as_raw_fd(),
                    libc::IPPROTO_IP,
                    libc::IP_TOS,
                    &tos as *const libc::c_int as *const libc::c_void,
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
        let mut key = [0u8; 32];
        SystemRandom::new().fill(&mut key).map_err(|_| std::io::Error::other("no randomness"))?;
        let inner = Arc::new(Inner {
            socket,
            crypto: Crypto::new(&key),
            allowed_ip: peer_ip,
            path: Mutex::new(Path { peer: None, last_probe: None, last_probe_counter: None }),
            counter: AtomicU64::new(0),
            prev: Mutex::new(None),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (inner, stop) = (inner.clone(), stop.clone());
            std::thread::Builder::new().name("audio-udp".into()).spawn(move || receive_loop(&inner, &stop))?
        };
        Ok(Self { inner, key, stop, thread: Some(thread) })
    }

    /// The offer the phone needs (send it over the TLS session).
    pub fn offer(&self) -> Message {
        let port = self.inner.socket.local_addr().map(|a| a.port()).unwrap_or(0);
        Message::AudioUdpOffer { port, key: self.key }
    }

    /// The phone's socket is at `port` on the TCP peer's address: send to it
    /// first so the host firewall admits its probes as replies.
    pub fn knock(&self, port: u16) {
        let to = SocketAddr::new(self.inner.allowed_ip, port);
        for _ in 0..3 {
            let counter = self.inner.counter.fetch_add(1, Ordering::Relaxed);
            let _ = self.inner.socket.send_to(&self.inner.crypto.seal(DIR_HOST, counter, &[KIND_KNOCK]), to);
        }
    }

    /// Whether the phone's probes are arriving, i.e. audio should go here.
    pub fn is_live(&self) -> bool {
        let path = self.inner.path.lock().unwrap_or_else(|e| e.into_inner());
        path.peer.is_some() && path.last_probe.is_some_and(|t| t.elapsed() < PROBE_TIMEOUT)
    }

    /// Sends `p` (with the previous frame for redundancy) if the path is live.
    /// False means the caller should send it over TCP instead.
    pub fn try_send(&self, p: &AudioPacket) -> bool {
        let peer = {
            let path = self.inner.path.lock().unwrap_or_else(|e| e.into_inner());
            match (path.peer, path.last_probe) {
                (Some(peer), Some(t)) if t.elapsed() < PROBE_TIMEOUT => peer,
                _ => return false,
            }
        };
        let plain = {
            let mut prev = self.inner.prev.lock().unwrap_or_else(|e| e.into_inner());
            let plain = match prev.as_ref() {
                Some(old) if old.seq.wrapping_add(1) == p.seq => encode_audio(&[old, p]),
                _ => encode_audio(&[p]),
            };
            *prev = Some(p.clone());
            plain
        };
        let counter = self.inner.counter.fetch_add(1, Ordering::Relaxed);
        let datagram = self.inner.crypto.seal(DIR_HOST, counter, &plain);
        // A full socket buffer (or a vanished route) drops this frame; the
        // phone conceals it. Never block the audio thread.
        let _ = self.inner.socket.send_to(&datagram, peer);
        true
    }
}

impl Drop for AudioUdp {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn receive_loop(inner: &Inner, stop: &AtomicBool) {
    let mut buf = [0u8; MAX_DATAGRAM];
    let mut announced = false;
    while !stop.load(Ordering::Relaxed) {
        let Ok((n, from)) = inner.socket.recv_from(&mut buf) else { continue };
        if from.ip() != inner.allowed_ip {
            continue;
        }
        let Some((counter, plain)) = inner.crypto.open(DIR_PHONE, &buf[..n]) else { continue };
        if plain.first() != Some(&KIND_PROBE) {
            continue;
        }
        let mut path = inner.path.lock().unwrap_or_else(|e| e.into_inner());
        // Replays (and reordered old probes) are ignored.
        if path.last_probe_counter.is_some_and(|c| counter <= c) {
            continue;
        }
        path.last_probe_counter = Some(counter);
        path.last_probe = Some(Instant::now());
        if path.peer != Some(from) {
            path.peer = Some(from);
            announced = false;
        }
        if !announced {
            announced = true;
            log::info!("audio: UDP path to {from} is up");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(seq: u32) -> AudioPacket {
        AudioPacket { seq, pts_us: seq as u64 * 10_000, channels: 2, data: vec![seq as u8; 40] }
    }

    #[test]
    fn seal_open_round_trip_and_direction_separation() {
        let c = Crypto::new(&[7; 32]);
        let d = c.seal(DIR_HOST, 5, b"hello");
        assert_eq!(c.open(DIR_HOST, &d), Some((5, b"hello".to_vec())));
        // The other direction's nonce space does not open it.
        assert_eq!(c.open(DIR_PHONE, &d), None);
        // Any flipped byte (header or body) is rejected.
        for i in 0..d.len() {
            let mut t = d.clone();
            t[i] ^= 1;
            assert_eq!(c.open(DIR_HOST, &t), None, "byte {i}");
        }
        assert_eq!(Crypto::new(&[8; 32]).open(DIR_HOST, &d), None, "wrong key");
    }

    /// Pinned bytes, also checked by the phone (AudioUdpClientTest.kt), so the
    /// two implementations cannot drift apart.
    #[test]
    fn sealed_datagrams_match_the_shared_vectors() {
        let c = Crypto::new(&[7; 32]);
        let p = AudioPacket { seq: 9, pts_us: 90_000, channels: 2, data: vec![1, 2, 3] };
        let hex = |d: Vec<u8>| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(
            hex(c.seal(DIR_HOST, 5, &encode_audio(&[&p]))),
            "444101000000000000000575c4a9287d81833ea2a47b02111b2b40ceaa5bbfdf04f0c453cadfd5aecc2158afb7626e"
        );
        assert_eq!(hex(c.seal(DIR_PHONE, 1, &[KIND_PROBE])), "44410100000000000000016239f6858844460fab8ce3fc72788f43e1");
    }

    #[test]
    fn audio_frames_round_trip() {
        let (a, b) = (pkt(9), pkt(10));
        let plain = encode_audio(&[&a, &b]);
        assert_eq!(decode_audio(&plain), Some(vec![a, b]));
        assert_eq!(decode_audio(&plain[..plain.len() - 1]), None, "truncated");
        assert_eq!(decode_audio(&[KIND_PROBE]), None);
    }

    /// The whole path on loopback: no audio before a probe, then UDP with the
    /// previous frame repeated, and nothing for an unauthenticated probe.
    #[test]
    fn probes_open_the_path_and_frames_carry_redundancy() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let udp = AudioUdp::bind(lo, lo).unwrap();
        let Message::AudioUdpOffer { port, key } = udp.offer() else { panic!() };
        assert!(!udp.try_send(&pkt(1)), "no probe yet: TCP");

        let phone = UdpSocket::bind((lo, 0)).unwrap();
        phone.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let crypto = Crypto::new(&key);
        // A probe under the wrong key does nothing.
        phone.send_to(&Crypto::new(&[1; 32]).seal(DIR_PHONE, 0, &[KIND_PROBE]), (lo, port)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(!udp.is_live());

        phone.send_to(&crypto.seal(DIR_PHONE, 1, &[KIND_PROBE]), (lo, port)).unwrap();
        let t = Instant::now();
        while !udp.is_live() {
            assert!(t.elapsed() < Duration::from_secs(2), "path never came up");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(udp.try_send(&pkt(2)));
        assert!(udp.try_send(&pkt(3)));
        let mut buf = [0u8; 4096];
        let n = phone.recv(&mut buf).unwrap();
        let (_, plain) = crypto.open(DIR_HOST, &buf[..n]).unwrap();
        assert_eq!(decode_audio(&plain).unwrap(), vec![pkt(2)]);
        let n = phone.recv(&mut buf).unwrap();
        let (_, plain) = crypto.open(DIR_HOST, &buf[..n]).unwrap();
        assert_eq!(decode_audio(&plain).unwrap(), vec![pkt(2), pkt(3)], "previous frame repeated");
    }
}
