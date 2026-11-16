//! Pairing and trust for network connections.
//!
//! Every connection is TLS (see [`super::tls`]) and starts with a `PairRequest`
//! before the v2 `Hello`:
//!
//! * A **trusted** phone sends `PAIR_MODE_TOKEN` with the per-device token it
//!   was given when it paired. No PIN is needed again.
//! * A new phone sends `PAIR_MODE_PAKE` with an empty credential. The host shows
//!   a 6-digit PIN (log line, [`PairEvent::PinRequested`]) and answers
//!   `PAIR_PIN_REQUIRED`. The phone and host then run SPAKE2
//!   ([`super::spake2`]) with the PIN: the phone sends pA, the host answers pB
//!   (`PAIR_PAKE_SHARE`), the phone sends its key confirmation, and the host
//!   answers with the token and its own confirmation. The identities bind the
//!   device id and the certificate the phone saw, so a man in the middle with
//!   another certificate cannot confirm, and every run tests one PIN guess at
//!   most, online: nothing captured can be brute-forced afterwards.
//! * The older `PAIR_MODE_PIN` proof (an HMAC keyed by the PIN) is refused:
//!   one captured proof gave the PIN away offline, and accepting it as a
//!   fallback would let an attacker force it. Such a phone is told to update.
//! * A phone that scanned the QR code ([`PairingManager::open_qr_offer`]) pins
//!   the fingerprint from the code and sends `PAIR_MODE_QR` with a proof keyed by
//!   the code's 128-bit one-time secret.
//!
//! A successful pairing yields a random 32-byte token, returned once in the
//! `PairResponse`; only its SHA-256 is stored (`trusted.json`, mode 0600).
//! Each source address gets [`MAX_ATTEMPTS`] wrong proofs per offer, then it is
//! locked out for [`LOCKOUT`]; other phones can still use the offer, so one
//! device on the network cannot stop everyone else from pairing. An offer
//! takes at most [`MAX_OFFER_ATTEMPTS`] wrong proofs from all addresses
//! together, then it is void and new offers are refused for [`LOCKOUT`].
//!
//! Before a phone is authenticated the host reads nothing but small
//! `PairRequest` frames on the control channel ([`read_pair_request`]), so an
//! unauthenticated peer cannot make it buffer more than a few kilobytes.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::tls::{fingerprint_hex, Fingerprint};
use super::spake2;
use crate::protocol::wire::{
    Header, Message, CH_CONTROL, HEADER_SIZE, MSG_PAIR_REQUEST, PAIR_MODE_PAKE, PAIR_PAKE_SHARE, PAKE_CONFIRM,
    PAKE_SHARE, PAIR_DISABLED, PAIR_LOCKED, PAIR_MODE_PIN, PAIR_MODE_QR, PAIR_MODE_TOKEN, PAIR_OK,
    PAIR_PIN_REQUIRED, PAIR_UNTRUSTED, PAIR_WRONG,
};

/// What a phone that only knows the old PIN proof is told.
pub const UPDATE_THE_APP: &str =
    "This PC needs a newer DisplaySwarm app to pair with a PIN. Update the app on this phone.";

/// Wrong proofs one source address gets per offer.
pub const MAX_ATTEMPTS: u32 = 3;
/// Wrong proofs an offer tolerates from all addresses together.
pub const MAX_OFFER_ATTEMPTS: u32 = 10;
/// Largest frame read before the phone is authenticated.
pub const MAX_PAIR_FRAME: usize = 4096;
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
    ///
    /// Only the same id is replaced. Dropping entries with the same *name* (to
    /// clean up after a reinstall) would un-pair a second phone of the same
    /// model, which sends the same name; an old install's entry is harmless
    /// (its token is gone) and the user removes it from the paired list.
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
    /// Every address is refused new offers until then (an offer was exhausted).
    locked_until: Option<Instant>,
    last_pin_request: Option<Instant>,
    /// Wrong proofs per source address against the current offer.
    peer_wrong: HashMap<Option<IpAddr>, u32>,
    /// Addresses locked out, until when.
    peer_locked: HashMap<Option<IpAddr>, Instant>,
}

