//! SPAKE2 (RFC 9382), ciphersuite SPAKE2-P256-SHA256-HKDF-HMAC, for pairing
//! with a PIN.
//!
//! Why: a 6-digit PIN used directly as a MAC key can be recovered offline from
//! one captured proof (a million guesses take well under a second), so a host
//! impostor that got the phone to send its proof could pair with the real host.
//! With a PAKE each run tests at most one guess, and only online, which the
//! host's attempt limits then bound.
//!
//! Roles: A is the phone, B the host. Their identities bind the device id and
//! the host certificate's fingerprint (what the phone saw in TLS), and the AAD
//! names this protocol, so a run against a man in the middle with another
//! certificate cannot be confirmed. The password scalar is
//! `w = SHA-512("displayswarm-pin-v1" || PIN) mod n`, a hash 256 bits longer
//! than the group order, as the RFC suggests, to keep the reduction unbiased.
//!
//! Messages are SEC1 uncompressed points (65 bytes) and 32-byte HMACs.
//! `protocol/v2-vectors.txt` has the shared vectors; `Spake2.kt` mirrors this.

use hmac::{Hmac, KeyInit, Mac};
use p256::elliptic_curve::ff::{Field, PrimeField};
use p256::elliptic_curve::group::Group;
use p256::elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
use p256::{AffinePoint, FieldBytes, ProjectivePoint, Scalar, Sec1Point};
use sha2::{Digest, Sha256, Sha512};

type HmacSha256 = Hmac<Sha256>;

/// RFC 9382 section 6, P-256 M and N (compressed SEC1).
const M_HEX: &str = "02886e2f97ace46e55ba9dd7242579f2993b64e16ef3dcab95afd497333d8fa12f";
const N_HEX: &str = "03d8bbd6c639c62937b04d997f38c3770719c629d7014d49a24b4f98baa1292b49";

/// The AAD of every DisplaySwarm run.
pub const AAD: &[u8] = b"displayswarm-pair-pake-v1";

/// Length of a point on the wire.
pub const POINT_LEN: usize = 65;
/// Length of a confirmation MAC.
pub const CONFIRM_LEN: usize = 32;

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}

fn point(hex: &str) -> ProjectivePoint {
    let enc = Sec1Point::from_bytes(unhex(hex)).expect("SEC1");
    ProjectivePoint::from(Option::<AffinePoint>::from(AffinePoint::from_sec1_point(&enc)).expect("on the curve"))
}

fn m() -> ProjectivePoint {
    point(M_HEX)
}

fn n() -> ProjectivePoint {
    point(N_HEX)
}

fn encode(p: &ProjectivePoint) -> Vec<u8> {
    p.to_affine().to_sec1_point(false).as_bytes().to_vec()
}

/// A point received from the peer; `None` if it is not a valid, non-identity
/// point of the curve (SEC1 uncompressed only).
pub fn decode(bytes: &[u8]) -> Option<ProjectivePoint> {
    if bytes.len() != POINT_LEN || bytes[0] != 4 {
        return None;
    }
    let enc = Sec1Point::from_bytes(bytes).ok()?;
    let p = Option::<AffinePoint>::from(AffinePoint::from_sec1_point(&enc))?;
    let p = ProjectivePoint::from(p);
    (!bool::from(p.is_identity())).then_some(p)
}

/// 32 big-endian bytes reduced mod n.
fn reduce32(b: &[u8; 32]) -> Scalar {
    // n < 2^256 < 2n, so one conditional subtraction is the reduction.
    match Option::<Scalar>::from(Scalar::from_repr(FieldBytes::from(*b))) {
        Some(s) => s,
        None => {
            // b >= n: b - n.
            let mut low = *b;
            // Subtract n byte-wise (b < 2^256 and b >= n, so no borrow out).
            let n_bytes = N_BYTES;
            let mut borrow = 0i16;
            for i in (0..32).rev() {
                let v = low[i] as i16 - n_bytes[i] as i16 - borrow;
                borrow = (v < 0) as i16;
                low[i] = v.rem_euclid(256) as u8;
            }
            Scalar::from_repr(FieldBytes::from(low)).unwrap()
        }
    }
}

