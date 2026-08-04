//! `manifest.root`: the only mutable object in a database directory.
//!
//! Everything else Chronicle writes is immutable and content-addressed. That
//! is only *safe* because this one mutable thing has an exhaustively specified
//! format and an exhaustively specified recovery rule — so this file is where
//! the doctrine's "no mutable primary file" claim is actually cashed.
//!
//! Two fixed 4096-byte slots at offsets 0 and 4096, written alternately. A
//! writer never updates a slot in place while it is the newest credible one,
//! so a torn write can destroy at most the slot being written, and the other
//! still holds a complete, authenticated state.
//!
//! THE RECOVERY RULE, in the order it must be applied (and the order is the
//! whole point):
//!   1. discard slots whose fixed record fails structural or tear validation;
//!   2. select the highest remaining generation;
//!   3. once a slot is structurally credible, an authentication or closure
//!      failure triggers repair and then **fails closed** — it NEVER silently
//!      rolls back to an older authenticated state, because that state may
//!      precede an acknowledged commit. Going backwards is data loss wearing
//!      the costume of recovery; an older generation requires the explicit
//!      restore protocol;
//!   4. two structurally credible equal-highest generations are accepted for
//!      READ recovery only when their complete authenticated bytes are
//!      identical; any difference fails closed.
//!
//! `RootBootstrap` is what makes recovery self-sufficient: it carries the
//! root's own complete cipher/encoding/placement description, so opening the
//! root never depends on the object-location index that the root itself must
//! bootstrap. No descriptor field comes from the object being opened.
//!
//! NOT IN THIS INCREMENT: the `RootManifest` four-posture union (its arms are
//! part of the G0 decision batch) and the publication sequencer/permit
//! machinery (sibling bead `w2-root-publication`). This is a genuine subset of
//! the final abstraction, not a substitute for it: the slot frame, the
//! self-sufficient bootstrap descriptor, and the selection rule are complete
//! and enforced as specified.

use crate::identity::{
    CipherDescriptor, EncodedObject, EncodingDescriptor, IdentityMismatch, LocationForm,
    PlacementDescriptor,
};
use crate::symbolize::{RecoveryTarget, SymbolizeError, decode_object};
use fgdb_crypto::Digest;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};

/// Fixed slot size. Both slots are exactly this, at offsets 0 and 4096.
pub const SLOT_LEN: usize = 4096;

/// Byte offset of slot A.
pub const SLOT_A_OFFSET: usize = 0;

/// Byte offset of slot B.
pub const SLOT_B_OFFSET: usize = 4096;

/// Total size of `manifest.root`.
pub const ROOT_FILE_LEN: usize = SLOT_LEN * 2;

/// Slot magic.
pub const ROOT_MAGIC: [u8; 4] = *b"FGRT";

/// Format major this build writes. A different major is not readable.
pub const ROOT_FORMAT_MAJOR: u16 = 1;

/// Format minor this build writes. A HIGHER minor is still readable —
/// additive-minor is the durable-format contract (§16.6).
pub const ROOT_FORMAT_MINOR: u16 = 0;

/// Domain separator for the tear checksum.
pub const TEAR_CHECKSUM_DOMAIN: &[u8] = b"fgdb:root-slot-tear:v1";

/// Fixed inline capacity of the opener payload; unused bytes are zero.
pub const OPENER_PAYLOAD_LEN: usize = 1024;

/// Fixed inline capacity of the nonce/SIV field; unused bytes are zero.
pub const NONCE_CAPACITY: usize = 24;

/// The tear checksum occupies the last 32 bytes of the slot.
const CHECKSUM_OFFSET: usize = SLOT_LEN - 32;

