//! The §5.1 noncircular object-identity pipeline.
//!
//! Four identity layers, each answering exactly one question, so that
//! branches, dedup, replication, backup, recoding, and KMS rewrap each change
//! exactly one of them (plan L278-L280):
//!
//! | layer          | question                | changes when                       |
//! |----------------|-------------------------|------------------------------------|
//! | `ObjectId`     | *what an object is*     | the canonical plaintext changes    |
//! | `CiphertextId` | *how it is protected*   | re-encryption / new DEK            |
//! | `EncodingId`   | *how it is coded*       | recoding (no re-encryption needed) |
//! | `PlacementId`  | *where it lives*        | symbols added or moved             |
//!
//! THE NONCIRCULARITY LAW, enforced by construction rather than by review:
//! each stage is a distinct type that can only be built from the previous
//! stage's value, and no stage's transcript contains its own identity or a
//! digest of its own record. You cannot compute a `PlacementId` without an
//! `EncodingId`, cannot get an `EncodingId` without a `CiphertextId`, and
//! cannot get a `CiphertextId` without the `ObjectId` that went into the AEAD
//! AAD. A caller holding the wrong stage does not compile.

use fgdb_crypto::{Digest, ObjectAeadProfile, aead};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};

/// Stable prefix width carried by crypto verification diagnostics.
///
/// The complete `EncodingId` remains in the durable identity graph; the log
/// needs only enough public identity to correlate one rejection without
/// turning routine verification telemetry into a second authoritative index.
pub const VERIFICATION_ENCODING_PREFIX_BYTES: usize = 8;

/// Which cryptographic or identity boundary emitted a verification event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationOperation {
    ObjectOpen,
    EncodingReconstruction,
    PlacementIdentity,
    SymbolRecord,
    RecoveredObjectOpen,
    ObjectRecovery,
}

/// Stable, typed rejection classes for post-implementation verification.
///
/// These classes deliberately contain no primitive detail, key material,
/// nonce, plaintext, or ciphertext bytes. A log consumer can correlate and
/// count failures without becoming a padding oracle or a secret-bearing
/// incident artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationFailureClass {
    UnsupportedDataCryptoProfile,
    ObjectTagLength,
    EncodingIdentity,
    PlacementIdentity,
    Authentication,
    CiphertextIdentity,
    SymbolTruncated,
    SymbolUnsupportedFraming,
    SymbolInconsistentLengths,
    ForeignEncoding,
    InvalidParameters,
    InsufficientSymbols,
    DecodeFailed,
    LogicalIdentity,
}

/// Accepted or rejected is explicit; a rejection always carries its typed
/// class rather than relying on a free-form error string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationOutcome {
    Accepted,
    Rejected(VerificationFailureClass),
}

/// Secret-free record emitted by every public Chronicle crypto-verification
/// path.
///
/// `plaintext_len` is the canonical logical length declared by the admitted
/// cipher descriptor. `ciphertext_len` is the actual protected input length
/// when bytes are present and the checked declared length otherwise. The
/// encoding prefix is absent only before an encoding exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CryptoVerificationEvent {
    pub profile_id: u16,
    pub object_kind: u16,
    pub plaintext_len: u64,
    pub ciphertext_len: Option<u64>,
    pub encoding_id_prefix: Option<[u8; VERIFICATION_ENCODING_PREFIX_BYTES]>,
    pub operation: VerificationOperation,
    pub outcome: VerificationOutcome,
}

impl CryptoVerificationEvent {
    #[must_use]
    pub const fn failure_class(self) -> Option<VerificationFailureClass> {
        match self.outcome {
            VerificationOutcome::Accepted => None,
            VerificationOutcome::Rejected(class) => Some(class),
        }
    }
}

/// Capability supplied by the caller of every verification path.
///
/// There is deliberately no no-op implementation. A production caller must
/// choose where these bounded, secret-free records go, while tests can use a
/// `Vec<CryptoVerificationEvent>` as an exact in-memory witness.
pub trait CryptoVerificationSink {
    fn record(&mut self, event: CryptoVerificationEvent);
}

impl CryptoVerificationSink for Vec<CryptoVerificationEvent> {
    fn record(&mut self, event: CryptoVerificationEvent) {
        self.push(event);
    }
}

