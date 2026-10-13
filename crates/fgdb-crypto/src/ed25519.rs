//! Pure Ed25519 signatures (RFC 8032 sections 5.1 and 7.1).
//!
//! This profile uses canonical A/R encodings, S<L and the COFACTORLESS
//! equation [S]B = R + [H(R||A||M)]A. Imported public keys must additionally
//! be nonidentity points in the prime-order subgroup. This intentionally
//! refuses torsion/mixed-order keys that looser verification profiles admit.
//! No Ed25519ctx/ph, batch verification, randomness source or transcript
//! domain separation is implied; applications must supply their own exact
//! domain-separated messages and obtain seeds through their entropy authority.
//!
//! Signing uses fixed-bound scalar/field loops and mask selection without
//! secret-dependent branches or table indexes. That source property is NOT a
//! pinned-codegen constant-time proof or an independent cryptographic audit.
//! Release use remains subject to the crate's external-audit gate. Secret
//! seed expansion, nonce hashes/scalars and owned field intermediates use the
//! existing best-effort scrub boundary; compiler-created copies are not covered.

mod field;
mod scalar;
#[cfg(test)]
mod tests;

use crate::sha512::Sha512;
use crate::zeroize::Secret;
use field::Field;

/// Widths are the pure Ed25519 wire representation, not a new record encoding.
pub const ED25519_SEED_BYTES: usize = 32;
pub const ED25519_PUBLIC_KEY_BYTES: usize = 32;
pub const ED25519_SIGNATURE_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ed25519Error {
    InvalidPublicKey,
    InvalidSignature,
}
impl core::fmt::Display for Ed25519Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::InvalidPublicKey => "invalid Ed25519 public key for the strict subgroup profile",
            Self::InvalidSignature => "invalid Ed25519 signature",
        })
    }
}
impl core::error::Error for Ed25519Error {}

/// Seed-expanded secret scalar/prefix, owned once and never Clone or Copy.
pub struct Ed25519SigningKey {
    expanded: Secret<64>,
    verifying_key: Ed25519VerifyingKey,
}
impl core::fmt::Debug for Ed25519SigningKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Ed25519SigningKey([REDACTED])")
    }
}

/// Canonical nonidentity prime-subgroup public key. No secret fields.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ed25519VerifyingKey {
    bytes: [u8; 32],
}
impl core::fmt::Debug for Ed25519VerifyingKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Ed25519VerifyingKey([REDACTED])")
    }
}

impl Ed25519SigningKey {
    /// Consume a seed; do not retain it alongside its expanded representation.
    pub fn from_seed(seed: Secret<32>) -> Self {
        let mut expanded = Secret::zeroed();
        let mut hash = Sha512::new();
        hash.update(seed.expose());
        hash.finalize_into(expanded.expose_mut());
        expanded.expose_mut()[0] &= 248;
        expanded.expose_mut()[31] &= 63;
        expanded.expose_mut()[31] |= 64;
        let scalar = expanded.expose()[..32]
            .try_into()
            .expect("32-byte secret scalar");
        let bytes = Point::base().multiply(scalar).encode();
        Self {
            expanded,
            verifying_key: Ed25519VerifyingKey { bytes },
        }
    }

    pub fn verifying_key(&self) -> Ed25519VerifyingKey {
        self.verifying_key
    }

    /// Deterministic pure Ed25519. SHA-512 sees prefix||message and R||A||message
    /// as streaming transcripts, without allocating/copying the full message.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        let mut hash = Sha512::new();
        hash.update(&self.expanded.expose()[32..]);
        hash.update(message);
        let mut digest = Secret::zeroed();
        hash.finalize_into(digest.expose_mut());
        let nonce = scalar::reduce(digest.expose());
        let r = Point::base().multiply(nonce.expose()).encode();
        let challenge = challenge(&r, &self.verifying_key.bytes, message);
        let secret = self.expanded.expose()[..32]
            .try_into()
            .expect("32-byte secret scalar");
        let s = scalar::mul_add(challenge.expose(), secret, nonce.expose());
        let mut signature = [0; 64];
        signature[..32].copy_from_slice(&r);
        signature[32..].copy_from_slice(s.expose());
        signature
    }
}

impl Ed25519VerifyingKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, Ed25519Error> {
        let point = Point::decode(&bytes).ok_or(Ed25519Error::InvalidPublicKey)?;
        if point.is_identity() || !point.multiply(&scalar::ORDER).is_identity() {
            return Err(Ed25519Error::InvalidPublicKey);
        }
        Ok(Self { bytes })
    }
    pub fn to_bytes(self) -> [u8; 32] {
        self.bytes
    }
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    pub fn verify(&self, message: &[u8], signature: &[u8; 64]) -> Result<(), Ed25519Error> {
        let r_bytes = signature[..32].try_into().expect("32-byte R");
        let s = signature[32..].try_into().expect("32-byte S");
        if !scalar::canonical(s) {
            return Err(Ed25519Error::InvalidSignature);
        }
        let r = Point::decode(r_bytes).ok_or(Ed25519Error::InvalidSignature)?;
        // Constructor validation is retained by the private immutable bytes.
        let a = Point::decode(&self.bytes).ok_or(Ed25519Error::InvalidPublicKey)?;
        let k = challenge(r_bytes, &self.bytes, message);
        let expected = r.add(&a.multiply(k.expose()));
        if Point::base().multiply(s).equals(&expected) {
            Ok(())
        } else {
            Err(Ed25519Error::InvalidSignature)
        }
    }
}

