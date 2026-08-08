//! `fgdb-strata` — B2's graph-structured LSM, currently implementing the Tier-D
//! delta-block posture.
//!
//! §2 of the plan puts adjacency in three temperature tiers: versioned delta
//! blocks, sealed compressed CSR runs, archived anchors. This crate now owns the
//! first tier's exact block format, ordered writer, partition roots, immutable
//! block store, cross-block snapshot merge, and compaction. Those pieces make a
//! durable partition reopenable without replaying the commit stream; they do not
//! imply that the warmer tiers or adaptive migration controller exist yet.
//!
//! **WHY DURABLE BLOCKS AND NOT A MAP, WHICH IS THE INTERESTING PART.** The obvious
//! first slice was a `BTreeMap<(VId, RelationId, VId, EId), _>` with the tier's
//! visibility semantics on top. It would be smaller, it would pass a differential
//! against the reference oracle, and it would be the exact thing doctrine 7
//! prohibits: "no `HashMap<VId, Vec<EId>>` presented as storage", and "early code
//! may implement a subset of a final abstraction — never a substitute for it". An
//! in-memory map with the right answers is a substitute for storage; it has no
//! durable form, so nothing about it can be wrong in the way storage is wrong.
//!
//! Durable blocks and roots can be wrong in exactly those ways, which is why this
//! is the honest slice: they can be non-canonical, truncated, falsely addressed,
//! published out of order, or disagree about visibility ranges. Every one of
//! those boundaries has a typed refusal and a law below.
//!
//! **WHAT IS DELIBERATELY ABSENT**, so the gap is legible rather than implied:
//! sealed compressed CSR runs, archived anchors, the stable-ID directory, tier
//! migration and its decision cards, and a read path that skips unopened blocks
//! without weakening the proof that every authenticated root range is truthful.
//!
//! **CANONICAL MEANS EXACTLY ONE BYTE STRING PER VALUE** (doctrine 4). Entries are
//! strictly ascending by `(src, relation, dst, eid)` and the ENCODER refuses input that
//! is not, rather than sorting it: a caller handing over a different order is
//! describing a different intent, and quietly repairing it would let two callers
//! disagree about what they stored while both succeed. The DECODER independently
//! refuses a block whose entries are not ascending, so a hand-built block cannot
//! smuggle an order the encoder would never emit.

#![forbid(unsafe_code)]

pub mod compact;
pub mod edge_props;
pub mod root;
pub mod store;
pub mod vertex;
pub mod writer;

use fgdb_codec::identity::{
    ElementIdentity, IdentityColumn, IdentityColumnDescriptor, IdentityColumnError,
    IdentityColumnLimits, IdentityRepresentation,
};
use fgdb_delta_types::RelationId;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{CommitSeq, EId, VId};

/// `FGSB` — FrankenGraph Strata Block.
pub const BLOCK_MAGIC: [u8; 4] = *b"FGSB";
/// Format version. Durable formats are versioned from day one (§16.6):
/// additive-minor, breaking-major.
///
/// **THESE BYTES ARE DELIBERATELY UNREGISTERED, WHICH IS AN EXPOSURE RATHER THAN
/// AN OVERSIGHT.** Appendix A is the normative on-disk contract, and no row in it
/// describes this format or the partition root beside it: `DeltaBlockVersion`
/// holds a code RESERVATION only (`plan:reservation:delta-block-version`,
/// `0x04d4`, disposition `reserved`) and `PartitionRoot` has no code anywhere —
/// not a kind, not a wire type, not a reservation. So every format law this
/// crate enforces, it enforces on its own authority; none of the catalog's
/// cross-cutting machinery — identity class, construction order, retention and
/// cut rules, golden corpora, GC reachability under FG-INV-14 — reaches the only
/// place graph data actually lives.
///
/// **REGISTERING IT TODAY WOULD BE WORSE THAN LEAVING IT OUT**, which is why the
/// row is absent on purpose rather than merely missing. A catalog row FREEZES the
/// normative contract, and §6.2's unit is `DeltaBlockVersion {format,
/// partition_id, descriptor_key, stripe_range, sorted_entries[],
/// visibility_intervals[], property_patch_refs[], predecessor,
/// canonical_logical_digest}`. V3 carries five of those nine — format, the
/// descriptor key, identity-column-coded sorted entries (fgdb-by2l; the
/// codec's joint-fit witness pins ~13 B/entry against the 16 B ceiling),
/// visibility spans, and the property-patch-ref count as a fail-closed
/// reservation (fgdb-2t7q 3B) — while FOUR remain absent: `partition_id`,
/// `stripe_range`, `predecessor` (the per-block MVCC chain), and
/// `canonical_logical_digest`. Freezing the incomplete shape would enshrine
/// it as the normative contract, and undoing that later costs a
/// breaking-major format change plus a catalog re-pin cycle.
/// (An earlier revision of this comment claimed six absent fields and a raw
/// 128-bit entry encoding; both went stale when V3 adopted the codec, which
/// is its own lesson about prose beside moving formats.)
///
/// The gap is MEASURED, not asserted — see the byte-economy witnesses in
/// `tests/delta_block_format.rs`, which encode real blocks and publish the bad
/// numbers. Registration is sequenced behind `fgdb-w3-tier-d-ctj` (bring the
/// block to its normative field set and the identity-column codec) and then
/// `fgdb-ge6a` (register both formats, and root the partition binding that makes
/// a database reopenable from its `manifest.root` alone).
pub const BLOCK_FORMAT_V3: u16 = 3;
/// Format version. V4 is a breaking bump (§16.6): the reserved property-patch
/// count becomes meaningful (fgdb-yqor, ruling 2t7q 3B) — when non-zero, the
/// spans are followed by the patch identities (32 B each) and a one-byte
/// per-entry `prop_row_ref` locator column. A zero-patch V4 block is
/// byte-identical to V3 except this version field, which keeps the
/// byte-economy witnesses' arithmetic intact. V3 is refused by name; no
/// production database predates V4.
pub const BLOCK_FORMAT_V4: u16 = 4;