/// Why a slot was rejected. Rejection is always *structural or cryptographic*,
/// never a judgement call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotError {
    /// Not exactly `SLOT_LEN` bytes.
    WrongLength,
    /// Magic or format major is not one this build reads.
    UnsupportedFraming,
    /// A declared inline length exceeds its fixed capacity.
    InconsistentLengths,
    /// The tear checksum does not cover these bytes: the slot was torn
    /// mid-write, or rotted.
    TearDetected,
    /// Reserved bytes were not zero. A future field written by a build that
    /// thinks this minor understands it would be silently ignored otherwise.
    ReservedNotZero,
}

impl core::fmt::Display for SlotError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::WrongLength => "root slot is not exactly 4096 bytes",
            Self::UnsupportedFraming => "root slot framing is not supported",
            Self::InconsistentLengths => "root slot declares an impossible inline length",
            Self::TearDetected => "root slot tear checksum does not match",
            Self::ReservedNotZero => "root slot reserved bytes are not zero",
        })
    }
}

impl core::error::Error for SlotError {}

/// Everything needed to open the root **before** any index exists.
///
/// Together with `root_manifest_oid` these reproduce the root's exact
/// `CipherDescriptorWithoutDigest`, complete `EncodingDescriptorWithoutId`,
/// and its ContiguousSpan `PlacementDescriptorWithoutId` byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootBootstrap {
    pub root_encoding_id: [u8; 32],
    pub root_placement_id: [u8; 32],
    pub root_placement_epoch: u64,
    pub failure_domain_policy_id: u16,
    pub root_failure_domain_id: u32,
    pub segment_id: u64,
    pub offset: u64,
    pub encoded_len: u64,
    pub root_symbol_inventory_digest: [u8; 32],
    pub object_kind: u16,
    pub canonical_plaintext_len: u64,
    pub codec_profile: u16,
    pub compressed_len: u64,
    pub data_crypto_profile: u16,
    pub dek_id: [u8; 16],
    pub nonce_len: u16,
    pub nonce_or_siv: [u8; NONCE_CAPACITY],
    pub object_tag_len: u16,
    pub fec_profile: u16,
    pub transfer_length: u64,
    pub oti_common: u64,
    pub oti_scheme: u32,
    pub symbol_size: u16,
    pub source_block_count: u16,
    pub symbol_auth_profile: u16,
    pub ciphertext_id: [u8; 32],
    pub ciphertext_digest: [u8; 32],
    pub opener_kind: u16,
    pub oid_key_id: [u8; 16],
    pub opener_payload_len: u16,
    /// A versioned bundle of inline wraps or authenticated KMS/HSM locators,
    /// sufficient to recover both the root DEK and the immutable `K_oid`
    /// without consulting the object index.
    pub opener_payload: [u8; OPENER_PAYLOAD_LEN],
    pub opener_digest: [u8; 32],
}