fn encoding_prefix(
    encoding_id: Option<Digest>,
) -> Option<[u8; VERIFICATION_ENCODING_PREFIX_BYTES]> {
    encoding_id.map(|encoding_id| {
        let mut prefix = [0u8; VERIFICATION_ENCODING_PREFIX_BYTES];
        prefix.copy_from_slice(&encoding_id.0[..VERIFICATION_ENCODING_PREFIX_BYTES]);
        prefix
    })
}

fn declared_ciphertext_len(descriptor: &CipherDescriptor) -> Option<u64> {
    descriptor
        .compressed_len
        .checked_add(u64::from(descriptor.object_tag_len))
}

pub(crate) fn verification_event(
    descriptor: &CipherDescriptor,
    encoding_id: Option<Digest>,
    actual_ciphertext_len: Option<usize>,
    operation: VerificationOperation,
    outcome: VerificationOutcome,
) -> CryptoVerificationEvent {
    CryptoVerificationEvent {
        profile_id: descriptor.data_crypto_profile,
        object_kind: descriptor.object_kind,
        plaintext_len: descriptor.canonical_plaintext_len,
        ciphertext_len: actual_ciphertext_len
            .and_then(|len| u64::try_from(len).ok())
            .or_else(|| declared_ciphertext_len(descriptor)),
        encoding_id_prefix: encoding_prefix(encoding_id),
        operation,
        outcome,
    }
}

/// Canonical-plaintext facts every identity layer repeats. These are the
/// fields of `CipherDescriptorWithoutDigest` that the plan fixes as inputs to
/// the AEAD AAD; FEC and placement are deliberately absent, which is what lets
/// one ciphertext be recoded without re-encryption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CipherDescriptor {
    pub object_kind: u16,
    pub canonical_plaintext_len: u64,
    pub codec_profile: u16,
    pub compressed_len: u64,
    pub data_crypto_profile: u16,
    pub dek_id: [u8; 16],
    pub object_nonce: [u8; 24],
    pub object_tag_len: u16,
}

impl CipherDescriptor {
    /// Resolve and validate the durable data-crypto profile before any
    /// cryptographic operation, allocation derived from ciphertext shape, or
    /// tag slicing. Profile IDs are a closed registry, not AAD decoration.
    pub fn registered_aead_profile(&self) -> Result<ObjectAeadProfile, IdentityMismatch> {
        let profile = fgdb_crypto::registered_object_aead_profile(self.data_crypto_profile).ok_or(
            IdentityMismatch::UnsupportedDataCryptoProfile {
                data_crypto_profile: self.data_crypto_profile,
            },
        )?;
        // ubs:ignore -- public durable profile widths, not secret or authentication material.
        if self.object_tag_len != profile.tag_len() {
            return Err(IdentityMismatch::ObjectTagLength {
                data_crypto_profile: self.data_crypto_profile,
                expected: profile.tag_len(),
                actual: self.object_tag_len,
            });
        }
        Ok(profile)
    }

    /// Canonical bytes: fixed-width little-endian fields in declaration order.
    /// The logical OID is bound separately by the AAD transcript, so it is
    /// deliberately not repeated here.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 + 8 + 2 + 8 + 2 + 16 + 24 + 2);
        out.extend_from_slice(&self.object_kind.to_le_bytes());
        out.extend_from_slice(&self.canonical_plaintext_len.to_le_bytes());
        out.extend_from_slice(&self.codec_profile.to_le_bytes());
        out.extend_from_slice(&self.compressed_len.to_le_bytes());
        out.extend_from_slice(&self.data_crypto_profile.to_le_bytes());
        out.extend_from_slice(&self.dek_id);
        out.extend_from_slice(&self.object_nonce);
        out.extend_from_slice(&self.object_tag_len.to_le_bytes());
        out
    }
}

/// Stage 1 — *what an object is*. Holds the keyed identity plus the exact
/// canonical bytes it was computed over, so a collision bucket can perform the
/// full verification the plan requires (digest, kind, length, plaintext)
/// rather than trusting a 128-bit prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifiedObject {
    object_id: ObjectId,
    namespace: DatabaseSecurityNamespaceId,
    object_kind: u16,
    canonical_plaintext: Vec<u8>,
}