/// Durable object kind for a Tier-D delta block.
///
/// This is part of the §5.1 logical-identity header. It is deliberately
/// separate from the block payload framing: a future durable object with the
/// same bytes must not share a logical object identity merely because its
/// payload happens to begin with `FGSB`.
pub const DELTA_BLOCK_OBJECT_KIND: u16 = 0x0301;

/// The V3 framing ID for the §6.2 identity-column scalar codec.  The scalar
/// codec deliberately has no durable envelope; this caller owns its ID,
/// descriptor, exact count, and payload framing.
pub const IDENTITY_COLUMN_CODEC_ID: u16 = 1;

/// The content identity of one immutable Tier-D block version.
///
/// The wrapped [`ObjectId`] is still the ordinary §5.1 logical object identity
/// derived from the block's canonical bytes. This type adds no new identity
/// transcript; it prevents a block identity from being passed accidentally to
/// a partition-root operation.
///
/// ```compile_fail
/// use fgdb_strata::DeltaBlockVersion;
/// use fgdb_strata::store::BlockStore;
/// use fgdb_types::context::CommitCx;
///
/// fn block_is_not_a_root(store: &BlockStore, cx: &CommitCx, block: DeltaBlockVersion) {
///     let _ = store.get_root(cx, block);
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct DeltaBlockVersion(pub ObjectId);

/// The content identity of one immutable partition-root version.
///
/// Like [`DeltaBlockVersion`], this is a semantic type boundary around the
/// existing §5.1 [`ObjectId`], not a second content-addressing domain.
///
/// ```compile_fail
/// use fgdb_strata::PartitionRootVersion;
/// use fgdb_strata::store::BlockStore;
/// use fgdb_types::context::CommitCx;
///
/// fn root_is_not_a_block(store: &BlockStore, cx: &CommitCx, root: PartitionRootVersion) {
///     let _ = store.get(cx, root);
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PartitionRootVersion(pub ObjectId);

/// Header: magic + format + rows + `(src, relation, direction)` + span count
/// + property-patch-ref count.
///
/// The final u16 is §6.2's `property_patch_refs[]` in its ruled shape
/// (fgdb-2t7q ruling 3B: a block-level ref set indexed by a per-entry
/// `prop_row_ref` locator), carried as an EXPLICIT RESERVATION: the count is
/// framed and validated, and a nonzero value is refused fail-closed until
/// `fgdb-w3-properties-gou` lands the patch objects and the locator column
/// beside real data. The sitting's own alternative (3C) was accepted only on
/// the condition that deferral be "explicitly reserved rather than silently
/// omitted, or the format is re-broken when properties lands" — this slot is
/// that reservation, in the same pattern as `Direction`'s reserved reverse
/// family. Budget note, measured in `tests/delta_block_format.rs`: the two
/// identity columns cost 13 B/entry and visibility spans amortize under 2, so
/// the locator has >=1 B of the 16 B ceiling left — the joint-fit witness
/// pins that arithmetic.
const HEADER_LEN: usize = 4 + 2 + 4 + 16 + 8 + 1 + 4 + 2;
const COLUMN_FRAME_LEN: usize = 2 + 1 + 4 + 4;
const VISIBILITY_SPAN_LEN: usize = 4 + 4 + 8 + 8;
const MAX_IDENTITY_COLUMN_BYTES: usize = 4096;

/// The §6.2 descriptor fields shared by every row in one block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DescriptorKey {
    pub src: VId,
    pub relation: RelationId,
    pub direction: Direction,
}