/// The order n of P-256.
const N_BYTES: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xbc, 0xe6, 0xfa,
    0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2, 0xfc, 0x63, 0x25, 0x51,
];
/// 2^256 - n, i.e. 2^256 mod n.
const TWO256_MINUS_N: [u8; 32] = [
    0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x43, 0x19, 0x05,
    0x52, 0x58, 0xe8, 0x61, 0x7b, 0x0c, 0x46, 0x35, 0x3d, 0x03, 0x9c, 0xda, 0xaf,
];

/// A 64-byte big-endian number mod n: `hi * 2^256 + lo`.
fn reduce64(b: &[u8; 64]) -> Scalar {
    let hi: [u8; 32] = b[..32].try_into().unwrap();
    let lo: [u8; 32] = b[32..].try_into().unwrap();
    let r = Scalar::from_repr(FieldBytes::from(TWO256_MINUS_N)).unwrap();
    reduce32(&hi) * r + reduce32(&lo)
}

/// The password scalar for a PIN.
pub fn password_scalar(pin: &str) -> Scalar {
    let mut h = Sha512::new();
    h.update(b"displayswarm-pin-v1");
    h.update(pin.trim().as_bytes());
    let digest: [u8; 64] = h.finalize().into();
    reduce64(&digest)
}

fn random_scalar() -> Scalar {
    loop {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).expect("the OS random source is available");
        if let Some(s) = Option::<Scalar>::from(Scalar::from_repr(FieldBytes::from(b))) {
            if !bool::from(s.is_zero()) {
                return s;
            }
        }
    }
}

fn len8(out: &mut Vec<u8>, part: &[u8]) {
    out.extend_from_slice(&(part.len() as u64).to_le_bytes());
    out.extend_from_slice(part);
}

/// The identities a DisplaySwarm run uses.
pub fn identity_a(device_id: &str) -> Vec<u8> {
    format!("displayswarm-phone:{device_id}").into_bytes()
}

pub fn identity_b(fingerprint: &[u8; 32]) -> Vec<u8> {
    let hex: String = fingerprint.iter().map(|b| format!("{b:02x}")).collect();
    format!("displayswarm-host:{hex}").into_bytes()
}

/// The keys both sides derive from one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keys {
    pub transcript: Vec<u8>,
    pub ke: [u8; 16],
    pub kca: [u8; 16],
    pub kcb: [u8; 16],
}

impl Keys {
    fn mac(key: &[u8], tt: &[u8]) -> [u8; 32] {
        let mut mac = HmacSha256::new_from_slice(key).expect("any key length");
        mac.update(tt);
        mac.finalize().into_bytes().into()
    }

    /// A's confirmation.
    pub fn confirm_a(&self) -> [u8; 32] {
        Self::mac(&self.kca, &self.transcript)
    }

    /// B's confirmation.
    pub fn confirm_b(&self) -> [u8; 32] {
        Self::mac(&self.kcb, &self.transcript)
    }
}

/// HKDF-SHA256 (RFC 5869) with an empty salt, 32 bytes out.
fn hkdf32(ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let prk = Keys::mac(&[0u8; 32], ikm); // HMAC(salt = zeros of hash length, ikm)
    let mut t = HmacSha256::new_from_slice(&prk).expect("any key length");
    t.update(info);
    t.update(&[1]);
    t.finalize().into_bytes().into()
}