impl State {
    /// A fresh offer: every address starts with a clean count.
    fn set_offer(&mut self, offer: Offer) {
        self.offer = Some(offer);
        self.peer_wrong.clear();
    }

    fn peer_is_locked(&mut self, peer: Option<IpAddr>, now: Instant) -> bool {
        self.peer_locked.retain(|_, until| *until > now);
        self.peer_locked.contains_key(&peer) || self.locked_until.is_some_and(|t| t > now)
    }
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
    /// Accept every phone without a PIN or token (still over TLS). Off by default.
    allow_unpaired: AtomicBool,
}

/// `DISPLAYSWARM_ALLOW_UNPAIRED=1` turns pairing off: any phone that can reach the port is let in.
pub const ALLOW_UNPAIRED_ENV: &str = "DISPLAYSWARM_ALLOW_UNPAIRED";

/// Whether [`ALLOW_UNPAIRED_ENV`] is set. It overrides the "Skip pairing" setting.
pub fn allow_unpaired_from_env() -> bool {
    std::env::var(ALLOW_UNPAIRED_ENV)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
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
        Arc::new(Self { state: Mutex::default(), trust: Mutex::new(trust), listener: Mutex::new(None), allow_unpaired: AtomicBool::new(false) })
    }

    pub fn in_memory() -> Arc<Self> {
        Self::new(TrustStore::in_memory())
    }

    /// The trust store in the user's config directory; in memory if unusable.
    pub fn with_defaults() -> Arc<Self> {
        let m = Self::new(TrustStore::open_default().unwrap_or_else(|e| {
            log::warn!("Trust store unavailable ({e}); pairings will last only for this run");
            TrustStore::in_memory()
        }));
        m.set_allow_unpaired(allow_unpaired_from_env());
        m
    }

    /// Lets every phone in without pairing. Anyone on the network can then use this PC's screen and input.
    pub fn set_allow_unpaired(&self, on: bool) {
        let was = self.allow_unpaired.swap(on, Ordering::Relaxed);
        if on && !was {
            log::warn!("Pairing is OFF: any phone on the network can connect");
        } else if was && !on {
            log::info!("Pairing is on again");
        }
    }

    pub fn allow_unpaired(&self) -> bool {
        self.allow_unpaired.load(Ordering::Relaxed)
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
        st.set_offer(Offer { kind: OfferKind::Pin, secret: pin.clone().into_bytes(), expires: Instant::now() + ttl, wrong: 0 });
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
        st.set_offer(Offer { kind: OfferKind::Qr, secret: secret.to_vec(), expires: Instant::now() + ttl, wrong: 0 });
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
        self.handle_request_from(None, fingerprint, mode, device_id, device_name, credential)
    }

    /// [`Self::handle_request`] for a request from `peer` (the lock-out is per address).
    pub fn handle_request_from(
        &self,
        peer: Option<IpAddr>,
        fingerprint: &Fingerprint,
        mode: u8,
        device_id: &str,
        device_name: &str,
        credential: &[u8],
    ) -> Decision {
        if device_id.is_empty() {
            return respond(PAIR_WRONG, "The phone did not identify itself", vec![], false, true);
        }
        if self.allow_unpaired.load(Ordering::Relaxed) && matches!(mode, PAIR_MODE_TOKEN | PAIR_MODE_PIN | PAIR_MODE_QR | PAIR_MODE_PAKE) {
            return respond(PAIR_OK, "", vec![], true, false);
        }
        match mode {
            PAIR_MODE_TOKEN => {
                if self.trust().is_trusted(device_id, credential) {
                    respond(PAIR_OK, "", vec![], true, false)
                } else {
                    respond(PAIR_UNTRUSTED, "This phone is not paired with this PC. Pair it with a PIN.", vec![], false, false)
                }
            }
            PAIR_MODE_PIN => respond(PAIR_DISABLED, UPDATE_THE_APP, vec![], false, true),
            PAIR_MODE_QR => self.handle_pairing(peer, fingerprint, mode, device_id, device_name, credential),
            PAIR_MODE_PAKE if credential.is_empty() => self.handle_pairing(peer, fingerprint, PAIR_MODE_PIN, device_id, device_name, &[]),
            // The SPAKE2 steps need the connection's state: see `Exchange`.
            PAIR_MODE_PAKE => respond(PAIR_WRONG, "Unexpected pairing step", vec![], false, true),
            _ => respond(PAIR_WRONG, "Unknown pairing mode", vec![], false, true),
        }
    }

    fn handle_pairing(&self, peer: Option<IpAddr>, fp: &Fingerprint, mode: u8, device_id: &str, device_name: &str, credential: &[u8]) -> Decision {
        let kind = if mode == PAIR_MODE_QR { OfferKind::Qr } else { OfferKind::Pin };
        let mut st = self.state();
        Self::expire(&mut st);
        let now = Instant::now();

        if st.peer_is_locked(peer, now) {
            return respond(PAIR_LOCKED, "Too many wrong PINs. Try again in a little while.", vec![], false, true);
        }
        // Empty credential: the phone asks for a PIN.
        if credential.is_empty() && kind == OfferKind::Pin {
            if matches!(&st.offer, Some(o) if o.kind == OfferKind::Pin) {
                return respond(PAIR_PIN_REQUIRED, "Enter the PIN shown on the PC", vec![], false, false);
            }
            if st.last_pin_request.is_some_and(|t| now.duration_since(t) < PIN_REQUEST_GAP) {
                return respond(PAIR_DISABLED, "Pairing was just requested; wait a moment", vec![], false, true);
            }
            let pin = random_pin();
            st.last_pin_request = Some(now);
            st.set_offer(Offer { kind: OfferKind::Pin, secret: pin.clone().into_bytes(), expires: now + PIN_TTL, wrong: 0 });
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
            return self.pair(device_id, device_name, &[]);
        }
        offer.wrong += 1;
        self.wrong_attempt(st, peer, now, device_id)
    }

    /// Counts a wrong proof (the offer's count already raised) and answers it.
    fn wrong_attempt(&self, mut st: std::sync::MutexGuard<'_, State>, peer: Option<IpAddr>, now: Instant, device_id: &str) -> Decision {
        if st.offer.as_ref().is_some_and(|o| o.wrong >= MAX_OFFER_ATTEMPTS) {
            st.offer = None;
            st.locked_until = Some(now + LOCKOUT);
            drop(st);
            self.emit(PairEvent::Failed { device_id: device_id.into(), reason: "too many wrong attempts".into() });
            return respond(PAIR_LOCKED, "Too many wrong attempts. Start pairing again on the PC.", vec![], false, true);
        }
        let mine = st.peer_wrong.entry(peer).or_insert(0);
        *mine += 1;
        let mine = *mine;
        if mine >= MAX_ATTEMPTS {
            st.peer_locked.insert(peer, now + LOCKOUT);
            drop(st);
            self.emit(PairEvent::Failed { device_id: device_id.into(), reason: "too many wrong attempts".into() });
            return respond(PAIR_LOCKED, "Too many wrong attempts. Try again in a little while.", vec![], false, true);
        }
        let left = MAX_ATTEMPTS - mine;
        drop(st);
        self.emit(PairEvent::Failed { device_id: device_id.into(), reason: "wrong PIN".into() });
        respond(PAIR_WRONG, &format!("Wrong PIN. {left} attempt(s) left."), vec![], false, false)
    }

    /// Pairs `device_id`: stores a new token and answers with it (and, after
    /// SPAKE2, the host's confirmation appended).
    fn pair(&self, device_id: &str, device_name: &str, extra: &[u8]) -> Decision {
        let token = random_bytes::<32>().to_vec();
        if let Err(e) = self.trust().add(device_id, device_name, &token) {
            log::warn!("Cannot save the trust store: {e}; the pairing lasts until the host restarts");
        }
        log::info!("Paired {device_name:?} ({device_id})");
        self.emit(PairEvent::Paired { device_id: device_id.into(), device_name: device_name.into() });
        let mut reply = token;
        reply.extend_from_slice(extra);
        respond(PAIR_OK, "Paired", reply, true, false)
    }

    /// SPAKE2 step 2: the phone's share. Answers pB and keeps the run's keys,
    /// or refuses. Needs a live PIN offer.
    fn pake_share(&self, peer: Option<IpAddr>, fp: &Fingerprint, device_id: &str, pa: &[u8]) -> Result<(spake2::Keys, Decision), Decision> {
        let mut st = self.state();
        Self::expire(&mut st);
        if st.peer_is_locked(peer, Instant::now()) {
            return Err(respond(PAIR_LOCKED, "Too many wrong PINs. Try again in a little while.", vec![], false, true));
        }
        let Some(pin) = st.offer.as_ref().filter(|o| o.kind == OfferKind::Pin).map(|o| String::from_utf8_lossy(&o.secret).into_owned()) else {
            return Err(respond(PAIR_DISABLED, "No pairing is in progress. Start pairing on the PC.", vec![], false, true));
        };
        drop(st);
        let host = spake2::Party::start_b(spake2::password_scalar(&pin), spake2::identity_a(device_id), spake2::identity_b(fp));
        let keys = host.finish_b(pa).ok_or_else(|| respond(PAIR_WRONG, "Invalid pairing message", vec![], false, true))?;
        Ok((keys, respond(PAIR_PAKE_SHARE, "", host.share().to_vec(), false, false)))
    }

    /// SPAKE2 step 3: the phone's confirmation. A wrong one is a wrong PIN.
    fn pake_confirm(&self, peer: Option<IpAddr>, keys: &spake2::Keys, ca: &[u8], device_id: &str, device_name: &str) -> Decision {
        let mut st = self.state();
        Self::expire(&mut st);
        let now = Instant::now();
        if st.peer_is_locked(peer, now) {
            return respond(PAIR_LOCKED, "Too many wrong PINs. Try again in a little while.", vec![], false, true);
        }
        let Some(offer) = st.offer.as_mut().filter(|o| o.kind == OfferKind::Pin) else {
            return respond(PAIR_DISABLED, "No pairing is in progress. Start pairing on the PC.", vec![], false, true);
        };
        if spake2::same(ca, &keys.confirm_a()) {
            st.offer = None;
            drop(st);
            return self.pair(device_id, device_name, &keys.confirm_b());
        }
        offer.wrong += 1;
        self.wrong_attempt(st, peer, now, device_id)
    }

    /// Runs the pre-Hello exchange on a fresh TLS stream. `Ok(device_id)` once
    /// the phone is authenticated; the stream is then positioned at its Hello.
    /// Bounded by [`PAIRING_TIMEOUT`].
    pub async fn negotiate<S>(&self, stream: &mut S, fingerprint: &Fingerprint, peer: Option<IpAddr>) -> io::Result<String>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let run = async {
            let mut exchange = Exchange::default();
            for _ in 0..MAX_REQUESTS_PER_CONNECTION {
                let msg = read_pair_request(&mut *stream).await?;
                let Message::PairRequest { mode, device_id, device_name, credential } = msg else {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "expected a PairRequest first"));
                };
                let d = exchange.handle(self, peer, fingerprint, mode, &device_id, &device_name, &credential);
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