/// V3 writes source-grouped blocks.  The tag is durable so reverse-family
/// blocks cannot be confused with this layout in a later format version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Direction {
    Outbound = 0,
}

/// A canonical `[start_row, end_row)` run with one half-open visibility value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibilityInterval {
    pub start_row: u32,
    pub end_row: u32,
    pub created_at: CommitSeq,
    pub retired_at: Option<CommitSeq>,
}

/// One versioned adjacency entry: an edge slot and the interval it is visible in.
///
/// The interval is HALF-OPEN, `[created_at, retired_at)`, for the same reason
/// valid-time periods are: with a closed upper bound an edge retired at sequence
/// N and one created at N would both be visible at N, so a replaced edge would
/// have two simultaneous versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AdjacencyEntry {
    pub src: VId,
    pub relation: RelationId,
    pub dst: VId,
    /// The unconditional discriminator between parallel edges (§4.1).
    pub eid: EId,
    pub created_at: CommitSeq,
    /// The sequence that retired this entry, or `None` while it is live.
    pub retired_at: Option<CommitSeq>,
}

impl AdjacencyEntry {
    /// Is this entry visible to a reader at `as_of`?
    ///
    /// `created_at <= as_of < retired_at`. Exposed because the visibility rule is
    /// the tier's semantics rather than an implementation detail of the scan, and
    /// a caller comparing tiers must be able to ask the same question of each.
    pub fn visible_at(&self, as_of: CommitSeq) -> bool {
        self.created_at.0 <= as_of.0 && self.retired_at.is_none_or(|r| as_of.0 < r.0)
    }
}

/// Why a block could not be encoded, decoded, or scanned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// The bytes do not begin with [`BLOCK_MAGIC`].
    NotABlock,
    /// A format version this build does not implement. Named rather than
    /// collapsed into `NotABlock`: "this is not our file" and "this is a newer
    /// version of our file" call for completely different operator responses.
    UnsupportedFormat {
        format: u16,
    },
    /// The bytes end before the declared entries do.
    Truncated {
        expected: usize,
        found: usize,
    },
    /// Bytes remain after the declared entries. Refused rather than ignored: a
    /// trailing region is either a second block someone concatenated or damage,
    /// and both are wrong to read past.
    TrailingBytes {
        extra: usize,
    },
    /// Entries are not strictly ascending by `(src, relation, dst, eid)`.
    ///
    /// Carries the position so a diagnostic can name the pair, since "this block
    /// is unsorted" is not actionable on a block with thousands of entries.
    NonCanonicalOrder {
        at: usize,
    },
    /// An entry claims to have been retired at or before it was created.
    RetiredBeforeCreated {
        at: usize,
        created_at: CommitSeq,
        retired_at: CommitSeq,
    },
    /// An entry was created at sequence zero, which names the empty stream and
    /// can therefore never have created anything.
    CreatedAtZero {
        at: usize,
    },
    /// More entries than this build will materialize from one block.
    ImplausibleEntryCount {
        declared: u32,
    },
    MixedDescriptor {
        at: usize,
    },
    UnsupportedDirection {
        direction: u8,
    },
    /// The property-patch count exceeds what this slice implements: V4 makes
    /// the reserved count meaningful for AT MOST ONE patch per block
    /// (fgdb-yqor); the multi-patch arm of ruling 2t7q 3B remains fail-closed
    /// until a block that needs it exists. Bytes claiming a capability this
    /// decoder does not have are refused, never skipped.
    PropertyPatchesNotYetImplemented {
        declared: u16,
    },
    /// The locator column violates its own canonical law: non-zero locators
    /// must be exactly `1..=n` in entry order (fgdb-yqor). Carries the entry
    /// position so the diagnostic names the pair.
    NonCanonicalLocators {
        at: usize,
    },
    UnsupportedIdentityCodec {
        column: u8,
        codec_id: u16,
    },
    IdentityColumn {
        column: u8,
        error: IdentityColumnError,
    },
    InvalidVisibilityInterval {
        at: usize,
    },
    /// The bytes are not the block that was asked for.
    ///
    /// Distinct from every other arm here: those say the bytes are malformed,
    /// this says they are well-formed and WRONG. A content-addressed store that
    /// returned a different object than the one named would be silent, which is
    /// the one failure worse than refusing.
    IdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
}