/// The key schedule of section 4 for a transcript.
pub fn derive(a: &[u8], b: &[u8], pa: &[u8], pb: &[u8], k: &ProjectivePoint, w: &Scalar, aad: &[u8]) -> Keys {
    let mut tt = Vec::new();
    len8(&mut tt, a);
    len8(&mut tt, b);
    len8(&mut tt, pa);
    len8(&mut tt, pb);
    len8(&mut tt, &encode(k));
    len8(&mut tt, &w.to_repr());
    let hash: [u8; 32] = Sha256::digest(&tt).into();
    let (ke, ka) = (hash[..16].try_into().unwrap(), &hash[16..]);
    let mut info = b"ConfirmationKeys".to_vec();
    info.extend_from_slice(aad);
    let kc = hkdf32(ka, &info);
    Keys { transcript: tt, ke, kca: kc[..16].try_into().unwrap(), kcb: kc[16..].try_into().unwrap() }
}

/// One side of a run, after its own share was made.
pub struct Party {
    secret: Scalar,
    w: Scalar,
    share: Vec<u8>,
    a: Vec<u8>,
    b: Vec<u8>,
}

impl Party {
    fn new(w: Scalar, secret: Scalar, blind: ProjectivePoint, a: Vec<u8>, b: Vec<u8>) -> Self {
        let share = encode(&(blind * w + ProjectivePoint::GENERATOR * secret));
        Self { secret, w, share, a, b }
    }

    /// A (the phone) with password scalar `w`.
    pub fn start_a(w: Scalar, a: Vec<u8>, b: Vec<u8>) -> Self {
        Self::new(w, random_scalar(), m(), a, b)
    }

    /// B (the host) with password scalar `w`.
    pub fn start_b(w: Scalar, a: Vec<u8>, b: Vec<u8>) -> Self {
        Self::new(w, random_scalar(), n(), a, b)
    }

    /// This side's share (pA or pB) to send.
    pub fn share(&self) -> &[u8] {
        &self.share
    }

    /// A's keys from pB; `None` for an invalid point.
    pub fn finish_a(&self, pb: &[u8]) -> Option<Keys> {
        let peer = decode(pb)?;
        let k = (peer - n() * self.w) * self.secret;
        (!bool::from(k.is_identity())).then(|| derive(&self.a, &self.b, &self.share, pb, &k, &self.w, AAD))
    }

    /// B's keys from pA; `None` for an invalid point.
    pub fn finish_b(&self, pa: &[u8]) -> Option<Keys> {
        let peer = decode(pa)?;
        let k = (peer - m() * self.w) * self.secret;
        (!bool::from(k.is_identity())).then(|| derive(&self.a, &self.b, pa, &self.share, &k, &self.w, AAD))
    }
}