impl IdentifiedObject {
    /// `ObjectId = BLAKE3_keyed(K_oid, "fgdb:logical:v1" ‖ namespace ‖ header
    /// ‖ payload)` (plan L278). The header is the canonical plaintext header;
    /// callers pass canonical bytes — canonicalization is the codec's law.
    pub fn new(
        k_oid: &[u8; 32],
        namespace: DatabaseSecurityNamespaceId,
        object_kind: u16,
        canonical_header: &[u8],
        canonical_payload: &[u8],
    ) -> Self {
        // `object_kind` is a canonical-logical-header field, not merely a
        // collision-bucket discriminator. Hash it before the caller's header
        // so identical payloads from two schema kinds cannot share an
        // `ObjectId` in the first place.
        let mut identity_header = Vec::with_capacity(2 + canonical_header.len());
        identity_header.extend_from_slice(&object_kind.to_le_bytes());
        identity_header.extend_from_slice(canonical_header);
        let digest = fgdb_crypto::logical_object_id(
            k_oid,
            &namespace.0,
            &identity_header,
            canonical_payload,
        );
        let mut canonical_plaintext =
            Vec::with_capacity(2 + canonical_header.len() + canonical_payload.len());
        canonical_plaintext.extend_from_slice(&object_kind.to_le_bytes());
        canonical_plaintext.extend_from_slice(canonical_header);
        canonical_plaintext.extend_from_slice(canonical_payload);
        Self {
            object_id: ObjectId(digest.0),
            namespace,
            object_kind,
            canonical_plaintext,
        }
    }

    pub fn object_id(&self) -> ObjectId {
        self.object_id
    }

    pub fn namespace(&self) -> DatabaseSecurityNamespaceId {
        self.namespace
    }

    pub fn object_kind(&self) -> u16 {
        self.object_kind
    }

    pub fn canonical_plaintext(&self) -> &[u8] {
        &self.canonical_plaintext
    }

    /// The 128-bit lookup accelerator. It is ONLY an accelerator: a bucket hit
    /// must still call [`IdentifiedObject::verifies_as_same_object`].
    pub fn lookup_prefix(&self) -> [u8; 16] {
        let mut prefix = [0u8; 16];
        prefix.copy_from_slice(&self.object_id.0[..16]);
        prefix
    }

    /// Full collision-bucket verification (plan L278): full digest, object
    /// kind, length, and canonical plaintext — every one, before any
    /// deduplication or substitution.
    pub fn verifies_as_same_object(&self, candidate: &Self) -> bool {
        self.object_id == candidate.object_id
            && self.object_kind == candidate.object_kind
            && self.canonical_plaintext.len() == candidate.canonical_plaintext.len()
            && self.canonical_plaintext == candidate.canonical_plaintext
    }

    /// Deduplication does not cross security namespaces (plan L278). A shared
    /// namespace/key domain is a separately specified deployment policy, so it
    /// is not expressible here by accident.
    pub fn may_deduplicate_against(&self, candidate: &Self) -> bool {
        self.namespace == candidate.namespace && self.verifies_as_same_object(candidate)
    }

    /// Stage 1 → 2. Performs the ONE object-level AEAD with the §5.1 AAD and
    /// returns the protected object. `protected_bytes` is the deterministically
    /// compressed canonical plaintext (the codec runs before encryption).
    pub fn protect(
        self,
        dek: &[u8; 32],
        descriptor: CipherDescriptor,
        compressed_plaintext: &[u8],
    ) -> Result<ProtectedObject, IdentityMismatch> {
        let profile = descriptor.registered_aead_profile()?;
        let aad = aead::object_aead_aad(&Digest(self.object_id.0), &descriptor.canonical_bytes());
        let sealed = match profile {
            ObjectAeadProfile::XChaCha20Poly1305V1 => aead::xchacha20poly1305_seal(
                dek,
                &descriptor.object_nonce,
                &aad,
                compressed_plaintext,
            ),
        };
        let tag_start = sealed.len() - usize::from(descriptor.object_tag_len);
        let ciphertext_id =
            ciphertext_identity(&descriptor, &sealed[..tag_start], &sealed[tag_start..]);
        Ok(ProtectedObject {
            object_id: self.object_id,
            descriptor,
            sealed,
            ciphertext_id,
        })
    }
}

/// `CiphertextId` hashes descriptor + ciphertext + profile-sized object tag
/// (plan L280). Domain-separated from every other identity transcript.
fn ciphertext_identity(
    descriptor: &CipherDescriptor,
    ciphertext: &[u8],
    object_tag: &[u8],
) -> Digest {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(CIPHERTEXT_IDENTITY_DOMAIN);
    hasher.update(&descriptor.canonical_bytes());
    hasher.update(ciphertext);
    hasher.update(object_tag);
    hasher.finalize()
}