impl core::fmt::Display for BlockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotABlock => write!(f, "not a strata block"),
            Self::UnsupportedFormat { format } => {
                write!(f, "block format {format} is not implemented")
            }
            Self::Truncated { expected, found } => {
                write!(
                    f,
                    "block declares {expected} bytes of entries, found {found}"
                )
            }
            Self::TrailingBytes { extra } => write!(f, "{extra} bytes after the last entry"),
            Self::NonCanonicalOrder { at } => {
                write!(f, "entry {at} does not strictly follow its predecessor")
            }
            Self::RetiredBeforeCreated {
                at,
                created_at,
                retired_at,
            } => write!(
                f,
                "entry {at} was retired at {retired_at:?} but created at {created_at:?}"
            ),
            Self::CreatedAtZero { at } => {
                write!(f, "entry {at} claims creation at the empty stream")
            }
            Self::IdentityMismatch { expected, actual } => write!(
                f,
                "these bytes are block {actual:?}, not the requested {expected:?}"
            ),
            Self::ImplausibleEntryCount { declared } => {
                write!(
                    f,
                    "a block declaring {declared} entries is not readable here"
                )
            }
            Self::MixedDescriptor { at } => {
                write!(f, "entry {at} differs from the block descriptor")
            }
            Self::UnsupportedDirection { direction } => {
                write!(f, "block direction {direction} is not implemented")
            }
            Self::PropertyPatchesNotYetImplemented { declared } => write!(
                f,
                "block declares {declared} property patch refs; this slice \
                 implements at most one per block"
            ),
            Self::NonCanonicalLocators { at } => write!(
                f,
                "the locator column violates its 1..=n entry-order law at entry {at}"
            ),
            Self::UnsupportedIdentityCodec { column, codec_id } => write!(
                f,
                "identity column {column} names unsupported codec {codec_id:#06x}"
            ),
            Self::IdentityColumn { column, error } => {
                write!(f, "identity column {column}: {error}")
            }
            Self::InvalidVisibilityInterval { at } => write!(
                f,
                "visibility span {at} does not canonically cover the block rows"
            ),
        }
    }
}

impl core::error::Error for BlockError {}

/// The largest entry count this build will accept from a declared header.
///
/// A length prefix read from possibly-damaged bytes must be bounded before it is
/// used to size anything — the same rule the commit log's `MAX_ENTRY_BODY`
/// applies. Without it a corrupted count is an allocation request.
pub const MAX_BLOCK_ENTRIES: u32 = 256;

/// Encode `entries` into a canonical block.
///
/// REFUSES rather than sorts. A caller whose entries are out of order or repeated
/// is describing something other than what a block means, and quietly repairing it
/// would let two callers store different intents and both be told they succeeded.
pub fn encode_block(entries: &[AdjacencyEntry]) -> Result<Vec<u8>, BlockError> {
    encode_block_inner(entries, None)
}

/// Encode a block that hosts one edge property patch (fgdb-yqor): `patch_id`
/// names the FGSP object and `locators` carries one byte per entry — 0 for
/// "no properties", otherwise the 1-based row in the patch, in the bijection
/// order [`edge_props::validate_locator_sequence`] enforces. The patch itself
/// is a separate content-addressed object; the joint law is proven at
/// admission with both in hand.
pub fn encode_block_with_properties(
    entries: &[AdjacencyEntry],
    patch_id: ObjectId,
    locators: &[u8],
) -> Result<Vec<u8>, BlockError> {
    if locators.len() != entries.len() {
        return Err(BlockError::NonCanonicalLocators {
            at: locators.len().min(entries.len()),
        });
    }
    let referenced =
        edge_props::validate_locator_sequence(locators).map_err(|error| match error {
            edge_props::EdgePropertyPatchError::LocatorBijectionViolation { entry_at, .. } => {
                BlockError::NonCanonicalLocators { at: entry_at }
            }
            _ => BlockError::NonCanonicalLocators { at: 0 },
        })?;
    if referenced == 0 {
        // A patch no entry references is dead weight; a propertyless block is
        // encoded WITHOUT the section, so there is exactly one byte string
        // for that meaning (doctrine 4).
        return Err(BlockError::NonCanonicalLocators { at: 0 });
    }
    encode_block_inner(entries, Some((patch_id, locators)))
}

