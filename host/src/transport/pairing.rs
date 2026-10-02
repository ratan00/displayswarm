//! Pairing and trust for network connections.
//!
//! Every connection is TLS (see [`super::tls`]) and starts with a `PairRequest`
//! before the v2 `Hello`:
//!
//! * A **trusted** phone sends `PAIR_MODE_TOKEN` with the per-device token it
//!   was given when it paired. No PIN is needed again.
//! * A new phone sends `PAIR_MODE_PIN` with an empty credential. The host shows
//!   a 6-digit PIN (log line, [`PairEvent::PinRequested`]) and answers
//!   `PAIR_PIN_REQUIRED`; the phone then sends `PAIR_MODE_PIN` with
//!   [`proof`]`(pin, host fingerprint, device id)`. The proof is bound to the
//!   certificate the phone actually saw, so a man in the middle presenting a
//!   different certificate cannot complete the pairing. (A 6-digit PIN can be
//!   brute-forced offline from a captured proof; the QR route below has no such
//!   weakness.)
//! * A phone that scanned the QR code ([`PairingManager::open_qr_offer`]) pins
//!   the fingerprint from the code and sends `PAIR_MODE_QR` with a proof keyed by
//!   the code's 128-bit one-time secret.
//!
//! A successful pairing yields a random 32-byte token, returned once in the
//! `PairResponse`; only its SHA-256 is stored (`trusted.json`, mode 0600).
//! An offer allows [`MAX_ATTEMPTS`] wrong proofs, then it is void and new offers
//! are refused for [`LOCKOUT`].

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use super::tls::{fingerprint_hex, Fingerprint};
use crate::protocol::wire::{
    Message, MessageReader, PAIR_DISABLED, PAIR_LOCKED, PAIR_MODE_PIN, PAIR_MODE_QR, PAIR_MODE_TOKEN, PAIR_OK,
    PAIR_PIN_REQUIRED, PAIR_UNTRUSTED, PAIR_WRONG,
};

/// Wrong proofs an offer tolerates.
pub const MAX_ATTEMPTS: u32 = 3;
/// How long new offers are refused after an offer was exhausted.
pub const LOCKOUT: Duration = Duration::from_secs(30);
/// How long a PIN requested by a phone stays valid.
pub const PIN_TTL: Duration = Duration::from_secs(120);
/// Least time between two phone-triggered PINs.
pub const PIN_REQUEST_GAP: Duration = Duration::from_secs(3);
/// Requests read on one connection before the host hangs up.
const MAX_REQUESTS_PER_CONNECTION: u32 = 12;
/// The whole pre-Hello exchange must finish in this time.
pub const PAIRING_TIMEOUT: Duration = Duration::from_secs(150);

// ---- Crypto helpers ------------------------------------------------------------

type HmacSha256 = Hmac<Sha256>;