impl RootBootstrap {
    fn write_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.root_encoding_id);
        out.extend_from_slice(&self.root_placement_id);
        out.extend_from_slice(&self.root_placement_epoch.to_be_bytes());
        out.extend_from_slice(&self.failure_domain_policy_id.to_be_bytes());
        out.extend_from_slice(&self.root_failure_domain_id.to_be_bytes());
        out.extend_from_slice(&self.segment_id.to_be_bytes());
        out.extend_from_slice(&self.offset.to_be_bytes());
        out.extend_from_slice(&self.encoded_len.to_be_bytes());
        out.extend_from_slice(&self.root_symbol_inventory_digest);
        out.extend_from_slice(&self.object_kind.to_be_bytes());
        out.extend_from_slice(&self.canonical_plaintext_len.to_be_bytes());
        out.extend_from_slice(&self.codec_profile.to_be_bytes());
        out.extend_from_slice(&self.compressed_len.to_be_bytes());
        out.extend_from_slice(&self.data_crypto_profile.to_be_bytes());
        out.extend_from_slice(&self.dek_id);
        out.extend_from_slice(&self.nonce_len.to_be_bytes());
        out.extend_from_slice(&self.nonce_or_siv);
        out.extend_from_slice(&self.object_tag_len.to_be_bytes());
        out.extend_from_slice(&self.fec_profile.to_be_bytes());
        out.extend_from_slice(&self.transfer_length.to_be_bytes());
        out.extend_from_slice(&self.oti_common.to_be_bytes());
        out.extend_from_slice(&self.oti_scheme.to_be_bytes());
        out.extend_from_slice(&self.symbol_size.to_be_bytes());
        out.extend_from_slice(&self.source_block_count.to_be_bytes());
        out.extend_from_slice(&self.symbol_auth_profile.to_be_bytes());
        out.extend_from_slice(&self.ciphertext_id);
        out.extend_from_slice(&self.ciphertext_digest);
        out.extend_from_slice(&self.opener_kind.to_be_bytes());
        out.extend_from_slice(&self.oid_key_id);
        out.extend_from_slice(&self.opener_payload_len.to_be_bytes());
        out.extend_from_slice(&self.opener_payload);
        out.extend_from_slice(&self.opener_digest);
    }

    fn read_from(cursor: &mut Cursor<'_>) -> Self {
        Self {
            root_encoding_id: cursor.take_32(),
            root_placement_id: cursor.take_32(),
            root_placement_epoch: cursor.take_u64(),
            failure_domain_policy_id: cursor.take_u16(),
            root_failure_domain_id: cursor.take_u32(),
            segment_id: cursor.take_u64(),
            offset: cursor.take_u64(),
            encoded_len: cursor.take_u64(),
            root_symbol_inventory_digest: cursor.take_32(),
            object_kind: cursor.take_u16(),
            canonical_plaintext_len: cursor.take_u64(),
            codec_profile: cursor.take_u16(),
            compressed_len: cursor.take_u64(),
            data_crypto_profile: cursor.take_u16(),
            dek_id: cursor.take_16(),
            nonce_len: cursor.take_u16(),
            nonce_or_siv: cursor.take_array::<NONCE_CAPACITY>(),
            object_tag_len: cursor.take_u16(),
            fec_profile: cursor.take_u16(),
            transfer_length: cursor.take_u64(),
            oti_common: cursor.take_u64(),
            oti_scheme: cursor.take_u32(),
            symbol_size: cursor.take_u16(),
            source_block_count: cursor.take_u16(),
            symbol_auth_profile: cursor.take_u16(),
            ciphertext_id: cursor.take_32(),
            ciphertext_digest: cursor.take_32(),
            opener_kind: cursor.take_u16(),
            oid_key_id: cursor.take_16(),
            opener_payload_len: cursor.take_u16(),
            opener_payload: cursor.take_array::<OPENER_PAYLOAD_LEN>(),
            opener_digest: cursor.take_32(),
        }
    }
}

/// One published recovery root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootSlot {
    pub format_major: u16,
    pub format_minor: u16,
    /// Monotone per publication. Slot selection is by highest generation.
    pub slot_generation: u64,
    pub local_writer_fence_epoch: u64,
    pub database_id: [u8; 16],
    pub database_security_namespace_id: [u8; 32],
    pub cluster_incarnation: u64,
    pub incarnation_continuity_profile_id: u16,
    pub cluster_incarnation_continuity_digest: [u8; 32],
    pub continuity_cas_version: u64,
    pub service_visibility_epoch: u64,
    pub root_manifest_oid: [u8; 32],
    pub bootstrap: RootBootstrap,
}

impl RootSlot {
    /// The slot's authenticated identity tuple. Recovery rejects a root whose
    /// own authenticated tuple disagrees with the slot that pointed at it, so
    /// this is the comparison that stops a valid root from another database,
    /// incarnation, or visibility epoch from being adopted.
    pub fn identity_tuple(&self) -> IdentityTuple {
        IdentityTuple {
            database_id: self.database_id,
            database_security_namespace_id: self.database_security_namespace_id,
            cluster_incarnation: self.cluster_incarnation,
            incarnation_continuity_profile_id: self.incarnation_continuity_profile_id,
            cluster_incarnation_continuity_digest: self.cluster_incarnation_continuity_digest,
            continuity_cas_version: self.continuity_cas_version,
            service_visibility_epoch: self.service_visibility_epoch,
        }
    }