fn challenge(r: &[u8; 32], a: &[u8; 32], message: &[u8]) -> Secret<32> {
    let mut hash = Sha512::new();
    hash.update(r);
    hash.update(a);
    hash.update(message);
    let mut digest = Secret::zeroed();
    hash.finalize_into(digest.expose_mut());
    scalar::reduce(digest.expose())
}

struct Point {
    x: Field,
    y: Field,
    z: Field,
    t: Field,
}
impl Point {
    fn identity() -> Self {
        Self {
            x: Field::ZERO,
            y: Field::ONE,
            z: Field::ONE,
            t: Field::ZERO,
        }
    }
    fn base() -> Self {
        let x = Field([
            54554, 36645, 11616, 51542, 42930, 38181, 51040, 26924, 56412, 64982, 57905, 49316,
            21502, 52590, 14035, 8553,
        ]);
        let y = Field([
            26200, 26214, 26214, 26214, 26214, 26214, 26214, 26214, 26214, 26214, 26214, 26214,
            26214, 26214, 26214, 26214,
        ]);
        let t = x.mul(&y);
        Self {
            x,
            y,
            z: Field::ONE,
            t,
        }
    }
    /// Complete extended twisted-Edwards addition (a=-1); all operands are
    /// private valid points. The identity and doubling need no exceptional arm.
    fn add(&self, rhs: &Self) -> Self {
        let a = self.y.sub(&self.x).mul(&rhs.y.sub(&rhs.x));
        let b = self.y.add(&self.x).mul(&rhs.y.add(&rhs.x));
        let c = self.t.mul(&rhs.t).mul(&Field::D.add(&Field::D));
        let d = self.z.mul(&rhs.z).add(&self.z.mul(&rhs.z));
        let e = b.sub(&a);
        let f = d.sub(&c);
        let g = d.add(&c);
        let h = b.add(&a);
        Self {
            x: e.mul(&f),
            y: g.mul(&h),
            z: f.mul(&g),
            t: e.mul(&h),
        }
    }
    fn select(a: &Self, b: &Self, bit: u8) -> Self {
        Self {
            x: Field::select(&a.x, &b.x, bit),
            y: Field::select(&a.y, &b.y, bit),
            z: Field::select(&a.z, &b.z, bit),
            t: Field::select(&a.t, &b.t, bit),
        }
    }
    fn multiply(&self, scalar: &[u8; 32]) -> Self {
        let mut point = Self::identity();
        for i in (0..256).rev() {
            let doubled = point.add(&point);
            let added = doubled.add(self);
            point = Self::select(&doubled, &added, (scalar[i / 8] >> (i % 8)) & 1);
        }
        point
    }
    fn equals(&self, rhs: &Self) -> bool {
        self.x.mul(&rhs.z).equals(&rhs.x.mul(&self.z))
            & self.y.mul(&rhs.z).equals(&rhs.y.mul(&self.z))
    }
    fn is_identity(&self) -> bool {
        self.x.is_zero() & self.y.equals(&self.z)
    }
    fn encode(&self) -> [u8; 32] {
        let inverse = self.z.invert();
        let x = self.x.mul(&inverse);
        let mut bytes = self.y.mul(&inverse).to_bytes();
        bytes[31] |= x.parity() << 7;
        bytes
    }
    fn decode(bytes: &[u8; 32]) -> Option<Self> {
        let sign = bytes[31] >> 7;
        let mut y_bytes = *bytes;
        y_bytes[31] &= 0x7f;
        let y = Field::from_bytes(&y_bytes)?;
        let y2 = y.square();
        let u = y2.sub(&Field::ONE);
        let v = Field::D.mul(&y2).add(&Field::ONE);
        if v.is_zero() {
            return None;
        }
        let ratio = u.mul(&v.invert());
        let mut x = ratio.sqrt_candidate();
        if !x.square().equals(&ratio) {
            x = x.mul(&Field::SQRT_M1);
        }
        if !v.mul(&x.square()).equals(&u) || (x.is_zero() && sign != 0) {
            return None;
        }
        x = Field::select(&x, &x.negate(), x.parity() ^ sign);
        let t = x.mul(&y);
        Some(Self {
            x,
            y,
            z: Field::ONE,
            t,
        })
    }
}