fn encode_block_inner(
    entries: &[AdjacencyEntry],
    patch: Option<(ObjectId, &[u8])>,
) -> Result<Vec<u8>, BlockError> {
    if entries.len() as u64 > u64::from(MAX_BLOCK_ENTRIES) {
        return Err(BlockError::ImplausibleEntryCount {
            declared: MAX_BLOCK_ENTRIES,
        });
    }
    let descriptor = entries.first().map(|entry| DescriptorKey {
        src: entry.src,
        relation: entry.relation,
        direction: Direction::Outbound,
    });
    for (index, entry) in entries.iter().enumerate() {
        validate_entry(index, entry)?;
        if let Some(descriptor) = descriptor
            && (entry.src != descriptor.src || entry.relation != descriptor.relation)
        {
            return Err(BlockError::MixedDescriptor { at: index });
        }
        if index > 0 && (entries[index - 1].dst, entries[index - 1].eid) >= (entry.dst, entry.eid) {
            return Err(BlockError::NonCanonicalOrder { at: index });
        }
    }

    let limits = IdentityColumnLimits::new(
        entries.len(),
        MAX_BLOCK_ENTRIES as usize,
        MAX_IDENTITY_COLUMN_BYTES,
    );
    let destinations: Vec<VId> = entries.iter().map(|entry| entry.dst).collect();
    let edge_ids: Vec<EId> = entries.iter().map(|entry| entry.eid).collect();
    let destinations = IdentityColumn::try_new(&destinations, limits)
        .map_err(|error| BlockError::IdentityColumn { column: 0, error })?;
    let edge_ids = IdentityColumn::try_new(&edge_ids, limits)
        .map_err(|error| BlockError::IdentityColumn { column: 1, error })?;
    let destination_payload = destinations
        .try_scalar_payload(MAX_IDENTITY_COLUMN_BYTES)
        .map_err(|error| BlockError::IdentityColumn { column: 0, error })?;
    let edge_id_payload = edge_ids
        .try_scalar_payload(MAX_IDENTITY_COLUMN_BYTES)
        .map_err(|error| BlockError::IdentityColumn { column: 1, error })?;
    let spans = visibility_spans(entries);

    let mut out = Vec::with_capacity(
        HEADER_LEN
            + 2 * COLUMN_FRAME_LEN
            + destination_payload.len()
            + edge_id_payload.len()
            + spans.len() * VISIBILITY_SPAN_LEN,
    );
    out.extend_from_slice(&BLOCK_MAGIC);
    out.extend_from_slice(&BLOCK_FORMAT_V4.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    let descriptor = descriptor.unwrap_or(DescriptorKey {
        src: VId(0),
        relation: RelationId(0),
        direction: Direction::Outbound,
    });
    out.extend_from_slice(&descriptor.src.0.to_be_bytes());
    out.extend_from_slice(&descriptor.relation.0.to_be_bytes());
    out.push(descriptor.direction as u8);
    out.extend_from_slice(&(spans.len() as u32).to_be_bytes());
    // property_patch_refs[] (fgdb-2t7q ruling 3B, live since V4/fgdb-yqor):
    // zero or one patch in this slice — see HEADER_LEN's doc.
    out.extend_from_slice(&u16::from(patch.is_some()).to_be_bytes());
    append_identity_column(&mut out, destinations.descriptor(), &destination_payload);
    append_identity_column(&mut out, edge_ids.descriptor(), &edge_id_payload);
    for span in spans {
        out.extend_from_slice(&span.start_row.to_be_bytes());
        out.extend_from_slice(&span.end_row.to_be_bytes());
        out.extend_from_slice(&span.created_at.0.to_be_bytes());
        out.extend_from_slice(&span.retired_at.map_or(0, |seq| seq.0).to_be_bytes());
    }
    if let Some((patch_id, locators)) = patch {
        out.extend_from_slice(&patch_id.0);
        out.extend_from_slice(locators);
    }
    Ok(out)
}

fn visibility_spans(entries: &[AdjacencyEntry]) -> Vec<VisibilityInterval> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    while start < entries.len() {
        let first = entries[start];
        let mut end = start + 1;
        while end < entries.len()
            && entries[end].created_at == first.created_at
            && entries[end].retired_at == first.retired_at
        {
            end += 1;
        }
        spans.push(VisibilityInterval {
            start_row: start as u32,
            end_row: end as u32,
            created_at: first.created_at,
            retired_at: first.retired_at,
        });
        start = end;
    }
    spans
}

fn representation_tag(representation: IdentityRepresentation) -> u8 {
    match representation {
        IdentityRepresentation::Raw128 => 0,
        IdentityRepresentation::SharedPrefixFixed => 1,
        IdentityRepresentation::SharedPrefixFor => 2,
        IdentityRepresentation::SharedPrefixDeltaFor => 3,
    }
}

fn representation_from_tag(tag: u8) -> Option<IdentityRepresentation> {
    match tag {
        0 => Some(IdentityRepresentation::Raw128),
        1 => Some(IdentityRepresentation::SharedPrefixFixed),
        2 => Some(IdentityRepresentation::SharedPrefixFor),
        3 => Some(IdentityRepresentation::SharedPrefixDeltaFor),
        _ => None,
    }
}