    /// Serialize to exactly `SLOT_LEN` bytes: fixed record, zero padding, then
    /// the tear checksum over everything preceding it.
    pub fn serialize(&self) -> [u8; SLOT_LEN] {
        let mut body = Vec::with_capacity(SLOT_LEN);
        body.extend_from_slice(&ROOT_MAGIC);
        body.extend_from_slice(&self.format_major.to_be_bytes());
        body.extend_from_slice(&self.format_minor.to_be_bytes());
        body.extend_from_slice(&self.slot_generation.to_be_bytes());
        body.extend_from_slice(&self.local_writer_fence_epoch.to_be_bytes());
        body.extend_from_slice(&self.database_id);
        body.extend_from_slice(&self.database_security_namespace_id);
        body.extend_from_slice(&self.cluster_incarnation.to_be_bytes());
        body.extend_from_slice(&self.incarnation_continuity_profile_id.to_be_bytes());
        body.extend_from_slice(&self.cluster_incarnation_continuity_digest);
        body.extend_from_slice(&self.continuity_cas_version.to_be_bytes());
        body.extend_from_slice(&self.service_visibility_epoch.to_be_bytes());
        body.extend_from_slice(&self.root_manifest_oid);
        self.bootstrap.write_into(&mut body);

        let mut slot = [0u8; SLOT_LEN];
        slot[..body.len()].copy_from_slice(&body);
        // Reserved bytes between the record and the checksum stay zero.
        let checksum = tear_checksum(&slot[..CHECKSUM_OFFSET]);
        slot[CHECKSUM_OFFSET..].copy_from_slice(&checksum.0);
        slot
    }

    /// Parse and STRUCTURALLY VALIDATE one slot. Every rejection here is
    /// step 1 of the recovery rule: a slot that fails is discarded, and
    /// discarding is the only thing a structural failure ever causes.
    pub fn parse(bytes: &[u8]) -> Result<Self, SlotError> {
        if bytes.len() != SLOT_LEN {
            return Err(SlotError::WrongLength);
        }
        if bytes[..4] != ROOT_MAGIC {
            return Err(SlotError::UnsupportedFraming);
        }

        // The tear checksum is validated BEFORE any field is trusted: a torn
        // slot's declared lengths are not evidence of anything.
        let expected = tear_checksum(&bytes[..CHECKSUM_OFFSET]);
        let mut found = [0u8; 32];
        found.copy_from_slice(&bytes[CHECKSUM_OFFSET..]);
        if expected.0 != found {
            return Err(SlotError::TearDetected);
        }

        let mut cursor = Cursor::new(&bytes[4..]);
        let format_major = cursor.take_u16();
        let format_minor = cursor.take_u16();
        if format_major != ROOT_FORMAT_MAJOR {
            return Err(SlotError::UnsupportedFraming);
        }

        let slot = Self {
            format_major,
            format_minor,
            slot_generation: cursor.take_u64(),
            local_writer_fence_epoch: cursor.take_u64(),
            database_id: cursor.take_16(),
            database_security_namespace_id: cursor.take_32(),
            cluster_incarnation: cursor.take_u64(),
            incarnation_continuity_profile_id: cursor.take_u16(),
            cluster_incarnation_continuity_digest: cursor.take_32(),
            continuity_cas_version: cursor.take_u64(),
            service_visibility_epoch: cursor.take_u64(),
            root_manifest_oid: cursor.take_32(),
            bootstrap: RootBootstrap::read_from(&mut cursor),
        };

        // Declared inline lengths must fit their fixed capacities. A slot that
        // claims more opener bytes than exist is not merely wrong, it is a
        // read primitive pointed off the end of the record.
        if usize::from(slot.bootstrap.opener_payload_len) > OPENER_PAYLOAD_LEN
            || usize::from(slot.bootstrap.nonce_len) > NONCE_CAPACITY
        {
            return Err(SlotError::InconsistentLengths);
        }

        // Reserved bytes must be zero, so a future minor's field cannot be
        // silently ignored by this build.
        let record_end = 4 + cursor.position();
        if bytes[record_end..CHECKSUM_OFFSET].iter().any(|b| *b != 0) {
            return Err(SlotError::ReservedNotZero);
        }
        Ok(slot)
    }
}