/// The ciphertext-identity domain string.
pub const CIPHERTEXT_IDENTITY_DOMAIN: &[u8] = b"fgdb:ciphertext:v1";

/// Stage 2 — *how it is protected*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedObject {
    object_id: ObjectId,
    descriptor: CipherDescriptor,
    /// Ciphertext followed by the AEAD tag, exactly as RaptorQ will encode it.
    sealed: Vec<u8>,
    ciphertext_id: Digest,
}

impl ProtectedObject {
    pub fn object_id(&self) -> ObjectId {
        self.object_id
    }

    pub fn ciphertext_id(&self) -> Digest {
        self.ciphertext_id
    }

    pub fn descriptor(&self) -> &CipherDescriptor {
        &self.descriptor
    }

    /// The complete ciphertext-plus-object-tag RaptorQ encodes (plan L280).
    pub fn protected_bytes(&self) -> &[u8] {
        &self.sealed
    }

    /// Recover the compressed plaintext. Fails closed on any tampering of the
    /// ciphertext, tag, descriptor, DEK, or bound object identity.
    pub fn open(
        &self,
        dek: &[u8; 32],
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<Vec<u8>, aead::AeadError> {
        let aad = aead::object_aead_aad(
            &Digest(self.object_id.0),
            &self.descriptor.canonical_bytes(),
        );
        let result = aead::xchacha20poly1305_open(
            dek,
            &self.descriptor.object_nonce,
            &aad,
            &self.sealed,
        );
        verification.record(verification_event(
            &self.descriptor,
            None,
            Some(self.sealed.len()),
            VerificationOperation::ObjectOpen,
            if result.is_ok() {
                VerificationOutcome::Accepted
            } else {
                VerificationOutcome::Rejected(VerificationFailureClass::Authentication)
            },
        ));
        result
    }

    /// Stage 2 → 3. The same authenticated ciphertext may be encoded — and
    /// later RE-encoded — under another complete encoding descriptor without
    /// re-encryption, which is why this consumes `&self` rather than `self`.
    pub fn encode(&self, descriptor: EncodingDescriptor) -> EncodedObject {
        let encoding_id = fgdb_crypto::encoding_id(&descriptor.canonical_bytes(self.ciphertext_id));
        EncodedObject {
            object_id: self.object_id,
            ciphertext_id: self.ciphertext_id,
            cipher_descriptor: self.descriptor.clone(),
            descriptor,
            encoding_id,
        }
    }
}

/// Every parameter that changes RaptorQ decoding or symbol authentication
/// (plan L280). `ciphertext_id` is supplied by the stage transition, not by
/// the caller, so an encoding cannot name a ciphertext it did not come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodingDescriptor {
    pub fec_profile: u16,
    pub transfer_length: u64,
    pub oti_common: u64,
    pub oti_scheme: u32,
    pub symbol_size: u16,
    pub source_block_count: u16,
    pub symbol_auth_profile: u16,
}

impl EncodingDescriptor {
    pub(crate) fn canonical_bytes(&self, ciphertext_id: Digest) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + 2 + 8 + 8 + 4 + 2 + 2 + 2);
        out.extend_from_slice(&ciphertext_id.0);
        out.extend_from_slice(&self.fec_profile.to_le_bytes());
        out.extend_from_slice(&self.transfer_length.to_le_bytes());
        out.extend_from_slice(&self.oti_common.to_le_bytes());
        out.extend_from_slice(&self.oti_scheme.to_le_bytes());
        out.extend_from_slice(&self.symbol_size.to_le_bytes());
        out.extend_from_slice(&self.source_block_count.to_le_bytes());
        out.extend_from_slice(&self.symbol_auth_profile.to_le_bytes());
        out
    }
}

/// Why a durable descriptor set could not be admitted.
///
/// Deliberately separate from an AEAD failure: this fires before any bytes are
/// opened, and it means the descriptor set itself is unsupported or does not
/// recompute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMismatch {
    /// The durable cipher descriptor names no registered data-crypto profile.
    UnsupportedDataCryptoProfile { data_crypto_profile: u16 },
    /// The declared object tag length disagrees with the selected profile.
    ObjectTagLength {
        data_crypto_profile: u16,
        expected: u16,
        actual: u16,
    },
    /// A bootstrap's declared nonce width disagrees with the selected profile.
    ObjectNonceLength {
        data_crypto_profile: u16,
        expected: u16,
        actual: u16,
    },
    /// The declared `EncodingId` is not the digest of its own descriptor.
    EncodingId,
    /// The declared `PlacementId` is not the digest of its own descriptor.
    PlacementId,
}

