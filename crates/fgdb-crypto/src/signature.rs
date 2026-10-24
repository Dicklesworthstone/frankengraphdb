//! Closed signature-profile registry and untrusted-byte verification dispatch.
//!
//! Profile IDs are scoped to the signature class, not AEAD or KDF IDs. The
//! exact algorithm, widths and acceptance policy also enter the external-audit
//! transcript. This is a registry, not evidence that an audit has happened.

use crate::ed25519::{
    ED25519_PUBLIC_KEY_BYTES, ED25519_SEED_BYTES, ED25519_SIGNATURE_BYTES, Ed25519Error,
    Ed25519VerifyingKey,
};

/// Pure Ed25519 with canonical encodings and strict subgroup admission.
pub const SIGNATURE_PROFILE_ED25519_STRICT: u16 = 1;

/// The exact acceptance policy, bound into the external-audit profile digest.
///
/// A is explicitly checked for prime-subgroup membership and nonidentity.
/// The cofactorless equation then implies R is in that subgroup as well:
/// R = [S]B - [k]A. Identity R is legal; identity A is not. No additional
/// multiplication by eight or permissive verification fallback is permitted.
pub const ED25519_VERIFICATION_POLICY: &str =
    "cofactorless;canonical;S<L;prime-subgroup-A-and-R;nonidentity-A";

/// Closed V1 signature-profile vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureProfile {
    /// RFC 8032 pure Ed25519, not Ed25519ctx or Ed25519ph.
    Ed25519StrictV1,
}

/// The same ordered inventory drives numeric lookup and release-audit scope.
pub const REGISTERED_SIGNATURE_PROFILES: [SignatureProfile; 1] =
    [SignatureProfile::Ed25519StrictV1];

impl SignatureProfile {
    /// Stable ID within the signature class.
    pub const fn id(self) -> u16 {
        match self {
            Self::Ed25519StrictV1 => SIGNATURE_PROFILE_ED25519_STRICT,
        }
    }
    /// Exact algorithm instance; hashing a message first does not select ph.
    pub const fn algorithm_name(self) -> &'static str {
        match self {
            Self::Ed25519StrictV1 => "ed25519-sha512",
        }
    }
    /// Seed width, obtained from the signing implementation's authority.
    pub const fn seed_len(self) -> u16 {
        match self {
            Self::Ed25519StrictV1 => ED25519_SEED_BYTES as u16,
        }
    }
    /// Canonical public-key width.
    pub const fn public_key_len(self) -> u16 {
        match self {
            Self::Ed25519StrictV1 => ED25519_PUBLIC_KEY_BYTES as u16,
        }
    }
    /// Canonical signature width.
    pub const fn signature_len(self) -> u16 {
        match self {
            Self::Ed25519StrictV1 => ED25519_SIGNATURE_BYTES as u16,
        }
    }
    /// Acceptance rules beyond the signature algorithm's name and widths.
    pub const fn verification_policy(self) -> &'static str {
        match self {
            Self::Ed25519StrictV1 => ED25519_VERIFICATION_POLICY,
        }
    }
}

/// Resolve an untrusted ID without silently substituting a default profile.
pub const fn registered_signature_profile(id: u16) -> Option<SignatureProfile> {
    let mut index = 0;
    while index < REGISTERED_SIGNATURE_PROFILES.len() {
        let profile = REGISTERED_SIGNATURE_PROFILES[index];
        if profile.id() == id {
            return Some(profile);
        }
        index += 1;
    }
    None
}

/// Typed refusals for profile-bound verification of untrusted byte slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    /// No registered primitive is selected by this ID.
    UnsupportedProfile { profile_id: u16 },
    /// Public-key bytes do not have the selected profile's exact width.
    PublicKeyLength {
        profile_id: u16,
        expected: u16,
        actual: usize,
    },
    /// Signature bytes do not have the selected profile's exact width.
    SignatureLength {
        profile_id: u16,
        expected: u16,
        actual: usize,
    },
    /// The selected primitive refused the key or signature.
    Primitive(Ed25519Error),
}

impl core::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedProfile { profile_id } => {
                write!(f, "unsupported signature profile {profile_id}")
            }
            Self::PublicKeyLength {
                profile_id,
                expected,
                actual,
            } => write!(
                f,
                "signature profile {profile_id} public key has {actual} bytes; expected {expected}"
            ),
            Self::SignatureLength {
                profile_id,
                expected,
                actual,
            } => write!(
                f,
                "signature profile {profile_id} signature has {actual} bytes; expected {expected}"
            ),
            Self::Primitive(error) => core::fmt::Display::fmt(error, f),
        }
    }
}
impl core::error::Error for SignatureError {}