/// The authenticated tuple a recovered root must agree with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityTuple {
    pub database_id: [u8; 16],
    pub database_security_namespace_id: [u8; 32],
    pub cluster_incarnation: u64,
    pub incarnation_continuity_profile_id: u16,
    pub cluster_incarnation_continuity_digest: [u8; 32],
    pub continuity_cas_version: u64,
    pub service_visibility_epoch: u64,
}

/// The tear checksum: a domain-separated digest over every preceding byte.
fn tear_checksum(preceding: &[u8]) -> Digest {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(TEAR_CHECKSUM_DOMAIN);
    hasher.update(preceding);
    hasher.finalize()
}

/// Which slot recovery selected, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootSelection {
    /// The root file is not exactly two fixed slots long. Slot credibility is
    /// irrelevant when the containing durable format is truncated or has
    /// trailing bytes.
    MalformedFile { len: usize },
    /// Exactly one credible slot, or one strictly newer than the other.
    Selected {
        slot: Box<RootSlot>,
        /// Which physical slot it came from: 0 = A, 1 = B.
        index: u8,
        /// The other slot's rejection, when it had one. A healthy alternating
        /// writer normally produces `None` here; a value means the last write
        /// was torn and the operator should know.
        other_rejected: Option<SlotError>,
    },
    /// Two structurally credible slots at the same generation whose complete
    /// bytes are identical. Legal for READ recovery; a writer must first
    /// normalize the pair through the takeover/convergence permit.
    IdenticalPair { slot: Box<RootSlot> },
    /// Neither slot is structurally credible. Fail closed — recovery does not
    /// invent a root.
    NoCredibleSlot {
        slot_a: SlotError,
        slot_b: SlotError,
    },
    /// Two credible slots at the same generation that DISAGREE. Fail closed:
    /// there is no rule that can choose between two equally-current published
    /// roots, and guessing risks discarding an acknowledged commit.
    DivergentPair { generation: u64 },
}

/// Apply the recovery rule to the two slots of a `manifest.root` file.
///
/// Takes the whole file so the caller cannot accidentally compare a slot with
/// itself, and so the offsets stay this module's business.
pub fn select_root(file_bytes: &[u8]) -> RootSelection {
    if file_bytes.len() != ROOT_FILE_LEN {
        return RootSelection::MalformedFile {
            len: file_bytes.len(),
        };
    }
    let read = |offset: usize| -> Result<RootSlot, SlotError> {
        file_bytes
            .get(offset..offset + SLOT_LEN)
            .ok_or(SlotError::WrongLength)
            .and_then(RootSlot::parse)
    };
    let parsed_a = read(SLOT_A_OFFSET);
    let parsed_b = read(SLOT_B_OFFSET);

    match (parsed_a, parsed_b) {
        // Step 1: discard structurally invalid slots. If both fail, fail
        // closed — recovery never invents a root.
        (Err(slot_a), Err(slot_b)) => RootSelection::NoCredibleSlot { slot_a, slot_b },
        (Ok(slot), Err(error)) => RootSelection::Selected {
            slot: Box::new(slot),
            index: 0,
            other_rejected: Some(error),
        },
        (Err(error), Ok(slot)) => RootSelection::Selected {
            slot: Box::new(slot),
            index: 1,
            other_rejected: Some(error),
        },
        (Ok(a), Ok(b)) => {
            // Step 2: highest generation wins. This is the ONLY direction
            // recovery ever moves.
            match a.slot_generation.cmp(&b.slot_generation) {
                core::cmp::Ordering::Greater => RootSelection::Selected {
                    slot: Box::new(a),
                    index: 0,
                    other_rejected: None,
                },
                core::cmp::Ordering::Less => RootSelection::Selected {
                    slot: Box::new(b),
                    index: 1,
                    other_rejected: None,
                },
                // Step 4: equal generations are acceptable only when the
                // complete authenticated bytes are identical.
                core::cmp::Ordering::Equal => {
                    let bytes_a = &file_bytes[SLOT_A_OFFSET..SLOT_A_OFFSET + SLOT_LEN];
                    let bytes_b = &file_bytes[SLOT_B_OFFSET..SLOT_B_OFFSET + SLOT_LEN];
                    if bytes_a == bytes_b {
                        RootSelection::IdenticalPair { slot: Box::new(a) }
                    } else {
                        RootSelection::DivergentPair {
                            generation: a.slot_generation,
                        }
                    }
                }
            }
        }
    }
}