/// `HMAC-SHA256(secret, "displayswarm-pair-v1" || fingerprint || device_id)`.
pub fn proof(secret: &[u8], fingerprint: &Fingerprint, device_id: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(b"displayswarm-pair-v1");
    mac.update(fingerprint);
    mac.update(device_id.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS random source is available");
    b
}

/// A uniformly random 6-digit PIN.
pub fn random_pin() -> String {
    loop {
        let n = u32::from_be_bytes(random_bytes::<4>());
        // Reject the tail so the modulo is unbiased.
        if n < 4_294_000_000 {
            return format!("{:06}", n % 1_000_000);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn token_hash(token: &[u8]) -> String {
    hex(&Sha256::digest(token))
}

// ---- Trust store ---------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustedDevice {
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    /// Hex SHA-256 of the device token.
    pub token_hash: String,
    #[serde(default)]
    pub paired_at: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct TrustFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    devices: Vec<TrustedDevice>,
}

/// Phones allowed to connect without a PIN: `$XDG_CONFIG_HOME/displayswarm/trusted.json`.
pub struct TrustStore {
    path: Option<PathBuf>,
    devices: Vec<TrustedDevice>,
}

impl TrustStore {
    pub fn in_memory() -> Self {
        Self { path: None, devices: Vec::new() }
    }

    pub fn default_path() -> Option<PathBuf> {
        super::tls::default_config_dir().map(|d| d.join("trusted.json"))
    }

    pub fn open_default() -> io::Result<Self> {
        match Self::default_path() {
            Some(p) => Self::open(&p),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "no config directory")),
        }
    }

    /// A missing file is an empty store; a corrupt one is moved aside.
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut store = Self { path: Some(path.to_path_buf()), devices: Vec::new() };
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<TrustFile>(&text) {
                Ok(f) => store.devices = f.devices.into_iter().filter(|d| !d.device_id.is_empty()).collect(),
                Err(e) => {
                    log::warn!("The trust store {} is corrupt ({e}); moving it aside", path.display());
                    let _ = std::fs::rename(path, path.with_extension("json.corrupt"));
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("Cannot read the trust store {}: {e}", path.display()),
        }
        Ok(store)
    }

    fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = TrustFile { version: 1, devices: self.devices.clone() };
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp)?;
            f.write_all(serde_json::to_string_pretty(&file).map_err(io::Error::other)?.as_bytes())?;
        }
        std::fs::rename(&tmp, path)
    }

    pub fn list(&self) -> Vec<TrustedDevice> {
        self.devices.clone()
    }

    pub fn is_trusted(&self, device_id: &str, token: &[u8]) -> bool {
        let want = token_hash(token);
        self.devices.iter().any(|d| d.device_id == device_id && ct_eq(d.token_hash.as_bytes(), want.as_bytes()))
    }

    /// Trusts `device_id` with `token`, replacing an earlier pairing of it.
    pub fn add(&mut self, device_id: &str, name: &str, token: &[u8]) -> io::Result<()> {
        self.devices.retain(|d| d.device_id != device_id);
        self.devices.push(TrustedDevice {
            device_id: device_id.into(),
            name: name.into(),
            token_hash: token_hash(token),
            paired_at: crate::devices::unix_now(),
        });
        self.save()
    }

    pub fn revoke(&mut self, device_id: &str) -> io::Result<bool> {
        let before = self.devices.len();
        self.devices.retain(|d| d.device_id != device_id);
        let changed = self.devices.len() != before;
        if changed {
            self.save()?;
        }
        Ok(changed)
    }
}

// ---- Manager -------------------------------------------------------------------

/// What the host UI / tray should react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairEvent {
    /// Show this PIN to the user; a phone is asking to pair (or the user opened an offer).
    PinRequested { device_id: String, device_name: String, pin: String, valid_for: Duration },
    Paired { device_id: String, device_name: String },
    /// A wrong PIN/secret, or a lock-out.
    Failed { device_id: String, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfferKind {
    Pin,
    Qr,
}

struct Offer {
    kind: OfferKind,
    secret: Vec<u8>,
    expires: Instant,
    wrong: u32,
}

#[derive(Default)]
struct State {
    offer: Option<Offer>,
    locked_until: Option<Instant>,
    last_pin_request: Option<Instant>,
}

/// What is currently on offer, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferInfo {
    pub pin: Option<String>,
    pub qr: bool,
    pub remaining: Duration,
}

pub struct QrOffer {
    /// `displayswarm://pair?...`; encode as a QR code with [`qr_svg`] / [`qr_text`].
    pub payload: String,
    pub valid_for: Duration,
}

type Listener = dyn Fn(PairEvent) + Send + Sync;

/// Pairing state shared by all connections. Lives in the session manager
/// (`SessionManager::pairing`).
pub struct PairingManager {
    state: Mutex<State>,
    trust: Mutex<TrustStore>,
    listener: Mutex<Option<Arc<Listener>>>,
}

/// What `handle_request` decided.
#[derive(Debug, PartialEq)]
pub struct Decision {
    pub reply: Message,
    /// The device is now authenticated (paired or trusted).
    pub accepted: bool,
    /// Hang up after sending the reply.
    pub close: bool,
}

fn respond(status: u8, message: &str, token: Vec<u8>, accepted: bool, close: bool) -> Decision {
    Decision { reply: Message::PairResponse { status, message: message.into(), token }, accepted, close }
}

impl PairingManager {
    pub fn new(trust: TrustStore) -> Arc<Self> {
        Arc::new(Self { state: Mutex::default(), trust: Mutex::new(trust), listener: Mutex::new(None) })
    }

    pub fn in_memory() -> Arc<Self> {
        Self::new(TrustStore::in_memory())
    }

    /// The trust store in the user's config directory; in memory if unusable.
    pub fn with_defaults() -> Arc<Self> {
        Self::new(TrustStore::open_default().unwrap_or_else(|e| {
            log::warn!("Trust store unavailable ({e}); pairings will last only for this run");
            TrustStore::in_memory()
        }))
    }