impl core::fmt::Display for IdentityMismatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedDataCryptoProfile {
                data_crypto_profile,
            } => write!(f, "unsupported data-crypto profile {data_crypto_profile}"),
            Self::ObjectTagLength {
                data_crypto_profile,
                expected,
                actual,
            } => write!(
                f,
                "data-crypto profile {data_crypto_profile} requires tag length {expected}, not {actual}"
            ),
            Self::ObjectNonceLength {
                data_crypto_profile,
                expected,
                actual,
            } => write!(
                f,
                "data-crypto profile {data_crypto_profile} requires nonce length {expected}, not {actual}"
            ),
            Self::EncodingId => {
                f.write_str("declared EncodingId does not recompute from its descriptor")
            }
            Self::PlacementId => {
                f.write_str("declared PlacementId does not recompute from its descriptor")
            }
        }
    }
}

impl core::error::Error for IdentityMismatch {}

/// Why authenticated bytes recovered for an admitted encoding could not be
/// opened as the exact durable ciphertext that encoding names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveredObjectError {
    /// The AEAD rejected the recovered bytes, descriptor, object identity, or
    /// DEK. Deliberately carries no primitive detail.
    AuthenticationFailed,
    /// The bytes authenticated, but their descriptor+ciphertext+tag digest is
    /// not the durable `CiphertextId` carried by this encoding.
    CiphertextIdentityMismatch,
}

impl core::fmt::Display for RecoveredObjectError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::AuthenticationFailed => "recovered ciphertext failed authentication",
            Self::CiphertextIdentityMismatch => {
                "recovered ciphertext does not recompute the declared CiphertextId"
            }
        })
    }
}

impl core::error::Error for RecoveredObjectError {}

/// Stage 3 — *how it is coded*.
///
/// Carries the cipher descriptor forward as well as the encoding descriptor:
/// the plan requires a decoder to obtain the *complete* descriptor set from an
/// authenticated encoding before accepting any symbol, so the authenticated
/// chain — not a side channel and never an individual symbol — is what supplies
/// the AAD and nonce needed to open recovered bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedObject {
    cipher_descriptor: CipherDescriptor,
    object_id: ObjectId,
    ciphertext_id: Digest,
    descriptor: EncodingDescriptor,
    encoding_id: Digest,
}

impl EncodedObject {
    pub fn object_id(&self) -> ObjectId {
        self.object_id
    }

    pub fn ciphertext_id(&self) -> Digest {
        self.ciphertext_id
    }

    pub fn encoding_id(&self) -> Digest {
        self.encoding_id
    }

    pub fn descriptor(&self) -> &EncodingDescriptor {
        &self.descriptor
    }