fn append_identity_column(out: &mut Vec<u8>, descriptor: IdentityColumnDescriptor, payload: &[u8]) {
    out.extend_from_slice(&IDENTITY_COLUMN_CODEC_ID.to_be_bytes());
    out.push(representation_tag(descriptor.representation()));
    out.extend_from_slice(&(descriptor.prefixes() as u32).to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
}

pub(crate) fn validate_entry(index: usize, entry: &AdjacencyEntry) -> Result<(), BlockError> {
    if entry.created_at.0 == 0 {
        return Err(BlockError::CreatedAtZero { at: index });
    }
    if let Some(retired) = entry.retired_at
        && retired.0 <= entry.created_at.0
    {
        return Err(BlockError::RetiredBeforeCreated {
            at: index,
            created_at: entry.created_at,
            retired_at: retired,
        });
    }
    Ok(())
}

struct DecodedFrame {
    descriptor: DescriptorKey,
    destinations: IdentityColumn<VId>,
    edge_ids: IdentityColumn<EId>,
    spans: Vec<VisibilityInterval>,
    /// The hosted edge property patch, when the block declares one:
    /// its identity and the per-entry locator column (fgdb-yqor).
    patch: Option<(ObjectId, Vec<u8>)>,
}

/// Read the V3 framing and reconstruct its two codec columns.  Counts live in
/// the enclosing frame, never in the scalar payload, so truncation is checked
/// before either codec sees a slice.
fn read_header(bytes: &[u8]) -> Result<DecodedFrame, BlockError> {
    if bytes.len() < HEADER_LEN || bytes[..4] != BLOCK_MAGIC {
        return Err(BlockError::NotABlock);
    }
    let format = u16::from_be_bytes([bytes[4], bytes[5]]);
    if format != BLOCK_FORMAT_V4 {
        return Err(BlockError::UnsupportedFormat { format });
    }
    let count = u32::from_be_bytes(bytes[6..10].try_into().expect("fixed header"));
    if count > MAX_BLOCK_ENTRIES {
        return Err(BlockError::ImplausibleEntryCount { declared: count });
    }
    let descriptor = DescriptorKey {
        src: VId(u128::from_be_bytes(
            bytes[10..26].try_into().expect("fixed header"),
        )),
        relation: RelationId(u64::from_be_bytes(
            bytes[26..34].try_into().expect("fixed header"),
        )),
        direction: match bytes[34] {
            0 => Direction::Outbound,
            direction => return Err(BlockError::UnsupportedDirection { direction }),
        },
    };
    let span_count = u32::from_be_bytes(bytes[35..39].try_into().expect("fixed header")) as usize;
    let patch_ref_count = u16::from_be_bytes(bytes[39..41].try_into().expect("fixed header"));
    if patch_ref_count > 1 {
        // The ruled shape is a SET; this slice implements at most one patch
        // per block, and the further arm stays fail-closed (2t7q 3B).
        return Err(BlockError::PropertyPatchesNotYetImplemented {
            declared: patch_ref_count,
        });
    }
    let mut offset = HEADER_LEN;
    let destinations = read_identity_column::<VId>(bytes, &mut offset, count as usize, 0)?;
    let edge_ids = read_identity_column::<EId>(bytes, &mut offset, count as usize, 1)?;
    let patch_section = if patch_ref_count == 1 {
        32usize + count as usize
    } else {
        0
    };
    let expected =
        offset
            .checked_add(span_count.checked_mul(VISIBILITY_SPAN_LEN).ok_or(
                BlockError::Truncated {
                    expected: usize::MAX,
                    found: bytes.len(),
                },
            )?)
            .and_then(|sum| sum.checked_add(patch_section))
            .ok_or(BlockError::Truncated {
                expected: usize::MAX,
                found: bytes.len(),
            })?;
    if bytes.len() < expected {
        return Err(BlockError::Truncated {
            expected,
            found: bytes.len(),
        });
    }
    if bytes.len() > expected {
        return Err(BlockError::TrailingBytes {
            extra: bytes.len() - expected,
        });
    }
    let mut spans = Vec::with_capacity(span_count);
    for _ in 0..span_count {
        spans.push(VisibilityInterval {
            start_row: u32::from_be_bytes(
                bytes[offset..offset + 4].try_into().expect("bounded span"),
            ),
            end_row: u32::from_be_bytes(
                bytes[offset + 4..offset + 8]
                    .try_into()
                    .expect("bounded span"),
            ),
            created_at: CommitSeq(u64::from_be_bytes(
                bytes[offset + 8..offset + 16]
                    .try_into()
                    .expect("bounded span"),
            )),
            retired_at: {
                let retired = u64::from_be_bytes(
                    bytes[offset + 16..offset + 24]
                        .try_into()
                        .expect("bounded span"),
                );
                (retired != 0).then_some(CommitSeq(retired))
            },
        });
        offset += VISIBILITY_SPAN_LEN;
    }
    validate_spans(&spans, count as usize)?;
    let patch = if patch_ref_count == 1 {
        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes[offset..offset + 32]);
        let locators = bytes[offset + 32..offset + 32 + count as usize].to_vec();
        // The locator column's own law holds without the patch in hand; the
        // joint row-count half is admission's, with both objects resolved.
        edge_props::validate_locator_sequence(&locators).map_err(|error| match error {
            edge_props::EdgePropertyPatchError::LocatorBijectionViolation { entry_at, .. } => {
                BlockError::NonCanonicalLocators { at: entry_at }
            }
            _ => BlockError::NonCanonicalLocators { at: 0 },
        })?;
        Some((ObjectId(id), locators))
    } else {
        None
    };
    Ok(DecodedFrame {
        descriptor,
        destinations,
        edge_ids,
        spans,
        patch,
    })
}