    /// Registers the UI callback (any thread; keep it cheap).
    pub fn set_event_listener(&self, f: impl Fn(PairEvent) + Send + Sync + 'static) {
        *self.listener.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
    }

    fn emit(&self, ev: PairEvent) {
        let l = self.listener.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(l) = l {
            l(ev);
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn trust(&self) -> std::sync::MutexGuard<'_, TrustStore> {
        self.trust.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn trusted_devices(&self) -> Vec<TrustedDevice> {
        self.trust().list()
    }

    pub fn revoke(&self, device_id: &str) -> bool {
        self.trust().revoke(device_id).unwrap_or_else(|e| {
            log::warn!("Cannot save the trust store: {e}");
            false
        })
    }

    /// The live offer, if any (expired ones are dropped).
    pub fn pending_offer(&self) -> Option<OfferInfo> {
        let mut st = self.state();
        Self::expire(&mut st);
        st.offer.as_ref().map(|o| OfferInfo {
            pin: (o.kind == OfferKind::Pin).then(|| String::from_utf8_lossy(&o.secret).into_owned()),
            qr: o.kind == OfferKind::Qr,
            remaining: o.expires.saturating_duration_since(Instant::now()),
        })
    }

    pub fn close_offer(&self) {
        self.state().offer = None;
    }

    fn expire(st: &mut State) {
        if st.offer.as_ref().is_some_and(|o| o.expires <= Instant::now()) {
            st.offer = None;
        }
    }

    /// The user pressed "Add device": show a PIN any phone may use for `ttl`.
    /// `None` while locked out.
    pub fn open_pin_offer(&self, ttl: Duration) -> Option<String> {
        let mut st = self.state();
        if st.locked_until.is_some_and(|t| t > Instant::now()) {
            return None;
        }
        let pin = random_pin();
        st.offer = Some(Offer { kind: OfferKind::Pin, secret: pin.clone().into_bytes(), expires: Instant::now() + ttl, wrong: 0 });
        drop(st);
        log::info!("PAIRING PIN: {pin} (valid {} s)", ttl.as_secs());
        self.emit(PairEvent::PinRequested { device_id: String::new(), device_name: String::new(), pin: pin.clone(), valid_for: ttl });
        Some(pin)
    }

    /// A one-time QR pairing code for a phone to scan. `host_addr` is what the
    /// phone should connect to (IP or name). `None` while locked out.
    pub fn open_qr_offer(
        &self,
        host_name: &str,
        host_addr: &str,
        port: u16,
        fingerprint: &Fingerprint,
        ttl: Duration,
    ) -> Option<QrOffer> {
        let mut st = self.state();
        if st.locked_until.is_some_and(|t| t > Instant::now()) {
            return None;
        }
        let secret = random_bytes::<16>();
        st.offer = Some(Offer { kind: OfferKind::Qr, secret: secret.to_vec(), expires: Instant::now() + ttl, wrong: 0 });
        drop(st);
        Some(QrOffer { payload: qr_payload(host_name, host_addr, port, fingerprint, &secret), valid_for: ttl })
    }

    /// The pure state machine for one request on a connection whose server
    /// certificate has `fingerprint`. `peer` is only for log lines.
    pub fn handle_request(
        &self,
        fingerprint: &Fingerprint,
        mode: u8,
        device_id: &str,
        device_name: &str,
        credential: &[u8],
    ) -> Decision {
        if device_id.is_empty() {
            return respond(PAIR_WRONG, "The phone did not identify itself", vec![], false, true);
        }
        match mode {
            PAIR_MODE_TOKEN => {
                if self.trust().is_trusted(device_id, credential) {
                    respond(PAIR_OK, "", vec![], true, false)
                } else {
                    respond(PAIR_UNTRUSTED, "This phone is not paired with this PC. Pair it with a PIN.", vec![], false, false)
                }
            }
            PAIR_MODE_PIN | PAIR_MODE_QR => self.handle_pairing(fingerprint, mode, device_id, device_name, credential),
            _ => respond(PAIR_WRONG, "Unknown pairing mode", vec![], false, true),
        }
    }

    fn handle_pairing(&self, fp: &Fingerprint, mode: u8, device_id: &str, device_name: &str, credential: &[u8]) -> Decision {
        let kind = if mode == PAIR_MODE_QR { OfferKind::Qr } else { OfferKind::Pin };
        let mut st = self.state();
        Self::expire(&mut st);
        let now = Instant::now();

        // Empty credential: the phone asks for a PIN.
        if credential.is_empty() && kind == OfferKind::Pin {
            if st.locked_until.is_some_and(|t| t > now) {
                return respond(PAIR_LOCKED, "Too many wrong PINs. Try again in a little while.", vec![], false, true);
            }
            if matches!(&st.offer, Some(o) if o.kind == OfferKind::Pin) {
                return respond(PAIR_PIN_REQUIRED, "Enter the PIN shown on the PC", vec![], false, false);
            }
            if st.last_pin_request.is_some_and(|t| now.duration_since(t) < PIN_REQUEST_GAP) {
                return respond(PAIR_DISABLED, "Pairing was just requested; wait a moment", vec![], false, true);
            }
            let pin = random_pin();
            st.last_pin_request = Some(now);
            st.offer = Some(Offer { kind: OfferKind::Pin, secret: pin.clone().into_bytes(), expires: now + PIN_TTL, wrong: 0 });
            drop(st);
            log::info!("PAIRING PIN for {device_name:?} ({device_id}): {pin}");
            self.emit(PairEvent::PinRequested {
                device_id: device_id.into(),
                device_name: device_name.into(),
                pin,
                valid_for: PIN_TTL,
            });
            return respond(PAIR_PIN_REQUIRED, "Enter the PIN shown on the PC", vec![], false, false);
        }

        let Some(offer) = st.offer.as_mut().filter(|o| o.kind == kind) else {
            return respond(PAIR_DISABLED, "No pairing is in progress. Start pairing on the PC.", vec![], false, true);
        };
        if ct_eq(&proof(&offer.secret, fp, device_id), credential) {
            st.offer = None;
            drop(st);
            let token = random_bytes::<32>().to_vec();
            if let Err(e) = self.trust().add(device_id, device_name, &token) {
                log::warn!("Cannot save the trust store: {e}; the pairing lasts until the host restarts");
            }
            log::info!("Paired {device_name:?} ({device_id})");
            self.emit(PairEvent::Paired { device_id: device_id.into(), device_name: device_name.into() });
            return respond(PAIR_OK, "Paired", token, true, false);
        }
        offer.wrong += 1;
        if offer.wrong >= MAX_ATTEMPTS {
            st.offer = None;
            st.locked_until = Some(now + LOCKOUT);
            drop(st);
            self.emit(PairEvent::Failed { device_id: device_id.into(), reason: "too many wrong attempts".into() });
            return respond(PAIR_LOCKED, "Too many wrong attempts. Start pairing again on the PC.", vec![], false, true);
        }
        let left = MAX_ATTEMPTS - offer.wrong;
        drop(st);
        self.emit(PairEvent::Failed { device_id: device_id.into(), reason: "wrong PIN".into() });
        respond(PAIR_WRONG, &format!("Wrong PIN. {left} attempt(s) left."), vec![], false, false)
    }

    /// Runs the pre-Hello exchange on a fresh TLS stream. `Ok(device_id)` once
    /// the phone is authenticated; the stream is then positioned at its Hello.
    /// Bounded by [`PAIRING_TIMEOUT`].
    pub async fn negotiate<S>(&self, stream: &mut S, fingerprint: &Fingerprint, peer: Option<IpAddr>) -> io::Result<String>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let run = async {
            let mut reader = MessageReader::new(&mut *stream);
            for _ in 0..MAX_REQUESTS_PER_CONNECTION {
                let msg = reader.next().await?;
                let Message::PairRequest { mode, device_id, device_name, credential } = msg else {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "expected a PairRequest first"));
                };
                let d = self.handle_request(fingerprint, mode, &device_id, &device_name, &credential);
                drop(reader);
                stream.write_all(&d.reply.encode()).await?;
                stream.flush().await?;
                if d.accepted {
                    return Ok(device_id);
                }
                if d.close {
                    let why = match &d.reply {
                        Message::PairResponse { message, .. } => message.clone(),
                        _ => String::new(),
                    };
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, why));
                }
                reader = MessageReader::new(&mut *stream);
            }
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "too many pairing requests"))
        };
        match tokio::time::timeout(PAIRING_TIMEOUT, run).await {
            Ok(r) => {
                if let Err(e) = &r {
                    log::info!("Pairing with {peer:?} ended: {e}");
                }
                r
            }
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "pairing timed out")),
        }
    }
}

