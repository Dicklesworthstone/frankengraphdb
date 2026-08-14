//! fgdb-crypto — the in-house cryptographic kernel (bead fgdb-w1-crypto-y5o).
//!
//! Closed dependency universe (doctrine #1): no external crypto crates. This
//! crate owns BLAKE3 (plain/keyed/derive-key/XOF) and the domain-separated
//! identity transcripts of plan §5.1 that Chronicle (W2) consumes. AEAD, KDF
//! profiles, and `CryptoCx` entropy discipline land as later increments of the
//! same bead; each primitive arrives with oracle-verified vectors before
//! anything consumes it.
//!
//! §5.1 identity law (plan L278): `ObjectId = BLAKE3_keyed(K_oid,
//! "fgdb:logical:v1" ‖ DatabaseSecurityNamespaceId ‖ canonical_plaintext_header
//! ‖ canonical_plaintext_payload)`. The transcript helpers here own the exact
//! domain strings and concatenation order so W2 cannot drift them per call
//! site.
#![forbid(unsafe_code)]

pub mod aead;
pub mod argon2id;
pub mod blake2b;
pub mod blake3;
pub mod chacha20;
pub mod cx;
pub mod poly1305;
pub mod zeroize;

pub use aead::{
    AeadError, DATA_CRYPTO_PROFILE_XCHACHA20_POLY1305, ObjectAeadProfile, object_aead_aad,
    registered_object_aead_profile, xchacha20poly1305_open, xchacha20poly1305_seal,
};
pub use argon2id::{
    PASSPHRASE_KDF_PROFILE_ARGON2ID_RFC9106_SECOND, PASSPHRASE_KDF_SALT_BYTES,
    PASSPHRASE_KEK_BYTES, PassphraseKdfError, PassphraseKdfProfile, PassphraseKdfProfileSpec,
    derive_passphrase_kek, registered_passphrase_kdf_profile,
};
pub use blake2b::{Blake2b, Blake2bError, blake2b, blake2b_keyed};
pub use blake3::{Digest, Hasher, derive_key, hash, keyed_hash};
pub use cx::{
    CryptoCx, DeterministicEntropy, EntropyError, EntropySource, FreshObjectProtectionMaterial,
    ObjectProtectionMaterialRef, SystemEntropy,
};

/// The §5.1 logical-identity domain string (plan L278).
pub const LOGICAL_IDENTITY_DOMAIN: &[u8] = b"fgdb:logical:v1";

/// The §5.1 encoding-identity domain string (plan L280).
pub const ENCODING_IDENTITY_DOMAIN: &[u8] = b"fgdb:encoding:v1";

/// The §5.1 placement-identity domain string (plan L280).
pub const PLACEMENT_IDENTITY_DOMAIN: &[u8] = b"fgdb:placement:v1";

/// The §5.1 symbol-auth KDF domain string (plan L280).
pub const SYMBOL_AUTH_DOMAIN: &str = "fgdb:symbol-auth:v1";

/// `ObjectId = BLAKE3_keyed(K_oid, "fgdb:logical:v1" ‖ namespace ‖ header ‖ payload)`.
///
/// Full 256-bit identity; a 128-bit prefix is only a lookup accelerator and
/// collision buckets always verify the full digest (plan L278). The caller
/// supplies canonical bytes — canonicalization is the codec layer's law, not
/// this function's.
pub fn logical_object_id(
    k_oid: &[u8; 32],
    namespace_id: &[u8; 32],
    canonical_header: &[u8],
    canonical_payload: &[u8],
) -> Digest {
    let mut hasher = Hasher::new_keyed(k_oid);
    hasher.update(LOGICAL_IDENTITY_DOMAIN);
    hasher.update(namespace_id);
    hasher.update(canonical_header);
    hasher.update(canonical_payload);
    hasher.finalize()
}

/// `EncodingId = BLAKE3("fgdb:encoding:v1" ‖ canonical(EncodingDescriptorWithoutId))`
/// (plan L280). Unkeyed by the source: encoding identity is not a secret.
pub fn encoding_id(canonical_descriptor: &[u8]) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(ENCODING_IDENTITY_DOMAIN);
    hasher.update(canonical_descriptor);
    hasher.finalize()
}

/// `PlacementId = BLAKE3("fgdb:placement:v1" ‖ canonical(PlacementDescriptorWithoutId))`
/// (plan L280). Neither the ID nor a digest of the record is an input to itself.
pub fn placement_id(canonical_descriptor: &[u8]) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(PLACEMENT_IDENTITY_DOMAIN);
    hasher.update(canonical_descriptor);
    hasher.finalize()
}

/// `K_symbol = KDF(DEK, "fgdb:symbol-auth:v1" ‖ EncodingId)` (plan L280):
/// the per-encoding symbol-authentication key, domain-separated from every
/// other use of the DEK.
pub fn symbol_auth_key(dek: &[u8; 32], encoding_id: &Digest) -> [u8; 32] {
    // derive_key's context string must be static and globally unique; the
    // encoding identity rides in the key-material position.
    let mut material = [0u8; 64];
    material[..32].copy_from_slice(dek);
    material[32..].copy_from_slice(&encoding_id.0);
    derive_key(SYMBOL_AUTH_DOMAIN, &material)
}