fn read_identity_column<T: ElementIdentity>(
    bytes: &[u8],
    offset: &mut usize,
    rows: usize,
    column: u8,
) -> Result<IdentityColumn<T>, BlockError> {
    let frame_end = offset
        .checked_add(COLUMN_FRAME_LEN)
        .ok_or(BlockError::Truncated {
            expected: usize::MAX,
            found: bytes.len(),
        })?;
    if frame_end > bytes.len() {
        return Err(BlockError::Truncated {
            expected: frame_end,
            found: bytes.len(),
        });
    }
    let codec_id = u16::from_be_bytes(
        bytes[*offset..*offset + 2]
            .try_into()
            .expect("bounded column"),
    );
    if codec_id != IDENTITY_COLUMN_CODEC_ID {
        return Err(BlockError::UnsupportedIdentityCodec { column, codec_id });
    }
    let representation = representation_from_tag(bytes[*offset + 2])
        .ok_or(BlockError::InvalidVisibilityInterval { at: *offset + 2 })?;
    let prefixes = u32::from_be_bytes(
        bytes[*offset + 3..*offset + 7]
            .try_into()
            .expect("bounded column"),
    ) as usize;
    let payload_len = u32::from_be_bytes(
        bytes[*offset + 7..*offset + 11]
            .try_into()
            .expect("bounded column"),
    ) as usize;
    *offset = frame_end;
    let payload_end = offset
        .checked_add(payload_len)
        .ok_or(BlockError::Truncated {
            expected: usize::MAX,
            found: bytes.len(),
        })?;
    if payload_end > bytes.len() {
        return Err(BlockError::Truncated {
            expected: payload_end,
            found: bytes.len(),
        });
    }
    let limits =
        IdentityColumnLimits::new(rows, MAX_BLOCK_ENTRIES as usize, MAX_IDENTITY_COLUMN_BYTES);
    let column_value = IdentityColumn::try_from_scalar_payload(
        &bytes[*offset..payload_end],
        IdentityColumnDescriptor::new(representation, rows, prefixes),
        limits,
    )
    .map_err(|error| BlockError::IdentityColumn { column, error })?;
    *offset = payload_end;
    Ok(column_value)
}

fn validate_spans(spans: &[VisibilityInterval], rows: usize) -> Result<(), BlockError> {
    let mut next = 0u32;
    for (at, span) in spans.iter().enumerate() {
        if span.start_row != next || span.end_row <= span.start_row || span.end_row as usize > rows
        {
            return Err(BlockError::InvalidVisibilityInterval { at });
        }
        if span.created_at.0 == 0
            || span
                .retired_at
                .is_some_and(|retired| retired <= span.created_at)
        {
            return Err(BlockError::InvalidVisibilityInterval { at });
        }
        next = span.end_row;
    }
    if next as usize != rows {
        return Err(BlockError::InvalidVisibilityInterval { at: spans.len() });
    }
    Ok(())
}

fn read_entry(frame: &DecodedFrame, index: usize) -> AdjacencyEntry {
    let span = frame
        .spans
        .iter()
        .find(|span| span.start_row as usize <= index && index < span.end_row as usize)
        .expect("validated spans cover every framed row");
    AdjacencyEntry {
        src: frame.descriptor.src,
        relation: frame.descriptor.relation,
        dst: frame
            .destinations
            .get(index)
            .expect("codec descriptor frames exact row count"),
        eid: frame
            .edge_ids
            .get(index)
            .expect("codec descriptor frames exact row count"),
        created_at: span.created_at,
        retired_at: span.retired_at,
    }
}

/// Decode a whole block, enforcing every law the encoder does.
///
/// The order and interval checks are re-run here rather than trusted from the
/// encoder, because a block read from disk was not necessarily written by this
/// process — and a decoder that trusts its input is not a decoder. That is the
/// same reason the chain's `verify` replays through `validate`: one law, checked
/// wherever a value can enter.
pub fn decode_block(bytes: &[u8]) -> Result<Vec<AdjacencyEntry>, BlockError> {
    decode_block_with_properties(bytes).map(|(entries, _)| entries)
}