/// A minimal big-endian reader. Bounds are guaranteed by the fixed slot size,
/// which `parse` validates before constructing one.
struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn position(&self) -> usize {
        self.position
    }

    fn take_array<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.position..self.position + N]);
        self.position += N;
        out
    }

    fn take_16(&mut self) -> [u8; 16] {
        self.take_array::<16>()
    }

    fn take_32(&mut self) -> [u8; 32] {
        self.take_array::<32>()
    }

    fn take_u16(&mut self) -> u16 {
        u16::from_be_bytes(self.take_array::<2>())
    }

    fn take_u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take_array::<4>())
    }

    fn take_u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take_array::<8>())
    }
}

/// Why self-sufficient root recovery failed. Every variant is fail-closed and
/// names WHICH check refused, because the escalations differ: a descriptor
/// that does not recompute is a corrupt or rewritten frame, while insufficient
/// symbols is a repair problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootRecoveryError {
    /// A declared identity did not recompute from its own descriptor.
    DescriptorMismatch(IdentityMismatch),
    /// The symbols could not reproduce the root object.
    Recovery(SymbolizeError),
    /// The recovered root's authenticated identity tuple disagrees with the
    /// slot that pointed at it. A perfectly valid root belonging to another
    /// database, incarnation, or visibility epoch must never be adopted.
    IdentityTupleMismatch,
}

impl core::fmt::Display for RootRecoveryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DescriptorMismatch(inner) => write!(f, "root descriptor rejected: {inner}"),
            Self::Recovery(inner) => write!(f, "root object not recovered: {inner}"),
            Self::IdentityTupleMismatch => {
                f.write_str("recovered root belongs to a different database identity")
            }
        }
    }
}

impl core::error::Error for RootRecoveryError {}

impl RootBootstrap {
    /// Rebuild the root's exact `CipherDescriptorWithoutDigest`.
    pub fn cipher_descriptor(&self) -> CipherDescriptor {
        CipherDescriptor {
            object_kind: self.object_kind,
            canonical_plaintext_len: self.canonical_plaintext_len,
            codec_profile: self.codec_profile,
            compressed_len: self.compressed_len,
            data_crypto_profile: self.data_crypto_profile,
            dek_id: self.dek_id,
            object_nonce: self.nonce_or_siv,
            object_tag_len: self.object_tag_len,
        }
    }

    /// Rebuild the root's complete `EncodingDescriptorWithoutId`.
    pub fn encoding_descriptor(&self) -> EncodingDescriptor {
        EncodingDescriptor {
            fec_profile: self.fec_profile,
            transfer_length: self.transfer_length,
            oti_common: self.oti_common,
            oti_scheme: self.oti_scheme,
            symbol_size: self.symbol_size,
            source_block_count: self.source_block_count,
            symbol_auth_profile: self.symbol_auth_profile,
        }
    }