/// Constant-time equality for confirmations.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(hex: &str) -> Scalar {
        let b: [u8; 32] = unhex(hex).try_into().unwrap();
        Scalar::from_repr(FieldBytes::from(b)).unwrap()
    }

    fn hexs(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// RFC 9382 Appendix B, first vector (A='server', B='client', no AAD).
    #[test]
    fn rfc9382_vector() {
        let w = scalar("2ee57912099d31560b3a44b1184b9b4866e904c49d12ac5042c97dca461b1a5f");
        let x = scalar("43dd0fd7215bdcb482879fca3220c6a968e66d70b1356cac18bb26c84a78d729");
        let y = scalar("dcb60106f276b02606d8ef0a328c02e4b629f84f89786af5befb0bc75b6e66be");
        let a = Party::new(w, x, m(), b"server".to_vec(), b"client".to_vec());
        let b = Party::new(w, y, n(), b"server".to_vec(), b"client".to_vec());
        assert_eq!(hexs(a.share()), "04a56fa807caaa53a4d28dbb9853b9815c61a411118a6fe516a8798434751470f9010153ac33d0d5f2047ffdb1a3e42c9b4e6be662766e1eeb4116988ede5f912c");
        assert_eq!(hexs(b.share()), "0406557e482bd03097ad0cbaa5df82115460d951e3451962f1eaf4367a420676d09857ccbc522686c83d1852abfa8ed6e4a1155cf8f1543ceca528afb591a1e0b7");
        // The RFC's key schedule uses no AAD; ours appends one, so check it directly.
        let k = (decode(b.share()).unwrap() - n() * w) * x;
        assert_eq!(hexs(&encode(&k)), "0412af7e89717850671913e6b469ace67bd90a4df8ce45c2af19010175e37eed69f75897996d539356e2fa6a406d528501f907e04d97515fbe83db277b715d3325");
        let keys = derive(b"server", b"client", a.share(), b.share(), &k, &w, b"");
        assert_eq!(hexs(&keys.ke), "0e0672dc86f8e45565d338b0540abe69");
        assert_eq!(hexs(&keys.kca), "00c12546835755c86d8c0db7851ae86f");
        assert_eq!(hexs(&keys.kcb), "a9fa3406c3b781b93d804485430ca27a");
        assert_eq!(hexs(&keys.confirm_a()), "58ad4aa88e0b60d5061eb6b5dd93e80d9c4f00d127c65b3b35b1b5281fee38f0");
        assert_eq!(hexs(&keys.confirm_b()), "d3e2e547f1ae04f2dbdbf0fc4b79f8ecff2dff314b5d32fe9fcef2fb26dc459b");
        // And B derives the same K from pA.
        let kb = (decode(a.share()).unwrap() - m() * w) * y;
        assert_eq!(encode(&kb), encode(&k));
    }

    #[test]
    fn the_same_pin_agrees_and_a_different_one_does_not() {
        let (ida, idb) = (identity_a("phone-1"), identity_b(&[7; 32]));
        let a = Party::start_a(password_scalar("123456"), ida.clone(), idb.clone());
        let b = Party::start_b(password_scalar("123456"), ida.clone(), idb.clone());
        let (ka, kb) = (a.finish_a(b.share()).unwrap(), b.finish_b(a.share()).unwrap());
        assert_eq!(ka, kb);
        let wrong = Party::start_b(password_scalar("654321"), ida.clone(), idb.clone());
        let (ka, kw) = (a.finish_a(wrong.share()).unwrap(), wrong.finish_b(a.share()).unwrap());
        assert!(!same(&ka.confirm_a(), &kw.confirm_a()));
        // A host with another certificate cannot confirm either.
        let other = Party::start_b(password_scalar("123456"), ida, identity_b(&[8; 32]));
        assert!(!same(&a.finish_a(other.share()).unwrap().confirm_a(), &other.finish_b(a.share()).unwrap().confirm_a()));
    }

    #[test]
    fn invalid_points_are_refused() {
        assert!(decode(&[4; 65]).is_none(), "not on the curve");
        assert!(decode(&unhex(M_HEX)).is_none(), "compressed points are not accepted");
        assert!(decode(&[0; 1]).is_none());
    }

    #[test]
    fn reduction_mod_n() {
        assert_eq!(reduce32(&N_BYTES), Scalar::ZERO);
        let mut n_plus_1 = N_BYTES;
        n_plus_1[31] += 1;
        assert_eq!(reduce32(&n_plus_1), Scalar::ONE);
        assert_eq!(reduce32(&[0xff; 32]), Scalar::from_repr(FieldBytes::from(TWO256_MINUS_N)).unwrap() - Scalar::ONE);
        let mut wide = [0u8; 64];
        wide[31] = 1; // 2^256
        assert_eq!(reduce64(&wide), Scalar::from_repr(FieldBytes::from(TWO256_MINUS_N)).unwrap());
    }

    /// Shared with `Spake2Test.kt`.
    #[test]
    fn password_scalar_cross_platform_vector() {
        assert_eq!(hexs(&password_scalar("123456").to_repr()), PIN_123456_SCALAR);
    }

    const PIN_123456_SCALAR: &str = "50da975e3f2ea678ac9d6751c48ed13349df4a60c8abfa3c5569f33766eafa6b";
}