/// The pairing state of one connection: the SPAKE2 run in progress, if any.
#[derive(Default)]
pub struct Exchange {
    keys: Option<spake2::Keys>,
}

impl Exchange {
    /// Handles one request on this connection.
    pub fn handle(
        &mut self,
        m: &PairingManager,
        peer: Option<IpAddr>,
        fp: &Fingerprint,
        mode: u8,
        device_id: &str,
        device_name: &str,
        credential: &[u8],
    ) -> Decision {
        if device_id.is_empty() || mode != PAIR_MODE_PAKE || credential.is_empty() || m.allow_unpaired() {
            return m.handle_request_from(peer, fp, mode, device_id, device_name, credential);
        }
        match (credential[0], &credential[1..]) {
            (PAKE_SHARE, pa) if pa.len() == spake2::POINT_LEN => match m.pake_share(peer, fp, device_id, pa) {
                Ok((keys, reply)) => {
                    self.keys = Some(keys);
                    reply
                }
                Err(refusal) => refusal,
            },
            (PAKE_CONFIRM, ca) if ca.len() == spake2::CONFIRM_LEN => match self.keys.take() {
                Some(keys) => m.pake_confirm(peer, &keys, ca, device_id, device_name),
                None => respond(PAIR_WRONG, "Unexpected pairing step", vec![], false, true),
            },
            _ => respond(PAIR_WRONG, "Invalid pairing message", vec![], false, true),
        }
    }
}