    /// Rebuild the root's specialized ContiguousSpan
    /// `PlacementDescriptorWithoutId`.
    pub fn placement_descriptor(&self) -> PlacementDescriptor {
        PlacementDescriptor {
            placement_epoch: self.root_placement_epoch,
            failure_domain_policy: self.failure_domain_policy_id,
            location_form: LocationForm::ContiguousSpan {
                failure_domain_id: self.root_failure_domain_id,
                segment_id: self.segment_id,
                offset: self.offset,
                encoded_len: self.encoded_len,
                symbol_inventory_digest: Digest(self.root_symbol_inventory_digest),
            },
        }
    }
}

/// SELF-SUFFICIENT ROOT RECOVERY, in the exact order the plan specifies.
///
/// Nothing here is read from the object being opened, and nothing is read from
/// an index: every descriptor comes from the slot, which is why a database can
/// be recovered from a fixed offset with no prior state.
///
/// The order matters as much as the steps. Descriptors are verified to
/// recompute their own declared identities BEFORE any symbol is trusted;
/// symbols are authenticated before they enter a decode; the keyed `ObjectId`
/// is recomputed from the recovered plaintext; and only then is the root's own
/// authenticated identity tuple compared against the slot. A check moved later
/// than this would let a rewritten field steer the ones before it.
pub fn recover_root_object(
    slot: &RootSlot,
    serialized_symbols: &[Vec<u8>],
    k_oid: &[u8; 32],
    dek: &[u8; 32],
    recovered_identity_tuple: impl Fn(&[u8]) -> IdentityTuple,
) -> Result<Vec<u8>, RootRecoveryError> {
    let bootstrap = &slot.bootstrap;
    let root_object_id = ObjectId(slot.root_manifest_oid);

    // Step 1: the descriptor set must be self-consistent. An EncodingId that
    // is not the digest of its own descriptor means the frame was rewritten,
    // and no amount of valid symbols makes that safe.
    let encoding = EncodedObject::reconstruct(
        root_object_id,
        bootstrap.cipher_descriptor(),
        Digest(bootstrap.ciphertext_id),
        bootstrap.encoding_descriptor(),
        Digest(bootstrap.root_encoding_id),
    )
    .map_err(RootRecoveryError::DescriptorMismatch)?;

    // Step 2: the placement must recompute too — placement is WHERE the
    // symbols are, so a rewritten placement is how recovery gets pointed at
    // another object's bytes.
    encoding
        .verify_placement(
            &bootstrap.placement_descriptor(),
            Digest(bootstrap.root_placement_id),
        )
        .map_err(RootRecoveryError::DescriptorMismatch)?;

    // Steps 3-5: authenticate every symbol, decode, open the AEAD, and
    // recompute the keyed ObjectId. `decode_object` owns that sequence and
    // fails closed at each stage.
    let protected_len = usize::try_from(bootstrap.encoded_len)
        .map_err(|_| RootRecoveryError::Recovery(SymbolizeError::InvalidParameters))?;
    let namespace = DatabaseSecurityNamespaceId(slot.database_security_namespace_id);
    let recovered = decode_object(
        &encoding,
        serialized_symbols,
        RecoveryTarget {
            k_oid,
            namespace,
            object_id: root_object_id,
            // The root's protected bytes ARE its complete canonical
            // plaintext, header included, so the header is not a separate
            // recomputation input. Passing it again would hash it twice and
            // no well-formed root would ever recover. The §5.1 transcript
            // concatenates header and payload, so hashing the whole canonical
            // plaintext as the payload reproduces the identical byte stream.
            canonical_header: &[],
            protected_len,
        },
        dek,
    )
    .map_err(RootRecoveryError::Recovery)?;

    // Step 6: the recovered root's OWN authenticated identity tuple must agree
    // with the slot that pointed at it. Every preceding check proves the bytes
    // are the object named; this one proves the object belongs to THIS
    // database, incarnation, continuity lineage, and visibility epoch.
    if recovered_identity_tuple(&recovered) != slot.identity_tuple() {
        return Err(RootRecoveryError::IdentityTupleMismatch);
    }
    Ok(recovered)
}