// ---- QR ------------------------------------------------------------------------

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `displayswarm://pair?h=<addr>&p=<port>&fp=<sha256 hex>&s=<secret hex>&n=<name>`.
pub fn qr_payload(host_name: &str, host_addr: &str, port: u16, fp: &Fingerprint, secret: &[u8]) -> String {
    format!("displayswarm://pair?h={}&p={port}&fp={}&s={}&n={}", pct(host_addr), fingerprint_hex(fp), hex(secret), pct(host_name))
}

/// The QR code as an SVG document.
pub fn qr_svg(payload: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(payload.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code.render::<qrcode::render::svg::Color>().min_dimensions(240, 240).quiet_zone(true).build())
}

/// The QR code as text (two rows per line, block characters) for terminals.
pub fn qr_text(payload: &str) -> Result<String, String> {
    let code = qrcode::QrCode::new(payload.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: Fingerprint = [7u8; 32];

    fn pin_of(m: &PairingManager) -> String {
        m.pending_offer().unwrap().pin.unwrap()
    }

    fn status(d: &Decision) -> u8 {
        match &d.reply {
            Message::PairResponse { status, .. } => *status,
            _ => panic!("not a PairResponse"),
        }
    }

    fn token_of(d: &Decision) -> Vec<u8> {
        match &d.reply {
            Message::PairResponse { token, .. } => token.clone(),
            _ => panic!(),
        }
    }

    #[test]
    fn right_pin_pairs_and_the_token_then_reconnects_without_a_pin() {
        let m = PairingManager::in_memory();
        let d = m.handle_request(&FP, PAIR_MODE_PIN, "phone", "Phone", &[]);
        assert_eq!(status(&d), PAIR_PIN_REQUIRED);
        let pin = pin_of(&m);
        let d = m.handle_request(&FP, PAIR_MODE_PIN, "phone", "Phone", &proof(pin.as_bytes(), &FP, "phone"));
        assert!(d.accepted);
        assert_eq!(status(&d), PAIR_OK);
        let token = token_of(&d);
        assert_eq!(token.len(), 32);
        assert!(m.pending_offer().is_none(), "an offer is single use");

        let again = m.handle_request(&FP, PAIR_MODE_TOKEN, "phone", "Phone", &token);
        assert!(again.accepted);
        let bad = m.handle_request(&FP, PAIR_MODE_TOKEN, "phone", "Phone", &[0u8; 32]);
        assert_eq!(status(&bad), PAIR_UNTRUSTED);
        let other = m.handle_request(&FP, PAIR_MODE_TOKEN, "other", "Other", &token);
        assert_eq!(status(&other), PAIR_UNTRUSTED, "a token is bound to its device");
    }

    #[test]
    fn wrong_pin_is_refused_then_the_offer_locks_after_the_retry_limit() {
        let m = PairingManager::in_memory();
        m.handle_request(&FP, PAIR_MODE_PIN, "p", "P", &[]);
        let pin = pin_of(&m);
        let wrong = proof(b"not the pin", &FP, "p");
        for i in 1..MAX_ATTEMPTS {
            let d = m.handle_request(&FP, PAIR_MODE_PIN, "p", "P", &wrong);
            assert_eq!(status(&d), PAIR_WRONG, "attempt {i}");
            assert!(!d.close);
        }
        let d = m.handle_request(&FP, PAIR_MODE_PIN, "p", "P", &wrong);
        assert_eq!(status(&d), PAIR_LOCKED);
        assert!(d.close);
        // The right PIN no longer works: the offer is void.
        let d = m.handle_request(&FP, PAIR_MODE_PIN, "p", "P", &proof(pin.as_bytes(), &FP, "p"));
        assert!(!d.accepted);
        // And asking for a new PIN is refused during the lock-out.
        let d = m.handle_request(&FP, PAIR_MODE_PIN, "p", "P", &[]);
        assert_eq!(status(&d), PAIR_LOCKED);
        assert!(m.open_pin_offer(Duration::from_secs(60)).is_none());
    }

    #[test]
    fn a_proof_for_another_certificate_does_not_pair() {
        let m = PairingManager::in_memory();
        let pin = m.open_pin_offer(Duration::from_secs(60)).unwrap();
        let mitm_fp = [9u8; 32];
        let d = m.handle_request(&FP, PAIR_MODE_PIN, "p", "P", &proof(pin.as_bytes(), &mitm_fp, "p"));
        assert!(!d.accepted);
    }

    #[test]
    fn pairing_without_an_offer_or_after_expiry_is_disabled() {
        let m = PairingManager::in_memory();
        let d = m.handle_request(&FP, PAIR_MODE_PIN, "p", "P", &[1, 2, 3]);
        assert_eq!(status(&d), PAIR_DISABLED);
        let pin = m.open_pin_offer(Duration::ZERO).unwrap();
        let d = m.handle_request(&FP, PAIR_MODE_PIN, "p", "P", &proof(pin.as_bytes(), &FP, "p"));
        assert_eq!(status(&d), PAIR_DISABLED, "expired");
    }

    #[test]
    fn qr_offer_pairs_with_its_secret_and_carries_the_fingerprint() {
        let m = PairingManager::in_memory();
        let qr = m.open_qr_offer("my pc", "192.168.1.5", 9999, &FP, Duration::from_secs(60)).unwrap();
        assert!(qr.payload.starts_with("displayswarm://pair?h=192.168.1.5&p=9999&fp="));
        assert!(qr.payload.contains(&fingerprint_hex(&FP)));
        assert!(qr.payload.ends_with("&n=my%20pc"));
        let secret_hex = qr.payload.split("&s=").nth(1).unwrap().split('&').next().unwrap();
        let secret: Vec<u8> = (0..secret_hex.len()).step_by(2).map(|i| u8::from_str_radix(&secret_hex[i..i + 2], 16).unwrap()).collect();
        // A PIN-mode attempt cannot use the QR offer.
        let d = m.handle_request(&FP, PAIR_MODE_PIN, "p", "P", &proof(&secret, &FP, "p"));
        assert!(!d.accepted);
        let d = m.handle_request(&FP, PAIR_MODE_QR, "p", "P", &proof(&secret, &FP, "p"));
        assert!(d.accepted);
        assert!(qr_text(&qr.payload).unwrap().len() > 100);
        assert!(qr_svg(&qr.payload).unwrap().contains("<svg"));
    }

    #[test]
    fn events_reach_the_listener_and_revoke_forgets_the_phone() {
        let m = PairingManager::in_memory();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        m.set_event_listener(move |e| s2.lock().unwrap().push(e));
        m.handle_request(&FP, PAIR_MODE_PIN, "p", "Pixel", &[]);
        let pin = pin_of(&m);
        let d = m.handle_request(&FP, PAIR_MODE_PIN, "p", "Pixel", &proof(pin.as_bytes(), &FP, "p"));
        let token = token_of(&d);
        {
            let seen = seen.lock().unwrap();
            assert!(matches!(&seen[0], PairEvent::PinRequested { pin: p, device_name, .. } if *p == pin && device_name == "Pixel"));
            assert!(matches!(&seen[1], PairEvent::Paired { device_id, .. } if device_id == "p"));
        }
        assert_eq!(m.trusted_devices().len(), 1);
        assert!(m.revoke("p"));
        let d = m.handle_request(&FP, PAIR_MODE_TOKEN, "p", "Pixel", &token);
        assert_eq!(status(&d), PAIR_UNTRUSTED);
    }

    #[test]
    fn trust_store_persists_privately_and_stores_only_a_hash() {
        let path = std::env::temp_dir().join(format!("displayswarm-trust-{}", std::process::id())).join("trusted.json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
        let mut s = TrustStore::open(&path).unwrap();
        s.add("a", "A", b"secret-token").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("secret-token"));
        let s2 = TrustStore::open(&path).unwrap();
        assert!(s2.is_trusted("a", b"secret-token"));
        assert!(!s2.is_trusted("a", b"other"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Shared with `PairingTest.kt`: both sides must compute the same proof.
    #[test]
    fn proof_matches_the_cross_platform_vector() {
        assert_eq!(hex(&proof(b"123456", &FP, "dev1")), "32bfa3ca8a127b2f7368af0ba49477c498ddd7f02b9116fa878ae37000f86cc0");
    }

    #[test]
    fn pins_are_six_digits() {
        for _ in 0..50 {
            let p = random_pin();
            assert_eq!(p.len(), 6);
            assert!(p.bytes().all(|b| b.is_ascii_digit()));
        }
    }
}