/// Reads one `PairRequest` from a peer that is not authenticated yet: a single
/// unfragmented control frame of at most [`MAX_PAIR_FRAME`] bytes, and nothing
/// else. The general reader would accept a 16 MiB message on the video channel.
pub async fn read_pair_request<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Message> {
    let bad = |why: String| io::Error::new(io::ErrorKind::InvalidData, why);
    let mut raw = [0u8; HEADER_SIZE];
    stream.read_exact(&mut raw).await?;
    let h = Header::parse(&raw)?;
    h.check()?;
    if h.channel != CH_CONTROL || h.msg_type != MSG_PAIR_REQUEST || h.flags != 0 {
        return Err(bad(format!("expected a PairRequest first, got type {:#04x} on channel {}", h.msg_type, h.channel)));
    }
    if h.len as usize > MAX_PAIR_FRAME {
        return Err(bad(format!("a {}-byte PairRequest is too large", h.len)));
    }
    let mut payload = vec![0u8; h.len as usize];
    stream.read_exact(&mut payload).await?;
    Message::decode(h.channel, h.msg_type, &payload)
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

    #[test]
    fn unpaired_phones_are_let_in_only_when_allowed() {
        let m = PairingManager::in_memory();
        assert_eq!(status(&m.handle_request(&FP, PAIR_MODE_TOKEN, "dev", "Phone", b"")), PAIR_UNTRUSTED);
        m.set_allow_unpaired(true);
        let d = m.handle_request(&FP, PAIR_MODE_TOKEN, "dev", "Phone", b"");
        assert_eq!(status(&d), PAIR_OK);
        assert!(d.accepted);
        assert!(!m.handle_request(&FP, PAIR_MODE_TOKEN, "", "Phone", b"").accepted, "still needs a device id");
    }

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

    /// The phone's side of a SPAKE2 run with `pin`, having seen `seen_fp`:
    /// the host's last answer, and the keys the phone derived (if it got that far).
    fn phone_attempt(
        m: &PairingManager,
        peer: Option<IpAddr>,
        host_fp: &Fingerprint,
        seen_fp: &Fingerprint,
        id: &str,
        pin: &str,
    ) -> (Decision, Option<spake2::Keys>) {
        let mut ex = Exchange::default();
        let phone = spake2::Party::start_a(spake2::password_scalar(pin), spake2::identity_a(id), spake2::identity_b(seen_fp));
        let mut share = vec![PAKE_SHARE];
        share.extend_from_slice(phone.share());
        let d = ex.handle(m, peer, host_fp, PAIR_MODE_PAKE, id, "Phone", &share);
        if status(&d) != PAIR_PAKE_SHARE {
            return (d, None);
        }
        let keys = phone.finish_a(&token_of(&d)).expect("valid pB");
        let mut confirm = vec![PAKE_CONFIRM];
        confirm.extend_from_slice(&keys.confirm_a());
        (ex.handle(m, peer, host_fp, PAIR_MODE_PAKE, id, "Phone", &confirm), Some(keys))
    }

    fn ask_pin(m: &PairingManager, peer: Option<IpAddr>, id: &str) -> Decision {
        m.handle_request_from(peer, &FP, PAIR_MODE_PAKE, id, "Phone", &[])
    }

    #[test]
    fn the_right_pin_pairs_over_spake2_and_the_token_then_reconnects() {
        let m = PairingManager::in_memory();
        let d = ask_pin(&m, None, "phone");
        assert_eq!(status(&d), PAIR_PIN_REQUIRED);
        let pin = pin_of(&m);
        let (d, keys) = phone_attempt(&m, None, &FP, &FP, "phone", &pin);
        assert!(d.accepted);
        assert_eq!(status(&d), PAIR_OK);
        let reply = token_of(&d);
        assert_eq!(reply.len(), 64, "token and the host's confirmation");
        let (token, confirm_b) = reply.split_at(32);
        assert_eq!(confirm_b, keys.unwrap().confirm_b(), "the phone can check the host knew the PIN");
        assert!(m.pending_offer().is_none(), "an offer is single use");

        let again = m.handle_request(&FP, PAIR_MODE_TOKEN, "phone", "Phone", token);
        assert!(again.accepted);
        let bad = m.handle_request(&FP, PAIR_MODE_TOKEN, "phone", "Phone", &[0u8; 32]);
        assert_eq!(status(&bad), PAIR_UNTRUSTED);
        let other = m.handle_request(&FP, PAIR_MODE_TOKEN, "other", "Other", token);
        assert_eq!(status(&other), PAIR_UNTRUSTED, "a token is bound to its device");
    }

    #[test]
    fn the_old_pin_proof_is_refused_with_a_reason() {
        let m = PairingManager::in_memory();
        let pin = m.open_pin_offer(Duration::from_secs(60)).unwrap();
        for credential in [vec![], proof(pin.as_bytes(), &FP, "p")] {
            let d = m.handle_request(&FP, PAIR_MODE_PIN, "p", "P", &credential);
            assert_eq!(status(&d), PAIR_DISABLED);
            assert!(!d.accepted && d.close);
            assert!(matches!(&d.reply, Message::PairResponse { message, .. } if message == UPDATE_THE_APP));
        }
    }

    #[test]
    fn a_confirmation_without_a_share_or_garbage_is_refused() {
        let m = PairingManager::in_memory();
        ask_pin(&m, None, "p");
        let mut ex = Exchange::default();
        let mut confirm = vec![PAKE_CONFIRM];
        confirm.extend_from_slice(&[0u8; 32]);
        assert_eq!(status(&ex.handle(&m, None, &FP, PAIR_MODE_PAKE, "p", "P", &confirm)), PAIR_WRONG);
        let mut bad_point = vec![PAKE_SHARE];
        bad_point.extend_from_slice(&[4u8; 65]);
        assert_eq!(status(&ex.handle(&m, None, &FP, PAIR_MODE_PAKE, "p", "P", &bad_point)), PAIR_WRONG);
    }
    #[test]
    fn wrong_pins_lock_out_that_address_but_not_other_phones() {
        let m = PairingManager::in_memory();
        let attacker = Some(IpAddr::from([192, 168, 1, 66]));
        let phone = Some(IpAddr::from([192, 168, 1, 20]));
        ask_pin(&m, phone, "p");
        let pin = pin_of(&m);
        let wrong = if pin == "000000" { "000001" } else { "000000" };
        for i in 1..MAX_ATTEMPTS {
            let (d, _) = phone_attempt(&m, attacker, &FP, &FP, "x", wrong);
            assert_eq!(status(&d), PAIR_WRONG, "attempt {i}");
            assert!(!d.close);
        }
        let (d, _) = phone_attempt(&m, attacker, &FP, &FP, "x", wrong);
        assert_eq!(status(&d), PAIR_LOCKED);
        assert!(d.close);
        // Locked out: even the right PIN from that address is refused...
        let (d, _) = phone_attempt(&m, attacker, &FP, &FP, "x", &pin);
        assert_eq!(status(&d), PAIR_LOCKED);
        // ...but the phone the user is pairing still can.
        let (d, _) = phone_attempt(&m, phone, &FP, &FP, "p", &pin);
        assert!(d.accepted);
    }
    #[test]
    fn an_offer_is_void_after_too_many_wrong_proofs_from_everyone() {
        let m = PairingManager::in_memory();
        let pin = m.open_pin_offer(Duration::from_secs(60)).unwrap();
        let wrong = if pin == "000000" { "000001" } else { "000000" };
        let mut last = None;
        for i in 0..MAX_OFFER_ATTEMPTS {
            // A new address each time, as an attacker spoofing addresses would.
            let peer = Some(IpAddr::from([10, 0, 0, i as u8 + 1]));
            last = Some(phone_attempt(&m, peer, &FP, &FP, "x", wrong).0);
        }
        assert_eq!(status(&last.unwrap()), PAIR_LOCKED);
        let (d, _) = phone_attempt(&m, Some(IpAddr::from([10, 9, 9, 9])), &FP, &FP, "p", &pin);
        assert!(!d.accepted, "the offer is void");
        assert!(m.open_pin_offer(Duration::from_secs(60)).is_none(), "and new offers wait out the lock-out");
    }
    #[test]
    fn two_phones_of_the_same_model_stay_paired() {
        let mut s = TrustStore::in_memory();
        s.add("tablet-1", "Galaxy Tab S9", b"t1").unwrap();
        s.add("tablet-2", "Galaxy Tab S9", b"t2").unwrap();
        assert!(s.is_trusted("tablet-1", b"t1") && s.is_trusted("tablet-2", b"t2"));
        s.add("tablet-1", "Galaxy Tab S9", b"t1b").unwrap();
        assert!(!s.is_trusted("tablet-1", b"t1") && s.is_trusted("tablet-1", b"t1b"));
        assert_eq!(s.list().len(), 2);
    }

    #[tokio::test]
    async fn only_a_small_pair_request_is_read_before_authentication() {
        use crate::protocol::wire::{write_frames, CH_VIDEO, MAX_FRAGMENT, MSG_VIDEO_FRAME};
        let ok = Message::PairRequest { mode: PAIR_MODE_PIN, device_id: "d".into(), device_name: "n".into(), credential: vec![] };
        let bytes = ok.encode();
        assert_eq!(read_pair_request(&mut &bytes[..]).await.unwrap(), ok);
        // A video frame before pairing is refused without reading its payload.
        let mut video = Vec::new();
        write_frames(CH_VIDEO, MSG_VIDEO_FRAME, &[&[0u8; 1000]], MAX_FRAGMENT, &mut video);
        assert!(read_pair_request(&mut &video[..]).await.is_err());
        // So is an oversized PairRequest.
        let big = Message::PairRequest { mode: PAIR_MODE_PIN, device_id: "d".into(), device_name: "n".repeat(5000), credential: vec![] };
        assert!(read_pair_request(&mut &big.encode()[..]).await.is_err());
    }

    #[test]
    fn a_phone_that_saw_another_certificate_cannot_pair() {
        let m = PairingManager::in_memory();
        let pin = m.open_pin_offer(Duration::from_secs(60)).unwrap();
        // A man in the middle relays with its own certificate: the phone binds
        // the run to the certificate it saw, the host to its own.
        let mitm_fp = [9u8; 32];
        let (d, _) = phone_attempt(&m, None, &FP, &mitm_fp, "p", &pin);
        assert!(!d.accepted);
        assert_eq!(status(&d), PAIR_WRONG);
    }
    #[test]
    fn pairing_without_an_offer_or_after_expiry_is_disabled() {
        let m = PairingManager::in_memory();
        let (d, _) = phone_attempt(&m, None, &FP, &FP, "p", "123456");
        assert_eq!(status(&d), PAIR_DISABLED);
        let pin = m.open_pin_offer(Duration::ZERO).unwrap();
        let (d, _) = phone_attempt(&m, None, &FP, &FP, "p", &pin);
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
        m.handle_request(&FP, PAIR_MODE_PAKE, "p", "Pixel", &[]);
        let pin = pin_of(&m);
        let (d, _) = phone_attempt(&m, None, &FP, &FP, "p", &pin);
        let token = token_of(&d)[..32].to_vec();
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