    /// The per-encoding symbol-authentication key (plan L280):
    /// `K_symbol = KDF(DEK, "fgdb:symbol-auth:v1" ‖ EncodingId)`. Symbols from
    /// different encodings therefore never share a MAC key.
    /// RECOVERY CONSTRUCTOR. Rebuild an encoding from durable descriptors
    /// alone — no plaintext, no prior stage value — and VERIFY the declared
    /// identities recompute from them.
    ///
    /// This is what bootstrap recovery does: it holds a descriptor set read
    /// out of a durable frame and must decide whether that set really names
    /// the object it claims. Returning a checked value rather than a raw
    /// struct is the point — a reconstruction that skipped the recomputation
    /// would let a rewritten descriptor redirect recovery at other bytes.
    pub fn reconstruct(
        object_id: ObjectId,
        cipher_descriptor: CipherDescriptor,
        ciphertext_id: Digest,
        descriptor: EncodingDescriptor,
        declared_encoding_id: Digest,
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<Self, IdentityMismatch> {
        if let Err(error) = cipher_descriptor.registered_aead_profile() {
            let failure = match error {
                IdentityMismatch::UnsupportedDataCryptoProfile { .. } => {
                    VerificationFailureClass::UnsupportedDataCryptoProfile
                }
                IdentityMismatch::ObjectTagLength { .. } => {
                    VerificationFailureClass::ObjectTagLength
                }
                IdentityMismatch::ObjectNonceLength { .. } => {
                    VerificationFailureClass::InvalidParameters
                }
                IdentityMismatch::EncodingId => VerificationFailureClass::EncodingIdentity,
                IdentityMismatch::PlacementId => VerificationFailureClass::PlacementIdentity,
            };
            verification.record(verification_event(
                &cipher_descriptor,
                Some(declared_encoding_id),
                None,
                VerificationOperation::EncodingReconstruction,
                VerificationOutcome::Rejected(failure),
            ));
            return Err(error);
        }
        let recomputed = fgdb_crypto::encoding_id(&descriptor.canonical_bytes(ciphertext_id));
        if recomputed != declared_encoding_id {
            verification.record(verification_event(
                &cipher_descriptor,
                Some(declared_encoding_id),
                None,
                VerificationOperation::EncodingReconstruction,
                VerificationOutcome::Rejected(VerificationFailureClass::EncodingIdentity),
            ));
            return Err(IdentityMismatch::EncodingId);
        }
        verification.record(verification_event(
            &cipher_descriptor,
            Some(declared_encoding_id),
            None,
            VerificationOperation::EncodingReconstruction,
            VerificationOutcome::Accepted,
        ));
        Ok(Self {
            cipher_descriptor,
            object_id,
            ciphertext_id,
            descriptor,
            encoding_id: declared_encoding_id,
        })
    }

    /// Verify that a declared `PlacementId` recomputes from its descriptor
    /// against THIS encoding. Placement is where symbols physically live, so a
    /// rewritten placement is how an attacker or a bug points recovery at the
    /// wrong span.
    pub fn verify_placement(
        &self,
        descriptor: &PlacementDescriptor,
        declared_placement_id: Digest,
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<(), IdentityMismatch> {
        let recomputed = fgdb_crypto::placement_id(&descriptor.canonical_bytes(self.encoding_id));
        if recomputed != declared_placement_id {
            verification.record(verification_event(
                &self.cipher_descriptor,
                Some(self.encoding_id),
                None,
                VerificationOperation::PlacementIdentity,
                VerificationOutcome::Rejected(VerificationFailureClass::PlacementIdentity),
            ));
            return Err(IdentityMismatch::PlacementId);
        }
        verification.record(verification_event(
            &self.cipher_descriptor,
            Some(self.encoding_id),
            None,
            VerificationOperation::PlacementIdentity,
            VerificationOutcome::Accepted,
        ));
        Ok(())
    }

    /// The cipher descriptor carried forward from stage 2 — the authenticated
    /// source of the AAD and nonce a decoder needs.
    pub fn cipher_descriptor(&self) -> &CipherDescriptor {
        &self.cipher_descriptor
    }

    /// Open protected bytes recovered from symbols. Used by the symbolization
    /// decode path, where the ciphertext is reassembled rather than held: the
    /// AAD comes from this authenticated encoding, so recovered bytes that are
    /// not the sealed bytes cannot open.
    pub fn open_recovered(
        &self,
        protected_bytes: &[u8],
        dek: &[u8; 32],
        verification: &mut dyn CryptoVerificationSink,
    ) -> Result<Vec<u8>, RecoveredObjectError> {
        let aad = aead::object_aead_aad(
            &Digest(self.object_id.0),
            &self.cipher_descriptor.canonical_bytes(),
        );
        let opened = match aead::xchacha20poly1305_open(
            dek,
            &self.cipher_descriptor.object_nonce,
            &aad,
            protected_bytes,
        ) {
            Ok(opened) => opened,
            Err(_) => {
                verification.record(verification_event(
                    &self.cipher_descriptor,
                    Some(self.encoding_id),
                    Some(protected_bytes.len()),
                    VerificationOperation::RecoveredObjectOpen,
                    VerificationOutcome::Rejected(VerificationFailureClass::Authentication),
                ));
                return Err(RecoveredObjectError::AuthenticationFailed);
            }
        };
        let tag_len = usize::from(self.cipher_descriptor.object_tag_len);
        let Some(tag_start) = protected_bytes.len().checked_sub(tag_len) else {
            verification.record(verification_event(
                &self.cipher_descriptor,
                Some(self.encoding_id),
                Some(protected_bytes.len()),
                VerificationOperation::RecoveredObjectOpen,
                VerificationOutcome::Rejected(VerificationFailureClass::Authentication),
            ));
            return Err(RecoveredObjectError::AuthenticationFailed);
        };
        let recomputed = ciphertext_identity(
            &self.cipher_descriptor,
            &protected_bytes[..tag_start],
            &protected_bytes[tag_start..],
        );
        // ubs:ignore -- public content identity after AEAD authentication, not secret material.
        if recomputed != self.ciphertext_id {
            verification.record(verification_event(
                &self.cipher_descriptor,
                Some(self.encoding_id),
                Some(protected_bytes.len()),
                VerificationOperation::RecoveredObjectOpen,
                VerificationOutcome::Rejected(VerificationFailureClass::CiphertextIdentity),
            ));
            return Err(RecoveredObjectError::CiphertextIdentityMismatch);
        }
        verification.record(verification_event(
            &self.cipher_descriptor,
            Some(self.encoding_id),
            Some(protected_bytes.len()),
            VerificationOperation::RecoveredObjectOpen,
            VerificationOutcome::Accepted,
        ));
        Ok(opened)
    }

    pub fn symbol_auth_key(&self, dek: &[u8; 32]) -> [u8; 32] {
        fgdb_crypto::symbol_auth_key(dek, &self.encoding_id)
    }

    /// Stage 3 → 4. Adding or moving symbols creates a new placement record,
    /// never a new encoding identity — so this too borrows rather than
    /// consumes.
    pub fn place(&self, descriptor: PlacementDescriptor) -> PlacedObject {
        let placement_id = fgdb_crypto::placement_id(&descriptor.canonical_bytes(self.encoding_id));
        PlacedObject {
            object_id: self.object_id,
            encoding_id: self.encoding_id,
            descriptor,
            placement_id,
        }
    }
}

/// Where the symbols of one encoding physically live (plan L280).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocationForm {
    /// One fully described contiguous span with an authenticated inventory.
    ContiguousSpan {
        failure_domain_id: u32,
        segment_id: u64,
        offset: u64,
        encoded_len: u64,
        symbol_inventory_digest: Digest,
    },
    /// An explicit canonical symbol/locator inventory.
    Explicit {
        /// Canonically sorted; the caller supplies the canonical encoding.
        sorted_symbol_inventory: Vec<u8>,
        failure_domains: Vec<u32>,
    },
}

impl LocationForm {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::ContiguousSpan {
                failure_domain_id,
                segment_id,
                offset,
                encoded_len,
                symbol_inventory_digest,
            } => {
                out.push(0x01);
                out.extend_from_slice(&failure_domain_id.to_le_bytes());
                out.extend_from_slice(&segment_id.to_le_bytes());
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&encoded_len.to_le_bytes());
                out.extend_from_slice(&symbol_inventory_digest.0);
            }
            Self::Explicit {
                sorted_symbol_inventory,
                failure_domains,
            } => {
                out.push(0x02);
                out.extend_from_slice(&(sorted_symbol_inventory.len() as u64).to_le_bytes());
                out.extend_from_slice(sorted_symbol_inventory);
                out.extend_from_slice(&(failure_domains.len() as u64).to_le_bytes());
                for domain in failure_domains {
                    out.extend_from_slice(&domain.to_le_bytes());
                }
            }
        }
        out
    }
}

/// Physical locations and symbol inventories (plan L280).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementDescriptor {
    pub placement_epoch: u64,
    pub failure_domain_policy: u16,
    pub location_form: LocationForm,
}

impl PlacementDescriptor {
    pub(crate) fn canonical_bytes(&self, encoding_id: Digest) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&encoding_id.0);
        out.extend_from_slice(&self.placement_epoch.to_le_bytes());
        out.extend_from_slice(&self.failure_domain_policy.to_le_bytes());
        out.extend_from_slice(&self.location_form.canonical_bytes());
        out
    }
}

/// Stage 4 — *where it lives*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedObject {
    object_id: ObjectId,
    encoding_id: Digest,
    descriptor: PlacementDescriptor,
    placement_id: Digest,
}

impl PlacedObject {
    pub fn object_id(&self) -> ObjectId {
        self.object_id
    }

    pub fn encoding_id(&self) -> Digest {
        self.encoding_id
    }

    pub fn placement_id(&self) -> Digest {
        self.placement_id
    }

    pub fn descriptor(&self) -> &PlacementDescriptor {
        &self.descriptor
    }
}