/// Verify exact bytes under an explicitly selected, registered profile.
///
/// No truncation, prehashing, implicit application domain or fallback occurs.
/// Callers own domain separation of their canonical checkpoint transcripts.
///
/// ```
/// use fgdb_crypto::{Ed25519SigningKey, SIGNATURE_PROFILE_ED25519_STRICT, verify_signature};
/// use fgdb_crypto::zeroize::Secret;
/// let key = Ed25519SigningKey::from_seed(Secret::new([7; 32]));
/// let message = b"example:checkpoint:v1\0canonical checkpoint bytes";
/// let signature = key.sign(message);
/// verify_signature(SIGNATURE_PROFILE_ED25519_STRICT,
///     key.verifying_key().as_bytes(), message, &signature).unwrap();
/// ```
pub fn verify_signature(
    profile_id: u16,
    public_key: &[u8],
    message: &[u8],
    signature: &[u8],
) -> Result<(), SignatureError> {
    let profile = registered_signature_profile(profile_id)
        .ok_or(SignatureError::UnsupportedProfile { profile_id })?;
    match profile {
        SignatureProfile::Ed25519StrictV1 => {
            let public_key: [u8; 32] =
                public_key
                    .try_into()
                    .map_err(|_| SignatureError::PublicKeyLength {
                        profile_id,
                        expected: profile.public_key_len(),
                        actual: public_key.len(),
                    })?;
            let signature: &[u8; 64] =
                signature
                    .try_into()
                    .map_err(|_| SignatureError::SignatureLength {
                        profile_id,
                        expected: profile.signature_len(),
                        actual: signature.len(),
                    })?;
            Ed25519VerifyingKey::from_bytes(public_key)
                .and_then(|key| key.verify(message, signature))
                .map_err(SignatureError::Primitive)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex<const N: usize>(text: &str) -> [u8; N] {
        assert_eq!(text.len(), N * 2);
        let mut bytes = [0; N];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap();
        }
        bytes
    }
    const PUBLIC: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";
    const SIGNATURE: &str = "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b";

    #[test]
    fn numeric_lookup_matches_the_complete_inventory() {
        let resolved: Vec<_> = (0..=u16::MAX)
            .filter_map(|id| registered_signature_profile(id).map(|p| (id, p)))
            .collect();
        let inventoried: Vec<_> = REGISTERED_SIGNATURE_PROFILES
            .iter()
            .copied()
            .map(|p| (p.id(), p))
            .collect();
        assert_eq!(resolved, inventoried);
        assert!(inventoried.windows(2).all(|rows| rows[0].0 < rows[1].0));
        let profile = registered_signature_profile(1).unwrap();
        assert_eq!(profile.algorithm_name(), "ed25519-sha512");
        assert_eq!(profile.seed_len(), 32);
        assert_eq!(profile.public_key_len(), 32);
        assert_eq!(profile.signature_len(), 64);
        assert_eq!(profile.verification_policy(), ED25519_VERIFICATION_POLICY);
    }

    #[test]
    fn unknown_profiles_and_nonexact_widths_never_fall_back() {
        let public = unhex::<32>(PUBLIC);
        let signature = unhex::<64>(SIGNATURE);
        for profile_id in [0, 2, u16::MAX] {
            assert_eq!(
                verify_signature(profile_id, &public, b"", &signature),
                Err(SignatureError::UnsupportedProfile { profile_id })
            );
        }
        for actual in [0, 1, 31, 33, 64] {
            assert_eq!(
                verify_signature(1, &vec![0; actual], b"", &signature),
                Err(SignatureError::PublicKeyLength {
                    profile_id: 1,
                    expected: 32,
                    actual
                })
            );
        }
        for actual in [0, 1, 32, 63, 65, 128] {
            assert_eq!(
                verify_signature(1, &public, b"", &vec![0; actual]),
                Err(SignatureError::SignatureLength {
                    profile_id: 1,
                    expected: 64,
                    actual
                })
            );
        }
    }

    #[test]
    fn registered_dispatch_authenticates_the_rfc_vector_and_message() {
        let public = unhex::<32>(PUBLIC);
        let signature = unhex::<64>(SIGNATURE);
        assert_eq!(verify_signature(1, &public, b"", &signature), Ok(()));
        assert_eq!(
            verify_signature(1, &public, b"wrong", &signature),
            Err(SignatureError::Primitive(Ed25519Error::InvalidSignature))
        );
        let mut identity = [0; 32];
        identity[0] = 1;
        let mut forgery = [0; 64];
        forgery[..32].copy_from_slice(&unhex::<32>(
            "5866666666666666666666666666666666666666666666666666666666666666",
        ));
        forgery[32] = 1; // [S]B=R for any message if identity A were permitted.
        assert_eq!(
            verify_signature(1, &identity, b"any signer", &forgery),
            Err(SignatureError::Primitive(Ed25519Error::InvalidPublicKey))
        );
    }
}