/// [`decode_block`], keeping the hosted property-patch reference and locator
/// column beside the entries (fgdb-yqor). The full format validation runs on
/// both faces — this one merely does not discard what the block declared.
#[allow(clippy::type_complexity)]
pub fn decode_block_with_properties(
    bytes: &[u8],
) -> Result<(Vec<AdjacencyEntry>, Option<(ObjectId, Vec<u8>)>), BlockError> {
    let frame = read_header(bytes)?;
    let count = frame.destinations.len();
    let mut out: Vec<AdjacencyEntry> = Vec::with_capacity(count);
    for index in 0..count {
        let entry = read_entry(&frame, index);
        validate_entry(index, &entry)?;
        if let Some(previous) = out.last()
            && (previous.dst, previous.eid) >= (entry.dst, entry.eid)
        {
            return Err(BlockError::NonCanonicalOrder { at: index });
        }
        out.push(entry);
    }
    Ok((out, frame.patch))
}

/// The neighbours of `src` over `relation` visible at `as_of`, ascending.
///
/// **Reads from the ENCODED bytes.** It walks entries in place and materializes
/// only the destinations it returns, rather than decoding the block and filtering
/// — which is the whole point of having a format: a scan for one adjacency must
/// not cost the whole block. The layout supports it because entries are sorted by
/// `(src, relation, dst, eid)`, so one adjacency's entries are contiguous.
///
/// Still validates every entry it reads. A scan is a read path, and a read path
/// that skipped the checks would be a second, weaker decoder — exactly the
/// verify-narrower-than-validate shape that has bitten this workspace twice.
pub fn scan_neighbours(
    bytes: &[u8],
    src: VId,
    relation: RelationId,
    as_of: CommitSeq,
) -> Result<Vec<VId>, BlockError> {
    let frame = read_header(bytes)?;
    if frame.descriptor.src != src || frame.descriptor.relation != relation {
        return Ok(Vec::new());
    }
    let count = frame.destinations.len();
    let mut out = Vec::new();
    let mut previous: Option<(VId, EId)> = None;
    for index in 0..count {
        let entry = read_entry(&frame, index);
        validate_entry(index, &entry)?;
        if let Some(prev) = previous
            && prev >= (entry.dst, entry.eid)
        {
            return Err(BlockError::NonCanonicalOrder { at: index });
        }
        previous = Some((entry.dst, entry.eid));

        if entry.visible_at(as_of) {
            // Neighbour semantics are set-valued: parallel EIds prove distinct
            // edges without repeating their common destination in the answer.
            if out.last() != Some(&entry.dst) {
                out.push(entry.dst);
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Block identity
// ---------------------------------------------------------------------------

/// The content identity of an encoded block.
///
/// **DERIVED, NEVER ACCEPTED**, which is the rule every durable object in this
/// codebase follows: a caller cannot name one block and store another. It is
/// §5.1's keyed `logical_object_id` over the block's canonical bytes, the same
/// function Chronicle's capsules use — not a private hash — so a block is a
/// logical object in the same sense everything else is, scoped to its database's
/// key, security namespace, and durable object kind rather than globally
/// guessable.
///
/// The identity is over the CANONICAL bytes, so it inherits canonicality: two
/// encoders that disagree about order produce different bytes and therefore
/// different identities, and there is no way to have one identity name two
/// contents.
///
/// SUBSET NOTE (doctrine 7): this is the ObjectId step of §5.1 and only that step.
/// A block is not sealed, so it has no CiphertextId, no EncodingId, and no
/// erasure coding — sealing it into a capsule is a later slice, and the pipeline's
/// remaining stages arrive with it rather than being approximated here.
pub fn block_id(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
) -> ObjectId {
    // The canonical header binds the durable kind exactly as
    // `IdentifiedObject::new` does for a capsule's plaintext. The payload magic
    // distinguishes block *format*; it is not a substitute for namespacing the
    // ObjectId by durable object kind.
    ObjectId(
        fgdb_crypto::logical_object_id(
            k_oid,
            &namespace.0,
            &DELTA_BLOCK_OBJECT_KIND.to_le_bytes(),
            bytes,
        )
        .0,
    )
}

/// Decode a block that must be the one named by `expected`.
///
/// The identity is checked BEFORE the contents are interpreted, because the
/// question "are these the bytes I asked for" is not answerable from a decoded
/// value: a block that decodes cleanly and is the wrong block is exactly the
/// failure a content-addressed store exists to prevent, and it is silent.
///
/// This is the read path a partition root will use once one exists — a root names
/// a block by identity, and the reader must be able to prove the bytes it found
/// are that block rather than trusting the path it read them from.
pub fn read_block(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    expected: ObjectId,
) -> Result<Vec<AdjacencyEntry>, BlockError> {
    let actual = block_id(k_oid, namespace, bytes);
    if actual != expected {
        return Err(BlockError::IdentityMismatch { expected, actual });
    }
    decode_block(bytes)
}
